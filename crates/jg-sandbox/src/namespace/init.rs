//! Starting the detached init process of a sandbox.

use std::os::fd::{AsFd, OwnedFd};
use std::time::Duration;

use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, Pid, sethostname};

use crate::error::{Result, SandboxError};
use crate::namespace::bridge;
use crate::namespace::rootfs;
use crate::namespace::util;
use crate::namespace::{SANDBOX_GID, SANDBOX_UID};
use crate::spec::SandboxSpec;

const MAP_TIMEOUT: Duration = Duration::from_secs(10);
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Create the namespaces and start a detached init; returns its host pid.
pub(crate) fn spawn(spec: &SandboxSpec) -> Result<i32> {
    let (ready_r, ready_w) = util::pipe()?;
    let (go_r, go_w) = util::pipe()?;
    let (unshared_r, unshared_w) = util::pipe()?;
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();

    let fork_result = util::fork_process()?;
    match fork_result {
        ForkResult::Parent { child } => {
            drop(ready_w);
            drop(go_r);
            drop(unshared_w);
            let result = parent_handshake(child, ready_r, go_w, unshared_r, uid, gid);
            if result.is_err() {
                let _ = kill(child, Signal::SIGKILL);
                let _ = waitpid(child, None);
            }
            result
        }
        ForkResult::Child => {
            drop(ready_r);
            drop(go_w);
            drop(unshared_r);
            child_stage(spec, ready_w, go_r, unshared_w)
        }
    }
}

fn parent_handshake(
    child: Pid,
    ready: OwnedFd,
    go: OwnedFd,
    unshared: OwnedFd,
    uid: u32,
    gid: u32,
) -> Result<i32> {
    let signal = util::read_byte(unshared.as_fd(), MAP_TIMEOUT)?;
    drop(unshared);
    if signal.is_none() {
        let reason = util::read_line(ready.as_fd(), Duration::from_secs(1))?
            .and_then(|line| line.strip_prefix("ERR ").map(str::to_string))
            .unwrap_or_else(|| "user namespace setup failed".to_string());
        return Err(SandboxError::Unsupported { reason });
    }

    write_id_maps(child.as_raw(), uid, gid)?;
    util::write_all(go.as_fd(), b"g")?;
    drop(go);

    let Some(pid_line) = util::read_line(ready.as_fd(), READY_TIMEOUT)? else {
        return Err(SandboxError::InitFailed {
            reason: "init did not report its pid".to_string(),
        });
    };
    let pid = parse_pid_line(&pid_line)?;

    let result_line = match util::read_line(ready.as_fd(), READY_TIMEOUT) {
        Ok(Some(line)) => line,
        Ok(None) => {
            stop_init(pid);
            return Err(SandboxError::InitFailed {
                reason: "init did not report readiness".to_string(),
            });
        }
        Err(error) => {
            stop_init(pid);
            return Err(error);
        }
    };
    let _ = waitpid(child, None);

    if result_line == "OK" {
        return Ok(pid);
    }
    let reason = result_line
        .strip_prefix("ERR ")
        .unwrap_or("init failed")
        .to_string();
    stop_init(pid);
    Err(SandboxError::InitFailed { reason })
}

fn stop_init(pid: i32) {
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    let _ = util::wait_dead(pid, Duration::from_secs(5));
}

fn write_id_maps(pid: i32, uid: u32, gid: u32) -> Result<()> {
    let uid_map = format!("/proc/{pid}/uid_map");
    std::fs::write(&uid_map, format!("{SANDBOX_UID} {uid} 1\n")).map_err(|source| {
        SandboxError::Unsupported {
            reason: format!("cannot write {uid_map}: {source}"),
        }
    })?;

    let setgroups = format!("/proc/{pid}/setgroups");
    match std::fs::write(&setgroups, "deny") {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(SandboxError::Unsupported {
                reason: format!("cannot write {setgroups}: {source}"),
            });
        }
    }

    let gid_map = format!("/proc/{pid}/gid_map");
    std::fs::write(&gid_map, format!("{SANDBOX_GID} {gid} 1\n")).map_err(|source| {
        SandboxError::Unsupported {
            reason: format!("cannot write {gid_map}: {source}"),
        }
    })?;
    Ok(())
}

fn parse_pid_line(line: &str) -> Result<i32> {
    let pid = line
        .strip_prefix("PID ")
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 1);
    pid.ok_or_else(|| SandboxError::InitFailed {
        reason: format!("invalid init pid line {line:?}"),
    })
}

fn child_stage(spec: &SandboxSpec, ready: OwnedFd, go: OwnedFd, unshared: OwnedFd) -> ! {
    match child_setup(spec, &ready, go, unshared) {
        Ok(()) => util::exit_now(0),
        Err(error) => {
            let _ = util::write_all(ready.as_fd(), format!("ERR {error}\n").as_bytes());
            util::exit_now(1)
        }
    }
}

fn child_setup(spec: &SandboxSpec, ready: &OwnedFd, go: OwnedFd, unshared: OwnedFd) -> Result<()> {
    // A detached session keeps the init out of the creator's terminal and process group.
    let _ = nix::unistd::setsid();

    unshare(CloneFlags::CLONE_NEWUSER).map_err(|source| SandboxError::Unsupported {
        reason: format!("unprivileged user namespaces are unavailable: {source}"),
    })?;

    util::write_all(unshared.as_fd(), b"u")?;
    drop(unshared);

    let go_signal = util::read_byte(go.as_fd(), MAP_TIMEOUT)?;
    drop(go);
    if go_signal.is_none() {
        return Err(SandboxError::InitFailed {
            reason: "parent did not map the user namespace".to_string(),
        });
    }

    let flags = CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWNET;
    unshare(flags).map_err(|source| SandboxError::nix("unshare namespaces", source))?;
    sethostname(spec.hostname.clone())
        .map_err(|source| SandboxError::nix("sethostname", source))?;

    let fork_result = util::fork_process()?;
    if let ForkResult::Child = fork_result {
        init_process(spec, ready);
    }
    Ok(())
}

fn init_process(spec: &SandboxSpec, ready: &OwnedFd) -> ! {
    let pid = match util::host_pid() {
        Ok(pid) => pid,
        Err(error) => {
            let _ = util::write_all(ready.as_fd(), format!("ERR {error}\n").as_bytes());
            util::exit_now(1)
        }
    };
    let _ = util::write_all(ready.as_fd(), format!("PID {pid}\n").as_bytes());

    match setup(spec) {
        Ok(()) => {
            let _ = util::write_all(ready.as_fd(), b"OK\n");
            init_loop()
        }
        Err(error) => {
            let _ = util::write_all(ready.as_fd(), format!("ERR {error}\n").as_bytes());
            util::exit_now(1)
        }
    }
}

/// Build the sandbox world, wiring egress before readiness is reported.
///
/// Egress failures happen here, before `create` returns, so a sandbox that
/// asked for egress never comes up without its loopback bridge.
fn setup(spec: &SandboxSpec) -> Result<()> {
    rootfs::setup_rootfs(spec)?;
    if spec.egress.is_some() {
        bridge::bring_up_loopback()?;
    }
    util::drop_all_privileges()?;
    if let Some(egress) = &spec.egress {
        bridge::spawn(egress.port)?;
    }
    Ok(())
}

fn init_loop() -> ! {
    let _ = util::redirect_stdio_to_dev_null();
    util::close_fds_from(3);
    loop {
        let _ = waitpid(None, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
        util::idle_pause();
    }
}
