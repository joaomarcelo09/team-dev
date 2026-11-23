//! Small process, pipe and namespace helpers shared by the backend.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use nix::fcntl::{OFlag, open};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::Mode;
use nix::unistd::{close, read, write};

use crate::error::{Result, SandboxError};

/// One end pair of a close-on-exec pipe.
pub(crate) fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(|source| SandboxError::nix("pipe2", source))
}

/// Write every byte of `bytes` to `fd`.
pub(crate) fn write_all(fd: BorrowedFd<'_>, bytes: &[u8]) -> Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        match write(fd, &bytes[written..]) {
            Ok(0) => {
                return Err(SandboxError::ExecFailed {
                    reason: "short write to pipe".to_string(),
                });
            }
            Ok(count) => written += count,
            Err(nix::errno::Errno::EINTR) => {}
            Err(source) => return Err(SandboxError::nix("write", source)),
        }
    }
    Ok(())
}

/// Read one byte from `fd`, waiting at most `timeout`.
pub(crate) fn read_byte(fd: BorrowedFd<'_>, timeout: Duration) -> Result<Option<u8>> {
    let mut byte = [0u8; 1];
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(left) if !left.is_zero() => left,
            _ => return Ok(None),
        };
        let poll_timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
        let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
        match poll(&mut fds, poll_timeout) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(source) => return Err(SandboxError::nix("poll", source)),
        }
        match read(fd, &mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(byte[0])),
            Err(nix::errno::Errno::EINTR) => {}
            Err(source) => return Err(SandboxError::nix("read", source)),
        }
    }
}

/// Read one newline-terminated line, waiting at most `timeout`.
///
/// Returns `None` on EOF or timeout before a full line.
pub(crate) fn read_line(fd: BorrowedFd<'_>, timeout: Duration) -> Result<Option<String>> {
    let mut line = Vec::new();
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(left) if !left.is_zero() => left,
            _ => return Ok(None),
        };
        match read_byte(fd, remaining)? {
            Some(b'\n') => {
                let text = String::from_utf8(line).map_err(|_| SandboxError::InitFailed {
                    reason: "init sent non-UTF-8 text".to_string(),
                })?;
                return Ok(Some(text));
            }
            Some(byte) => line.push(byte),
            None => return Ok(None),
        }
    }
}

/// The `/proc/<pid>/ns/*` file descriptors needed to enter a sandbox.
pub(crate) struct NamespaceFds {
    /// User namespace.
    pub user: OwnedFd,
    /// Mount namespace.
    pub mount: OwnedFd,
    /// PID namespace.
    pub pid: OwnedFd,
    /// IPC namespace.
    pub ipc: OwnedFd,
    /// UTS namespace.
    pub uts: OwnedFd,
    /// Network namespace.
    pub net: OwnedFd,
}

impl NamespaceFds {
    /// Open every namespace of `pid`.
    pub(crate) fn open(pid: i32) -> Result<Self> {
        Ok(Self {
            user: open_ns(pid, "user")?,
            mount: open_ns(pid, "mnt")?,
            pid: open_ns(pid, "pid")?,
            ipc: open_ns(pid, "ipc")?,
            uts: open_ns(pid, "uts")?,
            net: open_ns(pid, "net")?,
        })
    }
}

fn open_ns(pid: i32, name: &str) -> Result<OwnedFd> {
    let path = format!("/proc/{pid}/ns/{name}");
    open(
        path.as_str(),
        OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|source| SandboxError::nix(format!("open {path}"), source))
}

/// Close every file descriptor from `first` upwards.
pub(crate) fn close_fds_from(first: i32) {
    close_range_or_loop(first as u32, u32::MAX);
}

/// Close every descriptor above stdio except `keep`.
///
/// The worker must only keep the stdio dups and its status pipe (which is
/// close-on-exec) until `execve`, never an fd inherited from the caller.
pub(crate) fn close_inherited_fds(keep: BorrowedFd<'_>) {
    let keep = keep.as_raw_fd() as u32;
    close_range_or_loop(3, keep.saturating_sub(1));
    close_range_or_loop(keep.saturating_add(1), u32::MAX);
}

/// Try the `close_range` system call and fall back to a loop when the kernel lacks it.
fn close_range_or_loop(first: u32, last: u32) {
    if first > last {
        return;
    }
    // SAFETY: `close_range` only affects descriptors owned by this process.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first as libc::c_long,
            last as libc::c_long,
            0 as libc::c_long,
        )
    };
    if result != 0 {
        close_fd_loop(first, last);
    }
}

/// Portable fallback: close descriptors one by one up to `RLIMIT_NOFILE`.
fn close_fd_loop(first: u32, last: u32) {
    let limit = fd_limit();
    for fd in first..=last.min(limit.saturating_sub(1)) {
        let _ = close(fd as i32);
    }
}

fn fd_limit() -> u32 {
    const MAX_FDS: u32 = 1 << 20;
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes a `rlimit` into the provided pointer.
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    if result == 0 {
        (limit.rlim_cur.min(MAX_FDS as u64)) as u32
    } else {
        MAX_FDS
    }
}

/// Remove every privilege a sandboxed command must not keep.
///
/// Inside the sandbox the command runs as uid 0 of the user namespace that owns
/// the mount namespace, so without this it could remount read-only binds or
/// create new mounts. Clears the no-new-privs flag, the ambient set, the
/// bounding set and the effective/permitted/inheritable sets.
pub(crate) fn drop_all_privileges() -> Result<()> {
    set_no_new_privileges()?;
    clear_ambient_capabilities()?;
    drop_bounding_capabilities()?;
    clear_capability_sets()
}

/// Become the reaper of orphaned descendants instead of the sandbox init.
pub(crate) fn set_child_subreaper() -> Result<()> {
    // SAFETY: `prctl` only receives scalar arguments here.
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_CHILD_SUBREAPER,
            1 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    check_prctl("set child subreaper", result)
}

/// Ask the kernel to kill this process when its parent dies.
pub(crate) fn die_with_parent() -> Result<()> {
    // SAFETY: `prctl` only receives scalar arguments here.
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    check_prctl("set parent death signal", result)
}

fn set_no_new_privileges() -> Result<()> {
    // SAFETY: `prctl` only receives scalar arguments here.
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_NO_NEW_PRIVS,
            1 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    check_prctl("set no_new_privs", result)
}

fn clear_ambient_capabilities() -> Result<()> {
    // SAFETY: `prctl` only receives scalar arguments here.
    let result = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    check_prctl("clear ambient capabilities", result)
}

fn drop_bounding_capabilities() -> Result<()> {
    for cap in 0..=cap_last_cap() {
        // SAFETY: `prctl` only receives scalar arguments here.
        let result = unsafe {
            libc::prctl(
                libc::PR_CAPBSET_DROP,
                cap as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        if result != 0 {
            let error = nix::errno::Errno::last();
            if error != nix::errno::Errno::EINVAL {
                return Err(SandboxError::nix(
                    format!("drop bounding capability {cap}"),
                    error,
                ));
            }
        }
    }
    Ok(())
}

fn cap_last_cap() -> u32 {
    std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or(63)
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn clear_capability_sets() -> Result<()> {
    // `_LINUX_CAPABILITY_VERSION_3`; one `CapData` per 32 capability bits.
    const CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    let mut header = CapHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    // SAFETY: `capset` reads a header and an array of two 32-bit capability
    // sets, which is the layout required by `CAPABILITY_VERSION_3`.
    let result = unsafe {
        libc::syscall(
            libc::SYS_capset,
            &mut header as *mut CapHeader,
            data.as_ptr(),
        )
    };
    if result != 0 {
        return Err(SandboxError::nix(
            "clear capability sets",
            nix::errno::Errno::last(),
        ));
    }
    Ok(())
}

fn check_prctl(context: &str, result: libc::c_int) -> Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(SandboxError::nix(context, nix::errno::Errno::last()))
    }
}

/// Point standard input, output and error at `/dev/null`.
pub(crate) fn redirect_stdio_to_dev_null() -> Result<()> {
    let fd = open("/dev/null", OFlag::O_RDWR | OFlag::O_CLOEXEC, Mode::empty())
        .map_err(|source| SandboxError::nix("open /dev/null", source))?;
    nix::unistd::dup2_stdin(fd.as_fd())
        .map_err(|source| SandboxError::nix("dup2 stdin", source))?;
    nix::unistd::dup2_stdout(fd.as_fd())
        .map_err(|source| SandboxError::nix("dup2 stdout", source))?;
    nix::unistd::dup2_stderr(fd.as_fd()).map_err(|source| SandboxError::nix("dup2 stderr", source))
}

/// The host (outermost) pid of the calling process.
pub(crate) fn host_pid() -> Result<i32> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|source| SandboxError::io("/proc/self/status", source))?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("NSpid:")
            && let Some(pid) = rest.split_whitespace().next()
            && let Ok(pid) = pid.parse::<i32>()
        {
            return Ok(pid);
        }
    }
    Ok(nix::unistd::getpid().as_raw())
}

/// Wait until `pid` is no longer alive, up to `timeout`.
pub(crate) fn wait_dead(pid: i32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while crate::state::is_alive(pid) {
        if Instant::now() >= deadline {
            return Err(SandboxError::StopTimeout {
                pid,
                seconds: timeout.as_secs(),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Fork the current process.
///
/// The child must only run async-signal-safe work or `exec`, because the parent
/// may be multithreaded.
pub(crate) fn fork_process() -> Result<nix::unistd::ForkResult> {
    // SAFETY: `fork` is the documented way to create a process; the caller keeps
    // the child single-threaded until it exits or execs.
    unsafe { nix::unistd::fork() }.map_err(|source| SandboxError::nix("fork", source))
}

/// Terminate the current process immediately without running destructors.
pub(crate) fn exit_now(code: i32) -> ! {
    // SAFETY: `_exit` is async-signal-safe and never returns.
    unsafe { libc::_exit(code) }
}

/// Sleep briefly, used by the persistent init.
pub(crate) fn idle_pause() {
    std::thread::sleep(Duration::from_millis(200));
}
