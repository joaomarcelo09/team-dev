//! Lab-level cgroup v2 limit tests.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jg_sandbox::{Backend, ExecRequest, LabLimits, NamespaceBackend, SandboxError};
use support::{create_or_skip, ensure_delegated, fixture, run_ok, run_shell};

fn limits(cpu_percent: Option<u32>, memory_bytes: Option<u64>, pids: Option<u64>) -> LabLimits {
    LabLimits {
        cpu_percent,
        memory_bytes,
        pids,
    }
}

fn read_trim(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim()
        .to_string()
}

fn cgroup_path(handle: &jg_sandbox::SandboxHandle) -> PathBuf {
    handle
        .cgroup_path()
        .unwrap_or_else(|| panic!("sandbox has no cgroup path"))
        .to_path_buf()
}

#[test]
fn limit_files_hold_the_requested_values() {
    if !ensure_delegated("limit_files_hold_the_requested_values") {
        return;
    }
    let mut lab = fixture();
    lab.spec.limits = Some(limits(Some(50), Some(256 * 1024 * 1024), Some(64)));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };

    let path = cgroup_path(guard.handle());
    assert_eq!(read_trim(&path.join("cpu.max")), "50000 100000");
    assert_eq!(
        read_trim(&path.join("memory.max")),
        (256 * 1024 * 1024).to_string()
    );
    assert_eq!(read_trim(&path.join("memory.swap.max")), "0");
    assert_eq!(read_trim(&path.join("pids.max")), "64");
}

#[test]
fn memory_limit_oom_kills_a_hog() {
    if !ensure_delegated("memory_limit_oom_kills_a_hog") {
        return;
    }
    let mut lab = fixture();
    lab.spec.limits = Some(limits(None, Some(64 * 1024 * 1024), None));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };
    let backend = NamespaceBackend::new();
    run_ok(
        backend,
        guard.handle(),
        "command -v python3 >/dev/null || exit 2",
    );

    let request = ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "python3 -c 'x = bytearray(256 * 1024 * 1024)'".to_string(),
    ])
    .with_timeout(Duration::from_secs(20));
    // The OOM killer may take the command itself or a helper; either way the
    // exec must not report success.
    if let Ok(output) = backend.exec(guard.handle(), &request) {
        assert_ne!(output.exit_code, Some(0), "hog was not OOM-killed");
    }

    let path = cgroup_path(guard.handle());
    let events = read_trim(&path.join("memory.events"));
    let oom_kills = events
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill "))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    assert!(oom_kills > 0, "no OOM kill recorded: {events}");
}

#[test]
fn pids_limit_caps_a_fork_loop() {
    if !ensure_delegated("pids_limit_caps_a_fork_loop") {
        return;
    }
    const FORK_SCRIPT: &str = r#"python3 -c '
import os, time
count = 0
for _ in range(500):
    try:
        pid = os.fork()
    except OSError:
        break
    if pid == 0:
        time.sleep(60)
        os._exit(0)
    count += 1
print(count)
'"#;

    let pids = 32;
    let mut lab = fixture();
    lab.spec.limits = Some(limits(None, None, Some(pids)));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };
    let backend = NamespaceBackend::new();
    run_ok(
        backend,
        guard.handle(),
        "command -v python3 >/dev/null || exit 2",
    );

    let output = run_shell(backend, guard.handle(), FORK_SCRIPT);
    let count: u64 = output
        .stdout_text()
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("unexpected fork loop output {:?}", output.stdout_text()));
    assert!(count > 0, "fork loop could not fork at all");
    assert!(count < 500, "fork loop was not capped");
    assert!(count <= pids, "fork count {count} exceeds pids.max {pids}");
}

#[test]
fn verify_fails_when_the_init_leaves_its_cgroup() {
    if !ensure_delegated("verify_fails_when_the_init_leaves_its_cgroup") {
        return;
    }
    let mut lab = fixture();
    lab.spec.limits = Some(limits(None, None, Some(64)));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };
    let backend = NamespaceBackend::new();
    assert!(backend.verify(guard.handle()).is_ok());

    let path = cgroup_path(guard.handle());
    let sibling = move_init_out(guard.handle().init_pid(), &path);

    let verified = backend.verify(guard.handle());
    assert!(
        matches!(verified, Err(SandboxError::NotVerified { .. })),
        "verify should fail after the init left its cgroup: {verified:?}"
    );

    drop(guard);
    remove_cgroup_dir(&sibling);
}

#[test]
fn destroy_removes_the_cgroup_directory() {
    if !ensure_delegated("destroy_removes_the_cgroup_directory") {
        return;
    }
    let mut lab = fixture();
    lab.spec.limits = Some(limits(None, Some(128 * 1024 * 1024), Some(64)));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };
    let path = cgroup_path(guard.handle());
    assert!(path.is_dir());

    drop(guard);
    assert!(!path.exists(), "cgroup directory survived destroy");
}

#[test]
fn unusable_cgroup_parent_fails_and_leaves_nothing() {
    if !ensure_delegated("unusable_cgroup_parent_fails_and_leaves_nothing") {
        return;
    }
    let mut lab = fixture();
    let unusable = lab.temp.path().join("not-a-cgroup");
    lab.spec.limits = Some(limits(None, Some(64 * 1024 * 1024), None));
    lab.spec.cgroup_parent = Some(unusable.clone());

    let error = NamespaceBackend::new()
        .create(&lab.spec)
        .expect_err("create must fail closed");
    assert!(
        matches!(error, SandboxError::InvalidSpec { .. }),
        "unexpected error: {error}"
    );
    assert!(!lab.spec.state_dir.exists(), "state dir was left behind");
    assert!(
        !unusable.exists(),
        "a cgroup was created under a bad parent"
    );
}

/// Put `pid` in a fresh sibling of the sandbox cgroup, outside the limits.
fn move_init_out(pid: i32, sandbox_cgroup: &Path) -> PathBuf {
    let parent = sandbox_cgroup
        .parent()
        .expect("sandbox cgroup has a parent");
    let sibling = parent.join(format!("jg-sandbox-test-move-{pid}"));
    fs::create_dir(&sibling).unwrap_or_else(|error| {
        panic!("create {}: {error}", sibling.display());
    });
    fs::write(sibling.join("cgroup.procs"), format!("{pid}\n")).unwrap_or_else(|error| {
        panic!("move pid {pid} to {}: {error}", sibling.display());
    });
    sibling
}

fn remove_cgroup_dir(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match fs::remove_dir(path) {
            Ok(()) => return,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("remove {}: {error}", path.display()),
        }
    }
}
