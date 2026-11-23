//! Per-project cgroup v2 limit and isolation tests.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jg_sandbox::{
    Backend, ExecOutput, ExecRequest, ExecScope, LabLimits, NamespaceBackend, ProjectLimits,
    SandboxError, SandboxHandle, ScopeAccess,
};
use support::{LabFixture, create_or_skip, ensure_delegated, fixture, run_ok};

const MB: u64 = 1024 * 1024;

fn limits(cpu_percent: Option<u32>, memory_bytes: Option<u64>, pids: Option<u64>) -> LabLimits {
    LabLimits {
        cpu_percent,
        memory_bytes,
        pids,
    }
}

fn project_limits(
    cpu_percent: Option<u32>,
    memory_bytes: Option<u64>,
    pids: Option<u64>,
) -> ProjectLimits {
    ProjectLimits {
        cpu_percent,
        memory_bytes,
        pids,
    }
}

/// Create the host worktree of a project and return it below the lab.
fn project_worktree(lab: &LabFixture, project: &str) -> PathBuf {
    let worktree = lab.spec.lab_dir.join("projects").join(project);
    fs::create_dir_all(&worktree).expect("project worktree");
    worktree
}

fn scoped_request(project: &str, limits: Option<ProjectLimits>, script: &str) -> ExecRequest {
    let mut scope = ExecScope::new(ScopeAccess::ReadWrite, format!("/lab/projects/{project}"))
        .with_project(project);
    if let Some(limits) = limits {
        scope = scope.with_limits(limits);
    }
    ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        script.to_string(),
    ])
    .with_timeout(Duration::from_secs(30))
    .with_scope(scope)
}

fn exec_scoped(
    backend: NamespaceBackend,
    handle: &SandboxHandle,
    project: &str,
    limits: Option<ProjectLimits>,
    script: &str,
) -> jg_sandbox::Result<ExecOutput> {
    backend.exec(handle, &scoped_request(project, limits, script))
}

fn run_scoped_ok(
    backend: NamespaceBackend,
    handle: &SandboxHandle,
    project: &str,
    limits: Option<ProjectLimits>,
    script: &str,
) -> ExecOutput {
    let output = exec_scoped(backend, handle, project, limits, script)
        .unwrap_or_else(|error| panic!("scoped exec failed: {error}"));
    assert_eq!(
        output.exit_code,
        Some(0),
        "command failed: {}",
        output.stderr_text()
    );
    output
}

fn project_cgroup(handle: &SandboxHandle, project: &str) -> PathBuf {
    let lab = handle
        .cgroup_path()
        .unwrap_or_else(|| panic!("sandbox has no cgroup path"));
    lab.join(format!("project-{project}"))
}

fn read_trim(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim()
        .to_string()
}

fn oom_kills(path: &Path) -> u64 {
    read_trim(&path.join("memory.events"))
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill "))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[test]
fn project_cgroup_holds_its_limits() {
    if !ensure_delegated("project_cgroup_holds_its_limits") {
        return;
    }
    let mut lab = fixture();
    project_worktree(&lab, "a");
    lab.spec.limits = Some(limits(Some(100), Some(512 * MB), Some(128)));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };
    let backend = NamespaceBackend::new();

    let output = run_scoped_ok(
        backend,
        guard.handle(),
        "a",
        Some(project_limits(Some(50), Some(256 * MB), Some(32))),
        "cat /proc/self/cgroup",
    );
    assert!(
        output.stdout_text().contains("project-a"),
        "exec did not join project-a: {}",
        output.stdout_text()
    );

    let path = project_cgroup(guard.handle(), "a");
    assert_eq!(read_trim(&path.join("cpu.max")), "50000 100000");
    assert_eq!(read_trim(&path.join("memory.max")), (256 * MB).to_string());
    assert_eq!(read_trim(&path.join("memory.swap.max")), "0");
    assert_eq!(read_trim(&path.join("pids.max")), "32");

    // A second exec reuses the cgroup and rewrites the limits in place.
    run_scoped_ok(
        backend,
        guard.handle(),
        "a",
        Some(project_limits(Some(25), Some(128 * MB), Some(16))),
        "true",
    );
    assert_eq!(read_trim(&path.join("cpu.max")), "25000 100000");
    assert_eq!(read_trim(&path.join("memory.max")), (128 * MB).to_string());
    assert_eq!(read_trim(&path.join("pids.max")), "16");
}

#[test]
fn memory_hog_in_a_is_oom_killed_while_b_runs() {
    if !ensure_delegated("memory_hog_in_a_is_oom_killed_while_b_runs") {
        return;
    }
    let mut lab = fixture();
    project_worktree(&lab, "a");
    project_worktree(&lab, "b");
    lab.spec.limits = Some(limits(None, Some(256 * MB), None));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };
    let backend = NamespaceBackend::new();
    run_ok(
        backend,
        guard.handle(),
        "command -v python3 >/dev/null || exit 2",
    );

    let hog = "python3 -c 'x = bytearray(256 * 1024 * 1024)'";
    // The OOM killer may take the command, the shell or the supervisor; any of
    // those means the hog did not escape its cap and report success.
    if let Ok(output) = exec_scoped(
        backend,
        guard.handle(),
        "a",
        Some(project_limits(None, Some(64 * MB), None)),
        hog,
    ) {
        assert_ne!(output.exit_code, Some(0), "hog was not OOM-killed");
    }

    let a = project_cgroup(guard.handle(), "a");
    assert!(
        oom_kills(&a) > 0,
        "no OOM kill recorded in project-a: {}",
        read_trim(&a.join("memory.events"))
    );

    let output = run_scoped_ok(backend, guard.handle(), "b", None, "echo b-alive");
    assert_eq!(output.stdout_text(), "b-alive\n");
}

#[test]
fn pids_limit_in_a_still_lets_b_fork() {
    if !ensure_delegated("pids_limit_in_a_still_lets_b_fork") {
        return;
    }
    const FORK_LOOP: &str = r#"python3 -c '
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
    const FIVE_FORKS: &str = r#"python3 -c '
import os, time
kids = []
for _ in range(5):
    pid = os.fork()
    if pid == 0:
        time.sleep(0.1)
        os._exit(0)
    kids.append(pid)
for pid in kids:
    os.waitpid(pid, 0)
print(len(kids))
'"#;

    let mut lab = fixture();
    project_worktree(&lab, "a");
    project_worktree(&lab, "b");
    lab.spec.limits = Some(limits(None, None, Some(128)));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };
    let backend = NamespaceBackend::new();
    run_ok(
        backend,
        guard.handle(),
        "command -v python3 >/dev/null || exit 2",
    );

    let pids = 32;
    let output = run_scoped_ok(
        backend,
        guard.handle(),
        "a",
        Some(project_limits(None, None, Some(pids))),
        FORK_LOOP,
    );
    let count: u64 = output
        .stdout_text()
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("unexpected fork loop output {:?}", output.stdout_text()));
    assert!(count > 0, "project-a could not fork at all");
    assert!(count < 500, "project-a fork loop was not capped");
    assert!(count <= pids, "fork count {count} exceeds pids.max {pids}");

    let output = run_scoped_ok(backend, guard.handle(), "b", None, FIVE_FORKS);
    assert_eq!(
        output.stdout_text().trim(),
        "5",
        "project-b could not fork normally: {}",
        output.stderr_text()
    );
}

#[test]
fn project_limit_above_the_lab_limit_is_rejected() {
    if !ensure_delegated("project_limit_above_the_lab_limit_is_rejected") {
        return;
    }
    let mut lab = fixture();
    project_worktree(&lab, "a");
    lab.spec.limits = Some(limits(None, Some(64 * MB), None));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };

    let error = exec_scoped(
        NamespaceBackend::new(),
        guard.handle(),
        "a",
        Some(project_limits(None, Some(128 * MB), None)),
        "true",
    )
    .expect_err("a project above the lab memory limit must be rejected");
    match &error {
        SandboxError::Cgroup { reason, .. } => {
            assert!(
                reason.contains("exceeds"),
                "unclear rejection message: {reason}"
            );
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(
        !project_cgroup(guard.handle(), "a").exists(),
        "a rejected project cgroup was still created"
    );
}

#[test]
fn project_on_a_lab_without_a_cgroup_is_rejected() {
    let lab = fixture();
    let worktree = project_worktree(&lab, "a");
    let marker = worktree.join("ran");
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };

    let error = exec_scoped(
        NamespaceBackend::new(),
        guard.handle(),
        "a",
        None,
        "echo ran > ran",
    )
    .expect_err("a project on a lab without a cgroup must be rejected");
    assert!(
        matches!(error, SandboxError::ExecFailed { .. }),
        "unexpected error: {error}"
    );
    assert!(!marker.exists(), "the command ran despite the rejection");
}

#[test]
fn destroy_removes_the_lab_and_its_project_cgroups() {
    if !ensure_delegated("destroy_removes_the_lab_and_its_project_cgroups") {
        return;
    }
    let mut lab = fixture();
    project_worktree(&lab, "a");
    project_worktree(&lab, "b");
    lab.spec.limits = Some(limits(None, Some(256 * MB), Some(128)));
    let Some(guard) = create_or_skip(NamespaceBackend::new(), &lab.spec) else {
        return;
    };
    let backend = NamespaceBackend::new();
    for project in ["a", "b"] {
        run_scoped_ok(backend, guard.handle(), project, None, "true");
    }

    let lab_cgroup = guard
        .handle()
        .cgroup_path()
        .expect("sandbox has a cgroup path")
        .to_path_buf();
    let a = project_cgroup(guard.handle(), "a");
    let b = project_cgroup(guard.handle(), "b");
    assert!(lab_cgroup.is_dir() && a.is_dir() && b.is_dir());

    drop(guard);
    assert!(!lab_cgroup.exists(), "lab cgroup survived destroy");
    assert!(!a.exists(), "project-a cgroup survived destroy");
    assert!(!b.exists(), "project-b cgroup survived destroy");
}
