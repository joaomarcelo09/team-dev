//! Integration tests for the host-side egress proxy.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use jg_sandbox::{EgressEvent, EgressPolicy, EgressProxy};

/// A public-looking address used by the injected resolver.
const VETTED_PUBLIC: &str = "93.184.216.34";

fn vetted_public() -> IpAddr {
    VETTED_PUBLIC.parse().expect("valid public address")
}

#[derive(Clone, Default)]
struct AuditLog {
    events: Arc<Mutex<Vec<EgressEvent>>>,
}

impl AuditLog {
    fn hook(&self) -> Arc<dyn Fn(EgressEvent) + Send + Sync> {
        let events = Arc::clone(&self.events);
        Arc::new(move |event| events.lock().expect("audit lock").push(event))
    }

    fn snapshot(&self) -> Vec<EgressEvent> {
        self.events.lock().expect("audit lock").clone()
    }
}

fn open(socket_path: &Path) -> UnixStream {
    let stream = UnixStream::connect(socket_path).expect("connect to the proxy");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("write timeout");
    stream
}

fn send_request(stream: &mut UnixStream, request: &[u8]) {
    // An oversized request may be refused while it is still being written.
    let _ = stream.write_all(request);
}

fn read_status(stream: &mut UnixStream) -> (u16, String) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 512];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).expect("read response");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    (status, text)
}

fn spawn_echo_target() -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind target");
    let address = listener.local_addr().expect("target address");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept target connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("target read timeout");
        let mut payload = [0u8; 4];
        stream.read_exact(&mut payload).expect("read tunnel bytes");
        stream.write_all(&payload).expect("echo tunnel bytes");
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
    (address, handle)
}

#[test]
fn allowed_connect_relays_bytes_both_ways() {
    let temp = tempfile::tempdir().expect("temp dir");
    let socket_path = temp.path().join("egress.sock");
    let policy = EgressPolicy::new(["allowed.test"]).expect("valid policy");
    let audit = AuditLog::default();
    let (target, target_thread) = spawn_echo_target();

    let resolver =
        move |_host: &str, _port: u16| -> io::Result<Vec<IpAddr>> { Ok(vec![vetted_public()]) };
    let connector = move |_address: SocketAddr, timeout: Duration| -> io::Result<TcpStream> {
        TcpStream::connect_timeout(&target, timeout)
    };
    let mut proxy =
        EgressProxy::start_with(&socket_path, policy, audit.hook(), resolver, connector)
            .expect("start proxy");

    let metadata = std::fs::metadata(&socket_path).expect("socket metadata");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

    let mut client = open(&socket_path);
    send_request(
        &mut client,
        b"CONNECT allowed.test:443 HTTP/1.1\r\nHost: allowed.test:443\r\n\r\n",
    );
    let (status, _) = read_status(&mut client);
    assert_eq!(status, 200);

    client.write_all(b"ping").expect("write through the tunnel");
    let mut echoed = [0u8; 4];
    client
        .read_exact(&mut echoed)
        .expect("read through the tunnel");
    assert_eq!(&echoed, b"ping");
    drop(client);
    target_thread.join().expect("target thread");

    let events = audit.snapshot();
    assert!(
        events
            .iter()
            .any(|event| event.host == "allowed.test" && event.port == 443 && event.allowed),
        "missing allowed audit event: {events:?}"
    );

    proxy.stop();
    assert!(!socket_path.exists());
}

#[test]
fn disallowed_host_is_refused_before_resolution() {
    let temp = tempfile::tempdir().expect("temp dir");
    let socket_path = temp.path().join("egress.sock");
    let policy = EgressPolicy::new(["allowed.test"]).expect("valid policy");
    let audit = AuditLog::default();

    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&resolver_calls);
    let resolver = move |_host: &str, _port: u16| -> io::Result<Vec<IpAddr>> {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![vetted_public()])
    };
    let connector = |_address: SocketAddr, _timeout: Duration| -> io::Result<TcpStream> {
        Err(io::Error::other("connector must not run"))
    };
    let mut proxy =
        EgressProxy::start_with(&socket_path, policy, audit.hook(), resolver, connector)
            .expect("start proxy");

    let mut client = open(&socket_path);
    send_request(
        &mut client,
        b"CONNECT blocked.test:443 HTTP/1.1\r\nHost: blocked.test:443\r\n\r\n",
    );
    let (status, _) = read_status(&mut client);
    assert_eq!(status, 403);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);

    let events = audit.snapshot();
    assert!(
        events
            .iter()
            .any(|event| event.host == "blocked.test" && !event.allowed),
        "missing refused audit event: {events:?}"
    );
    proxy.stop();
}

#[test]
fn host_resolving_to_private_address_is_refused() {
    for resolved in [
        Ipv4Addr::LOCALHOST,
        Ipv4Addr::new(10, 1, 2, 3),
        Ipv4Addr::new(192, 168, 1, 8),
        Ipv4Addr::new(169, 254, 169, 254),
        Ipv4Addr::new(100, 64, 0, 1),
        Ipv4Addr::UNSPECIFIED,
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        let socket_path = temp.path().join("egress.sock");
        let policy = EgressPolicy::new(["internal.test"]).expect("valid policy");
        let audit = AuditLog::default();

        let resolved = IpAddr::V4(resolved);
        let resolver =
            move |_host: &str, _port: u16| -> io::Result<Vec<IpAddr>> { Ok(vec![resolved]) };
        let connected = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&connected);
        let connector = move |_address: SocketAddr, _timeout: Duration| -> io::Result<TcpStream> {
            flag.store(true, Ordering::SeqCst);
            Err(io::Error::other("connector must not run"))
        };
        let mut proxy =
            EgressProxy::start_with(&socket_path, policy, audit.hook(), resolver, connector)
                .expect("start proxy");

        let mut client = open(&socket_path);
        send_request(
            &mut client,
            b"CONNECT internal.test:443 HTTP/1.1\r\nHost: internal.test:443\r\n\r\n",
        );
        let (status, _) = read_status(&mut client);
        assert_eq!(status, 403, "{resolved} should be refused");
        assert!(
            !connected.load(Ordering::SeqCst),
            "{resolved} must not reach the connector"
        );
        proxy.stop();
    }
}

#[test]
fn ip_literal_hosts_are_refused() {
    for literal in ["93.184.216.34:443", "[::1]:443"] {
        let temp = tempfile::tempdir().expect("temp dir");
        let socket_path = temp.path().join("egress.sock");
        let policy = EgressPolicy::new(["allowed.test"]).expect("valid policy");
        let audit = AuditLog::default();

        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&resolver_calls);
        let resolver = move |_host: &str, _port: u16| -> io::Result<Vec<IpAddr>> {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![vetted_public()])
        };
        let connector = |_address: SocketAddr, _timeout: Duration| -> io::Result<TcpStream> {
            Err(io::Error::other("connector must not run"))
        };
        let mut proxy =
            EgressProxy::start_with(&socket_path, policy, audit.hook(), resolver, connector)
                .expect("start proxy");

        let mut client = open(&socket_path);
        let request = format!("CONNECT {literal} HTTP/1.1\r\nHost: {literal}\r\n\r\n");
        send_request(&mut client, request.as_bytes());
        let (status, _) = read_status(&mut client);
        assert_eq!(status, 403, "{literal} should be refused");
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
        proxy.stop();
    }
}

#[test]
fn oversized_and_garbled_requests_are_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let socket_path = temp.path().join("egress.sock");
    let policy = EgressPolicy::new(["allowed.test"]).expect("valid policy");
    let audit = AuditLog::default();

    let resolver =
        move |_host: &str, _port: u16| -> io::Result<Vec<IpAddr>> { Ok(vec![vetted_public()]) };
    let connector = |_address: SocketAddr, _timeout: Duration| -> io::Result<TcpStream> {
        Err(io::Error::other("connector must not run"))
    };
    let mut proxy =
        EgressProxy::start_with(&socket_path, policy, audit.hook(), resolver, connector)
            .expect("start proxy");

    let mut client = open(&socket_path);
    send_request(&mut client, &vec![b'A'; 9_000]);
    let (status, _) = read_status(&mut client);
    assert_eq!(status, 400, "oversized request should be refused");

    let mut client = open(&socket_path);
    send_request(&mut client, b"GET / HTTP/1.1\r\nHost: allowed.test\r\n\r\n");
    let (status, _) = read_status(&mut client);
    assert_eq!(status, 405, "non-CONNECT method should be refused");

    let mut client = open(&socket_path);
    send_request(&mut client, b"hello\r\n\r\n");
    let (status, _) = read_status(&mut client);
    assert_eq!(status, 400, "garbled request should be refused");

    let mut client = open(&socket_path);
    send_request(
        &mut client,
        b"CONNECT allowed.test HTTP/1.1\r\nHost: allowed.test\r\n\r\n",
    );
    let (status, _) = read_status(&mut client);
    assert_eq!(status, 400, "request without a port should be refused");

    proxy.stop();
}

#[test]
fn stop_removes_the_socket() {
    let temp = tempfile::tempdir().expect("temp dir");
    let socket_path = temp.path().join("egress.sock");
    let policy = EgressPolicy::new(["allowed.test"]).expect("valid policy");
    let audit = AuditLog::default();

    let resolver =
        move |_host: &str, _port: u16| -> io::Result<Vec<IpAddr>> { Ok(vec![vetted_public()]) };
    let connector = |_address: SocketAddr, _timeout: Duration| -> io::Result<TcpStream> {
        Err(io::Error::other("connector must not run"))
    };
    let mut proxy =
        EgressProxy::start_with(&socket_path, policy, audit.hook(), resolver, connector)
            .expect("start proxy");
    assert!(socket_path.exists());

    proxy.stop();
    assert!(!socket_path.exists());
    assert!(UnixStream::connect(&socket_path).is_err());
}

fn start_default_proxy(socket_path: &Path) -> bool {
    let policy = EgressPolicy::new(["allowed.test"]).expect("valid policy");
    let audit = AuditLog::default();
    let resolver =
        move |_host: &str, _port: u16| -> io::Result<Vec<IpAddr>> { Ok(vec![vetted_public()]) };
    let connector = |_address: SocketAddr, _timeout: Duration| -> io::Result<TcpStream> {
        Err(io::Error::other("connector must not run"))
    };
    EgressProxy::start_with(socket_path, policy, audit.hook(), resolver, connector).is_ok()
}

#[test]
fn refuses_to_replace_a_regular_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let socket_path = temp.path().join("egress.sock");
    std::fs::write(&socket_path, b"keep me").expect("write file");

    assert!(!start_default_proxy(&socket_path));
    assert_eq!(
        std::fs::read(&socket_path).expect("read file"),
        b"keep me",
        "the existing file must be untouched"
    );
}

#[test]
fn replaces_a_stale_socket() {
    let temp = tempfile::tempdir().expect("temp dir");
    let socket_path = temp.path().join("egress.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stale socket");
    drop(listener);
    assert!(socket_path.exists());

    assert!(start_default_proxy(&socket_path));
}
