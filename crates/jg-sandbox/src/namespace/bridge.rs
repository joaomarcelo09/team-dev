//! Loopback and TCP-to-unix bridge for host-controlled sandbox egress.
//!
//! The sandbox network namespace only has loopback. When egress is configured,
//! the init brings `lo` up and forks a small forwarder that listens on
//! `127.0.0.1:<port>` and relays each connection to the host egress proxy
//! socket mounted at [`EGRESS_SOCKET_PATH`].

use std::io;
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use nix::unistd::ForkResult;

use crate::error::{Result, SandboxError};
use crate::namespace::EGRESS_SOCKET_PATH;
use crate::namespace::util;

/// Connections served at the same time before new ones are refused.
const MAX_CONNECTIONS: usize = 64;
/// How long a silent tunnelled connection stays open.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Delay before retrying an accept that failed for a transient reason.
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

/// Bring the sandbox loopback interface up so `127.0.0.1` is usable.
pub(crate) fn bring_up_loopback() -> Result<()> {
    // SAFETY: the socket call only creates a descriptor owned below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(SandboxError::nix(
            "socket for loopback setup",
            nix::errno::Errno::last(),
        ));
    }
    let result = set_loopback_up(fd);
    // SAFETY: `fd` was created above and is not used after this point.
    unsafe { libc::close(fd) };
    result
}

fn set_loopback_up(fd: libc::c_int) -> Result<()> {
    // SAFETY: `ifreq` is plain old data and zero is a valid initial value.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    let name = b"lo";
    for (slot, byte) in request.ifr_name.iter_mut().zip(name.iter()) {
        *slot = *byte as libc::c_char;
    }

    // SAFETY: `request` is a valid `ifreq` for the duration of the call.
    if unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut request) } < 0 {
        return Err(SandboxError::nix(
            "read loopback flags",
            nix::errno::Errno::last(),
        ));
    }
    // SAFETY: SIOCGIFFLAGS filled the flags member of the union.
    let flags = unsafe { request.ifr_ifru.ifru_flags };
    request.ifr_ifru.ifru_flags = flags | libc::IFF_UP as libc::c_short;
    // SAFETY: `request` is a valid `ifreq` for the duration of the call.
    if unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS, &request) } < 0 {
        return Err(SandboxError::nix(
            "bring loopback up",
            nix::errno::Errno::last(),
        ));
    }
    Ok(())
}

/// Bind the loopback bridge and fork the process that serves it.
///
/// The listener is bound before the fork, so a port or loopback problem fails
/// the sandbox init and `create` fails closed. The serving child dies with the
/// sandbox init because it lives in the same PID namespace.
pub(crate) fn spawn(port: u16) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|source| SandboxError::io(format!("{}:{port}", Ipv4Addr::LOCALHOST), source))?;

    match util::fork_process()? {
        ForkResult::Parent { .. } => Ok(()),
        ForkResult::Child => serve(listener),
    }
}

fn serve(listener: TcpListener) -> ! {
    let _ = util::redirect_stdio_to_dev_null();
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let client = match listener.accept() {
            Ok((client, _)) => client,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                thread::sleep(ACCEPT_RETRY);
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let guard = ActiveGuard(Arc::clone(&active));
        let spawned = thread::Builder::new()
            .name("jg-egress-bridge".to_string())
            .spawn(move || handle(client, guard));
        // On spawn failure the closure is dropped, which closes the client and
        // releases the connection slot through the guard.
        if spawned.is_err() {
            continue;
        }
    }
}

struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle(client: TcpStream, _guard: ActiveGuard) {
    let _ = client.set_read_timeout(Some(IO_TIMEOUT));
    let _ = client.set_write_timeout(Some(IO_TIMEOUT));
    let Ok(upstream) = UnixStream::connect(EGRESS_SOCKET_PATH) else {
        return;
    };
    let _ = upstream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = upstream.set_write_timeout(Some(IO_TIMEOUT));
    relay(client, upstream);
}

fn relay(client: TcpStream, upstream: UnixStream) {
    let (Ok(mut client_read), Ok(mut upstream_write)) = (client.try_clone(), upstream.try_clone())
    else {
        return;
    };
    let client_to_upstream = thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });

    let mut client_write = client;
    let mut upstream_read = upstream;
    let _ = io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Write);
    let _ = client_to_upstream.join();
}
