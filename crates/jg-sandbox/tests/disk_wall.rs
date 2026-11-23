//! Disk caps and wall-clock timeout tests.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jg_sandbox::{
    Backend, DiskLimits, ExecRequest, ExecScope, LabLimits, NamespaceBackend, SandboxError,
    SandboxHandle, SandboxSpec, ScopeAccess,
};
use support::{
    LabFixture, create_or_skip, ensure_delegated, fixture, run_ok, run_shell, wait_until,
};

const MB: u64 = 1024 * 1024;
const MIN_DISK_BYTES: u64 = MB;

fn shell_request(script: &str) -> ExecRequest {
    ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        script.to_string(),
    ])
}

fn scoped_request(worktree: &str, script: &str) -> ExecRequest {
    shell_request(script).with_scope(ExecScope::new(ScopeAccess::ReadWrite, worktree))
}

fn project_worktree(lab: &LabFixture, project: &str) -> PathBuf {
    let worktree = lab.spec.lab_dir.join("projects").join(project);
    fs::create_dir_all(&worktree).expect("project worktree");
    worktree
}

fn project_scope(project: &str) -> ExecScope {
    ExecScope::new(ScopeAccess::ReadWrite, format!("/lab/projects/{project}")).with_project(project)
}

fn project_cgroup(handle: &SandboxHandle, project: &str) -> PathBuf {
    handle
        .cgroup_path()
        .unwrap_or_else(|| panic!("sandbox has no cgroup path"))
        .join(format!("project-{project}"))
}

fn cgroup_pids(path: &Path) -> Vec<i32> {
    let text = match fs::read_to_string(path.join("cgroup.procs")) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn cmdline(pid: i32) -> String {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    String::from_utf8_lossy(&bytes).replace('\0', " ")
}

fn pid_with_marker(pids: &[i32], marker: &str) -> Option<i32> {
    pids.iter()
        .copied()
        .find(|pid| cmdline(*pid).contains(marker))
}

#[test]
fn lab_tmp_cap_fails_writes_with_enospc() {
    let mut lab = fixture();
    lab.spec.disk.tmp_bytes = MIN_DISK_BYTES;
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let output = run_shell(
        backend,
        guard.handle(),
        "dd if=/dev/zero of=/tmp/big bs=1M count=4",
    );
    assert_ne!(
        output.exit_code,
        Some(0),
        "a write past the lab /tmp cap succeeded"
    );
    assert!(
        output.stderr_text().contains("No space left on device"),
        "unexpected failure: {}",
        output.stderr_text()
    );
}

#[test]
fn scoped_tmp_cap_fails_writes_with_enospc() {
    let lab = fixture();
    fs::create_dir_all(lab.spec.lab_dir.join("proj-a")).expect("proj-a");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let scope =
        ExecScope::new(ScopeAccess::ReadWrite, "/lab/proj-a").with_tmp_bytes(MIN_DISK_BYTES);
    let request = shell_request("dd if=/dev/zero of=/tmp/big bs=1M count=4").with_scope(scope);
    let output = backend
        .exec(guard.handle(), &request)
        .expect("scoped exec runs");

    assert_ne!(
        output.exit_code,
        Some(0),
        "a write past the scoped /tmp cap succeeded"
    );
    assert!(
        output.stderr_text().contains("No space left on device"),
        "unexpected failure: {}",
        output.stderr_text()
    );
}

#[test]
fn defaults_keep_the_large_caps_when_nothing_is_set() {
    let spec = SandboxSpec::new("/tmp/x/state", "/tmp/x/lab");
    assert_eq!(spec.disk, DiskLimits::default());
    assert_eq!(spec.disk.root_bytes, 512 * MB);
    assert_eq!(spec.disk.tmp_bytes, 1024 * MB);

    let scope = ExecScope::new(ScopeAccess::ReadWrite, "/lab/a");
    assert_eq!(scope.tmp_bytes, None);
    assert_eq!(scope.effective_tmp_bytes(), 256 * MB);

    let lab = fixture();
    fs::create_dir_all(lab.spec.lab_dir.join("proj-a")).expect("proj-a");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    // Both defaults are far above the 1 MiB minimum, so 2 MiB fits everywhere.
    let options = "dd if=/dev/zero of=/tmp/big bs=1M count=2";
    let output = run_shell(backend, guard.handle(), options);
    assert_eq!(
        output.exit_code,
        Some(0),
        "the default lab /tmp cap shrank: {}",
        output.stderr_text()
    );

    let output = backend
        .exec(guard.handle(), &scoped_request("/lab/proj-a", options))
        .expect("scoped exec runs");
    assert_eq!(
        output.exit_code,
        Some(0),
        "the default scoped /tmp cap shrank: {}",
        output.stderr_text()
    );
}

#[test]
fn invalid_disk_caps_are_rejected() {
    let lab = fixture();
    let mut spec = lab.spec.clone();
    for bytes in [0, MIN_DISK_BYTES - 1] {
        spec.disk.root_bytes = bytes;
        let error = spec
            .validate()
            .expect_err("a tiny root_bytes must be rejected");
        assert!(
            error.to_string().contains("root_bytes"),
            "unexpected error: {error}"
        );
        spec.disk.root_bytes = 512 * MB;
        spec.disk.tmp_bytes = bytes;
        let error = spec
            .validate()
            .expect_err("a tiny tmp_bytes must be rejected");
        assert!(
            error.to_string().contains("tmp_bytes"),
            "unexpected error: {error}"
        );
        spec.disk.tmp_bytes = 1024 * MB;
    }

    let mut bad = fixture();
    bad.spec.disk.tmp_bytes = 0;
    assert!(
        NamespaceBackend::new().create(&bad.spec).is_err(),
        "a spec with a zero /tmp cap was created"
    );

    for tmp_bytes in [0, MIN_DISK_BYTES - 1] {
        let scope = ExecScope::new(ScopeAccess::ReadWrite, "/lab/a").with_tmp_bytes(tmp_bytes);
        assert!(
            scope.validate().is_err(),
            "tmp_bytes {tmp_bytes} must be rejected"
        );
    }

    // A scope above the lab cap is rejected before the command runs.
    let lab = fixture();
    let worktree = lab.spec.lab_dir.join("proj-a");
    fs::create_dir_all(&worktree).expect("proj-a");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let scope = ExecScope::new(ScopeAccess::ReadWrite, "/lab/proj-a").with_tmp_bytes(1024 * MB + 1);
    let request = shell_request("touch /workspace/ran").with_scope(scope);
    let error = backend
        .exec(guard.handle(), &request)
        .expect_err("a scope above the lab /tmp cap must be rejected");
    assert!(
        matches!(error, SandboxError::ExecFailed { .. }),
        "unexpected error: {error}"
    );
    assert!(
        !worktree.join("ran").exists(),
        "the command ran despite the rejection"
    );
}

#[test]
fn spec_json_without_disk_fields_still_deserializes() {
    let json = r#"{
        "state_dir": "/tmp/x/state",
        "lab_dir": "/tmp/x/lab",
        "ro_binds": [],
        "allowed_sockets": [],
        "hostname": "jg-sandbox"
    }"#;
    let spec: SandboxSpec = serde_json::from_str(json).expect("old state loads");
    assert_eq!(spec.disk, DiskLimits::default());
    assert_eq!(spec.disk.root_bytes, 512 * MB);
    assert_eq!(spec.disk.tmp_bytes, 1024 * MB);
}

#[test]
fn timed_out_project_exec_is_killed_while_a_sibling_keeps_running() {
    if !ensure_delegated("timed_out_project_exec_is_killed_while_a_sibling_keeps_running") {
        return;
    }
    let mut lab = fixture();
    project_worktree(&lab, "a");
    lab.spec.limits = Some(LabLimits {
        pids: Some(128),
        ..LabLimits::default()
    });
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let cgroup = project_cgroup(handle, "a");

    let sibling = ExecRequest::new(vec!["sleep".to_string(), "4242".to_string()])
        .with_timeout(Duration::from_secs(5))
        .with_scope(project_scope("a"));
    let timed = ExecRequest::new(vec!["sleep".to_string(), "4747".to_string()])
        .with_timeout(Duration::from_secs(1))
        .with_scope(project_scope("a"));

    std::thread::scope(|threads| {
        let sibling_thread = threads.spawn(|| backend.exec(handle, &sibling));
        wait_until("the sibling exec joins project-a", || {
            pid_with_marker(&cgroup_pids(&cgroup), "4242").is_some()
        });
        let before = cgroup_pids(&cgroup);

        let output = backend.exec(handle, &timed).expect("timed exec returns");
        assert!(output.timed_out, "the wall-clock limit did not fire");

        // Nothing new may stay behind: project-a only keeps the sibling's pids.
        wait_until("the timed-out exec leaves project-a", || {
            cgroup_pids(&cgroup).iter().all(|pid| before.contains(pid))
        });
        assert!(
            pid_with_marker(&cgroup_pids(&cgroup), "4747").is_none(),
            "a timed-out process survived in project-a"
        );
        assert!(
            pid_with_marker(&cgroup_pids(&cgroup), "4242").is_some(),
            "the sibling exec was killed by the other exec's timeout"
        );

        let result = sibling_thread.join().expect("sibling thread panicked");
        assert!(result.is_ok(), "sibling exec failed: {:?}", result.err());
    });
}

#[test]
fn timeout_kills_double_forked_daemons_in_a_project_cgroup() {
    if !ensure_delegated("timeout_kills_double_forked_daemons_in_a_project_cgroup") {
        return;
    }
    const DAEMON_MARKER: &str = "JGT_DAEMON_4711";
    let daemon = format!(
        r#"import os, time
# {DAEMON_MARKER}
if os.fork() > 0:
    os._exit(0)
os.setsid()
if os.fork() > 0:
    os._exit(0)
open("/workspace/daemon-started", "w").close()
time.sleep(120)
"#
    );

    let mut lab = fixture();
    let worktree = project_worktree(&lab, "a");
    lab.spec.limits = Some(LabLimits {
        pids: Some(128),
        ..LabLimits::default()
    });
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let cgroup = project_cgroup(handle, "a");
    run_ok(backend, handle, "command -v python3 >/dev/null || exit 2");

    let script = format!("python3 -c '{daemon}'; sleep 60");
    let request = shell_request(&script)
        .with_timeout(Duration::from_secs(3))
        .with_scope(project_scope("a"));
    let output = backend.exec(handle, &request).expect("timed exec returns");

    assert!(output.timed_out, "the wall-clock limit did not fire");
    assert!(
        worktree.join("daemon-started").exists(),
        "the double-forked daemon never started"
    );
    wait_until("the detached daemon leaves project-a", || {
        pid_with_marker(&cgroup_pids(&cgroup), DAEMON_MARKER).is_none()
    });
}
