//! Lifecycle, exec and persistence tests for the sandbox.

mod support;

use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::{Duration, Instant};

use jg_sandbox::{Backend, ExecRequest, NamespaceBackend, SandboxError, SandboxSpec};
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use support::{
    assert_process_gone, create_or_skip, fixture, mount_namespace_inode, pids_in_mount_namespace,
    process_args, run_ok, run_shell, wait_until,
};

#[test]
fn create_exec_destroy_cycle() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    let output = run_shell(backend, handle, "echo out; echo err >&2; exit 3");
    assert_eq!(output.exit_code, Some(3), "unexpected exit code");
    assert_eq!(output.stdout_text(), "out\n");
    assert_eq!(output.stderr_text(), "err\n");

    let output = run_ok(backend, handle, "printf 'no newline'");
    assert_eq!(output.stdout_text(), "no newline");

    let init_pid = handle.init_pid();
    backend.destroy(handle).expect("first destroy");
    backend.destroy(handle).expect("destroy is idempotent");

    assert!(
        !lab.spec.state_dir.join("state.json").exists(),
        "state file left behind"
    );
    assert_process_gone(init_pid);
}

#[test]
fn files_persist_across_execs_and_attach() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    run_ok(backend, handle, "echo persisted > /lab/data.txt");
    let output = run_ok(backend, handle, "cat /lab/data.txt");
    assert_eq!(output.stdout_text(), "persisted\n");

    let attached = backend.attach(&lab.spec.state_dir).expect("attach");
    assert_eq!(attached.init_pid(), handle.init_pid());
    let output = run_ok(backend, &attached, "cat /lab/data.txt");
    assert_eq!(output.stdout_text(), "persisted\n");
}

#[test]
fn init_survives_the_creator_process() {
    let temp = tempfile::tempdir().expect("temp dir");
    let lab_dir = temp.path().join("lab");
    std::fs::create_dir_all(&lab_dir).expect("lab dir");
    let state_dir = temp.path().join("state");

    let status = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "helper_create_sandbox", "--nocapture"])
        .env("JGF_SANDBOX_HELPER_STATE", &state_dir)
        .env("JGF_SANDBOX_HELPER_LAB", &lab_dir)
        .status()
        .expect("run helper");
    assert!(status.success(), "helper process failed");

    if state_dir.join("unsupported.txt").exists() {
        eprintln!("skipping sandbox test: helper reported unsupported host");
        return;
    }

    let backend = NamespaceBackend::new();
    let handle = backend
        .attach(&state_dir)
        .expect("attach after creator exited");
    let init_pid = handle.init_pid();

    run_ok(backend, &handle, "echo from-attach > /lab/after.txt");
    let output = run_ok(backend, &handle, "cat /lab/after.txt");
    assert_eq!(output.stdout_text(), "from-attach\n");

    backend.destroy(&handle).expect("destroy");
    assert_process_gone(init_pid);
}

#[test]
fn inherited_host_fds_are_closed_before_exec() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    // A directory fd without O_CLOEXEC, as a caller or a library may leave behind.
    let host_fd = open("/", OFlag::O_RDONLY | OFlag::O_DIRECTORY, Mode::empty())
        .expect("open host directory");
    let host_fd_number = host_fd.as_raw_fd();

    let script = concat!(
        "fd=0\n",
        "while [ \"$fd\" -lt 64 ]; do\n",
        "  if [ -e \"/proc/self/fd/$fd\" ]; then echo \"FD $fd\"; fi\n",
        "  fd=$((fd + 1))\n",
        "done\n",
    );
    let output = run_ok(backend, handle, script);
    let text = output.stdout_text();
    let listed: Vec<&str> = text.lines().collect();
    assert_eq!(
        listed,
        vec!["FD 0", "FD 1", "FD 2"],
        "only stdio may survive exec, found {listed:?}"
    );
    assert!(
        !text.contains(&format!("FD {host_fd_number}")),
        "inherited host fd {host_fd_number} reached the command"
    );
}

#[test]
fn command_runs_in_its_own_session_without_a_terminal() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    let script = concat!(
        "read pid comm state ppid pgrp sid tty rest < /proc/$$/stat\n",
        "echo \"PID=$pid SID=$sid TTY=$tty\"\n",
        "if [ -e /dev/tty ]; then echo TTY-EXISTS; else echo NO-TTY; fi\n",
    );
    let output = run_ok(backend, handle, script);
    let text = output.stdout_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "unexpected output: {lines:?}");

    let mut fields = lines[0].split_whitespace();
    let pid: i32 = fields
        .next()
        .and_then(|field| field.strip_prefix("PID="))
        .and_then(|value| value.parse().ok())
        .expect("pid field");
    let sid: i32 = fields
        .next()
        .and_then(|field| field.strip_prefix("SID="))
        .and_then(|value| value.parse().ok())
        .expect("session field");
    let tty: i32 = fields
        .next()
        .and_then(|field| field.strip_prefix("TTY="))
        .and_then(|value| value.parse().ok())
        .expect("tty field");

    assert_eq!(sid, pid, "the command is not a session leader");
    assert_eq!(tty, 0, "the command has a controlling terminal");
    assert_eq!(lines[1], "NO-TTY", "/dev/tty exists inside the sandbox");
}

#[test]
fn background_process_does_not_outlive_the_command() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let mount_ns = mount_namespace_inode(handle.init_pid());

    let started = Instant::now();
    let output = run_ok(backend, handle, "setsid sleep 987654 & echo backgrounded");
    let elapsed = started.elapsed();
    assert_eq!(output.stdout_text(), "backgrounded\n");
    assert!(
        elapsed < Duration::from_secs(5),
        "exec waited for the background process: {elapsed:?}"
    );
    wait_until("the background sleep exits", || {
        !sandbox_has_arg(mount_ns, "987654")
    });
}

#[test]
fn timeout_kills_setsid_descendants() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let mount_ns = mount_namespace_inode(handle.init_pid());

    let request = ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "setsid sleep 987655 & sleep 60".to_string(),
    ])
    .with_timeout(Duration::from_secs(1));
    let started = Instant::now();
    let output = backend.exec(handle, &request).expect("exec");
    let elapsed = started.elapsed();
    assert!(output.timed_out, "the timeout was not reported");
    assert!(
        elapsed < Duration::from_secs(5),
        "exec returned {elapsed:?} after a one second timeout"
    );
    wait_until("the setsid sleep exits", || {
        !sandbox_has_arg(mount_ns, "987655")
    });
}

#[test]
fn command_cannot_kill_or_stop_its_supervisor() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let mount_ns = mount_namespace_inode(handle.init_pid());

    // `$PPID` is PID 1 of the exec's private PID namespace; SIGKILL and
    // SIGSTOP from inside that namespace must not reach the supervisor.
    for (signal, marker) in [("KILL", "987656"), ("STOP", "987657")] {
        let script = format!("setsid sleep {marker} & kill -{signal} $PPID; exit 7");
        let request = ExecRequest::new(vec!["/bin/sh".to_string(), "-c".to_string(), script])
            .with_timeout(Duration::from_secs(10));
        let started = Instant::now();
        let output = backend.exec(handle, &request).expect("exec");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "exec took {elapsed:?} after kill -{signal} of $PPID"
        );
        assert!(
            !output.timed_out,
            "kill -{signal} of $PPID was reported as a timeout"
        );
        assert_eq!(
            output.exit_code,
            Some(7),
            "the command's own exit status was lost after kill -{signal}: {output:?}"
        );
        wait_until("the background sleep exits", || {
            !sandbox_has_arg(mount_ns, marker)
        });
    }
}

#[test]
fn timeout_tears_down_the_nested_pid_namespace() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let mount_ns = mount_namespace_inode(handle.init_pid());

    let request = ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "setsid sleep 987658 & sleep 60".to_string(),
    ])
    .with_timeout(Duration::from_secs(1));
    let started = Instant::now();
    let output = backend.exec(handle, &request).expect("exec");
    let elapsed = started.elapsed();
    assert!(output.timed_out, "the timeout was not reported");
    assert!(
        elapsed < Duration::from_secs(5),
        "exec returned {elapsed:?} after a one second timeout"
    );
    assert_ne!(
        output.exit_code,
        Some(0),
        "a timed-out command was reported as a success"
    );
    wait_until("the setsid sleep exits", || {
        !sandbox_has_arg(mount_ns, "987658")
    });
}

fn sandbox_has_arg(mount_ns: u64, marker: &str) -> bool {
    pids_in_mount_namespace(mount_ns)
        .into_iter()
        .any(|pid| process_args(pid).iter().any(|arg| arg == marker))
}

#[test]
fn helper_create_sandbox() {
    let Ok(state_dir) = std::env::var("JGF_SANDBOX_HELPER_STATE") else {
        return;
    };
    let Ok(lab_dir) = std::env::var("JGF_SANDBOX_HELPER_LAB") else {
        return;
    };
    let spec = SandboxSpec::new(state_dir, lab_dir);
    let backend = NamespaceBackend::new();
    match backend.create(&spec) {
        Ok(_) => {}
        Err(SandboxError::Unsupported { reason }) => {
            let _ = std::fs::create_dir_all(&spec.state_dir);
            let _ = std::fs::write(spec.state_dir.join("unsupported.txt"), reason);
        }
        Err(error) => panic!("helper create failed: {error}"),
    }
}
