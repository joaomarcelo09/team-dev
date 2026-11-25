//! Per-exec project scope: `/workspace` and `/font` mounts, role access and lab hiding.

mod support;

use std::os::unix::fs::symlink;
use std::time::Duration;

use jg_sandbox::{
    Backend, ExecOutput, ExecRequest, ExecScope, NamespaceBackend, SandboxHandle, ScopeAccess,
};
use support::{create_or_skip, fixture, process_args, run_ok, run_shell, wait_until};

fn shell_request(script: &str) -> ExecRequest {
    ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        script.to_string(),
    ])
    .with_timeout(Duration::from_secs(30))
}

fn run_scoped(
    backend: NamespaceBackend,
    handle: &SandboxHandle,
    scope: ExecScope,
    script: &str,
) -> ExecOutput {
    let request = shell_request(script).with_scope(scope);
    match backend.exec(handle, &request) {
        Ok(output) => output,
        Err(error) => panic!("scoped exec failed: {error}"),
    }
}

fn coder(worktree: &str) -> ExecScope {
    ExecScope::new(ScopeAccess::ReadWrite, worktree)
}

fn reviewer(worktree: &str) -> ExecScope {
    ExecScope::new(ScopeAccess::ReadOnly, worktree)
}

#[test]
fn coder_writes_land_in_the_host_worktree() {
    let lab = fixture();
    let worktree = lab.spec.lab_dir.join("proj-a");
    std::fs::create_dir_all(&worktree).expect("worktree");
    std::fs::write(worktree.join("existing.txt"), "from-host").expect("seed file");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let output = run_scoped(
        backend,
        guard.handle(),
        coder("/lab/proj-a"),
        "cat existing.txt && echo from-coder > /workspace/new.txt && pwd && echo $HOME",
    );

    assert_eq!(output.exit_code, Some(0), "{}", output.stderr_text());
    assert_eq!(output.stdout_text(), "from-host/workspace\n/tmp/home\n");
    assert_eq!(
        std::fs::read_to_string(worktree.join("new.txt")).expect("host sees the write"),
        "from-coder\n"
    );
}

#[test]
fn request_cwd_and_home_override_the_scoped_defaults() {
    let lab = fixture();
    std::fs::create_dir_all(lab.spec.lab_dir.join("proj-a")).expect("worktree");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let request = shell_request("pwd && echo $HOME")
        .with_scope(coder("/lab/proj-a"))
        .with_cwd("/tmp")
        .with_env("HOME", "/tmp/custom");
    let output = backend
        .exec(guard.handle(), &request)
        .expect("scoped exec runs");

    assert_eq!(output.stdout_text(), "/tmp\n/tmp/custom\n");
}

#[test]
fn reviewer_scope_is_read_only() {
    let lab = fixture();
    let worktree = lab.spec.lab_dir.join("proj-a");
    std::fs::create_dir_all(&worktree).expect("worktree");
    std::fs::write(worktree.join("existing.txt"), "from-host").expect("seed file");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let read = run_scoped(
        backend,
        guard.handle(),
        reviewer("/lab/proj-a"),
        "cat /workspace/existing.txt",
    );
    assert_eq!(read.stdout_text(), "from-host");

    for script in [
        "echo x > /workspace/new.txt",
        "echo x >> /workspace/existing.txt",
        "rm /workspace/existing.txt",
        "mkdir /workspace/dir",
    ] {
        let output = run_scoped(backend, guard.handle(), reviewer("/lab/proj-a"), script);
        assert_ne!(output.exit_code, Some(0), "`{script}` must fail");
        assert!(
            output.stderr_text().contains("Read-only file system"),
            "`{script}` did not fail with EROFS: {}",
            output.stderr_text()
        );
    }
    assert!(!worktree.join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(worktree.join("existing.txt")).expect("untouched"),
        "from-host"
    );
}

#[test]
fn font_mount_obeys_role_access_and_hides_other_projects_from_coders() {
    let mut lab = fixture();
    std::fs::create_dir_all(lab.spec.lab_dir.join("proj-a")).expect("worktree");
    let font_dir = lab.temp.path().join("font");
    let project = font_dir.join("project-a");
    std::fs::create_dir_all(&project).expect("font project");
    std::fs::write(project.join("existing.txt"), "from-host").expect("seed project file");
    lab.spec.font_dir = Some(font_dir.clone());
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let coordinator = run_scoped(
        backend,
        guard.handle(),
        coder("/lab/proj-a").with_font_access(ScopeAccess::ReadWrite),
        "echo from-coordinator > /font/project-a/new.txt && cat /font/project-a/existing.txt",
    );
    assert_eq!(
        coordinator.exit_code,
        Some(0),
        "{}",
        coordinator.stderr_text()
    );
    assert_eq!(
        std::fs::read_to_string(project.join("new.txt")).expect("coordinator write"),
        "from-coordinator\n"
    );

    let reviewer = run_scoped(
        backend,
        guard.handle(),
        reviewer("/lab/proj-a").with_font_access(ScopeAccess::ReadOnly),
        "cat /font/project-a/existing.txt && echo x > /font/project-a/reviewer.txt",
    );
    assert_ne!(reviewer.exit_code, Some(0));
    assert!(reviewer.stdout_text().contains("from-host"));
    assert!(
        reviewer.stderr_text().contains("Read-only file system"),
        "{}",
        reviewer.stderr_text()
    );
    assert!(!project.join("reviewer.txt").exists());

    let coder = run_scoped(
        backend,
        guard.handle(),
        coder("/lab/proj-a"),
        "ls -A /font; cat /font/project-a/existing.txt",
    );
    assert!(
        !coder.stdout_text().contains("from-host"),
        "font content leaked to a coder: {}",
        coder.stdout_text()
    );
    assert!(
        coder.stderr_text().contains("project-a"),
        "coder unexpectedly accessed /font: {}",
        coder.stderr_text()
    );
}

#[test]
fn project_scoped_font_mount_exposes_only_the_selected_project_read_write() {
    let mut lab = fixture();
    std::fs::create_dir_all(lab.spec.lab_dir.join("proj-a")).expect("worktree");
    let font_dir = lab.temp.path().join("font");
    let project = font_dir.join("project-a");
    let other_project = font_dir.join("project-b");
    std::fs::create_dir_all(&project).expect("selected project");
    std::fs::create_dir_all(&other_project).expect("other project");
    std::fs::write(project.join("SPEC.md"), "project-a spec").expect("selected spec");
    std::fs::write(other_project.join("SPEC.md"), "project-b spec").expect("other spec");
    lab.spec.font_dir = Some(font_dir.clone());
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let scope = coder("/lab/proj-a")
        .with_font_access(ScopeAccess::ReadWrite)
        .with_font_project("project-a");

    let output = run_scoped(
        backend,
        guard.handle(),
        scope,
        "cat /font/project-a/SPEC.md && test ! -e /font/project-b/SPEC.md && echo updated > /font/project-a/notes.txt",
    );

    assert_eq!(output.exit_code, Some(0), "{}", output.stderr_text());
    assert_eq!(output.stdout_text(), "project-a spec");
    assert_eq!(
        std::fs::read_to_string(project.join("notes.txt")).expect("selected write"),
        "updated\n"
    );
    assert!(!other_project.join("notes.txt").exists());
}

#[test]
fn font_project_scope_rejects_traversal_and_cross_project_mismatch() {
    let traversal = coder("/lab/project-a")
        .with_project("project-a")
        .with_font_access(ScopeAccess::ReadWrite)
        .with_font_project("../project-b");
    assert!(traversal.validate().is_err());

    let mismatch = coder("/lab/project-a")
        .with_project("project-a")
        .with_font_access(ScopeAccess::ReadWrite)
        .with_font_project("project-b");
    assert!(mismatch.validate().is_err());
}

#[test]
fn scoped_exec_does_not_see_other_projects_but_unscoped_does() {
    let lab = fixture();
    for project in ["proj-a", "proj-b"] {
        let dir = lab.spec.lab_dir.join(project);
        std::fs::create_dir_all(&dir).expect("project dir");
        std::fs::write(dir.join("secret.txt"), format!("{project}-secret")).expect("secret");
    }
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let scoped = run_scoped(
        backend,
        guard.handle(),
        coder("/lab/proj-a"),
        "ls -A /lab | wc -l; ls /workspace; cat /lab/proj-b/secret.txt; \
         touch /lab/x 2>&1; ls /lab/proj-a; echo done",
    );
    assert_eq!(scoped.exit_code, Some(0), "{}", scoped.stderr_text());
    let text = scoped.stdout_text();
    assert!(text.starts_with("0\nsecret.txt\n"), "lab not empty: {text}");
    assert!(!text.contains("proj-b-secret"), "other project leaked");
    assert!(
        scoped.stderr_text().contains("proj-b/secret.txt"),
        "reading project b must fail: {}",
        scoped.stderr_text()
    );

    let unscoped = run_ok(
        backend,
        guard.handle(),
        "cat /lab/proj-a/secret.txt /lab/proj-b/secret.txt",
    );
    assert_eq!(unscoped.stdout_text(), "proj-a-secretproj-b-secret");
}

#[test]
fn dotdot_and_symlinks_cannot_reach_a_sibling_project() {
    let lab = fixture();
    let project_a = lab.spec.lab_dir.join("proj-a");
    let project_b = lab.spec.lab_dir.join("proj-b");
    std::fs::create_dir_all(&project_a).expect("project a");
    std::fs::create_dir_all(&project_b).expect("project b");
    std::fs::write(project_b.join("secret.txt"), "proj-b-secret").expect("secret");
    symlink("/lab/proj-b", project_a.join("to-b")).expect("absolute link");
    symlink("../proj-b", project_a.join("to-b-relative")).expect("relative link");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let output = run_scoped(
        backend,
        guard.handle(),
        coder("/lab/proj-a"),
        "cat /workspace/to-b/secret.txt; cat /workspace/to-b-relative/secret.txt; \
         cat /workspace/../proj-b/secret.txt; cat /workspace/../lab/proj-b/secret.txt; \
         cd /workspace/.. && ls -A /lab; true",
    );

    assert!(
        !output.stdout_text().contains("proj-b-secret"),
        "a sibling project was reachable: {}",
        output.stdout_text()
    );
    assert!(!output.stderr_text().is_empty());
}

#[test]
fn worktree_outside_the_lab_is_rejected() {
    let lab = fixture();
    std::fs::create_dir_all(lab.spec.lab_dir.join("proj-a")).expect("project dir");
    symlink("/usr", lab.spec.lab_dir.join("escape")).expect("escape link");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let bad_worktrees = [
        "/usr",
        "/lab",
        "/lab/../usr",
        "lab/proj-a",
        "/lab/missing",
        "/lab/escape",
    ];
    for worktree in bad_worktrees {
        let request = shell_request("touch /tmp/ran").with_scope(coder(worktree));
        let result = backend.exec(guard.handle(), &request);
        assert!(result.is_err(), "worktree {worktree} must be rejected");
    }

    let marker = run_shell(backend, guard.handle(), "test ! -e /tmp/ran");
    assert_eq!(marker.exit_code, Some(0), "a rejected scope still ran");
}

/// The pid of a process as seen from the sandbox pid namespace, found from the host.
fn sandbox_pid_of(marker: &str) -> Option<i32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Ok(host_pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if !process_args(host_pid).iter().any(|arg| arg == marker) {
            continue;
        }
        let status = std::fs::read_to_string(format!("/proc/{host_pid}/status")).ok()?;
        let line = status.lines().find(|line| line.starts_with("NSpid:"))?;
        // Outermost first: host, sandbox, exec.
        let pids: Vec<i32> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|pid| pid.parse().ok())
            .collect();
        if pids.len() == 3 {
            return Some(pids[1]);
        }
    }
    None
}

#[test]
fn scoped_exec_proc_shows_only_its_own_pids() {
    let lab = fixture();
    for project in ["proj-a", "proj-b"] {
        std::fs::create_dir_all(lab.spec.lab_dir.join(project)).expect("project dir");
    }
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    // Advance the sandbox pid counter, so the pid of the other exec cannot
    // coincide with a small pid of the scoped exec's own namespace.
    run_ok(
        backend,
        guard.handle(),
        "for i in $(seq 40); do /bin/true; done",
    );
    let marker = "4711";
    // Long-lived exec of another project; its own timeout ends it.
    let other_request = ExecRequest::new(vec!["sleep".to_string(), marker.to_string()])
        .with_timeout(Duration::from_secs(5))
        .with_scope(coder("/lab/proj-b"));
    let handle = guard.handle();

    std::thread::scope(|threads| {
        threads.spawn(|| backend.exec(handle, &other_request));
        wait_until("the other exec is running", || {
            sandbox_pid_of(marker).is_some()
        });
        let other_pid = sandbox_pid_of(marker).expect("sandbox pid of the other exec");
        assert!(
            other_pid > 40,
            "pid {other_pid} may collide with the scoped exec"
        );

        let script = format!(
            "echo pids:$(ls /proc | grep -E '^[0-9]+$' | tr '\\n' ' '); \
             ls /proc/1/root/lab >/dev/null 2>&1 && echo init-root:readable; \
             ls /proc/{other_pid}/root/workspace >/dev/null 2>&1 && echo other-root:readable; \
             ls /proc/{other_pid} >/dev/null 2>&1 && echo other-pid:visible; \
             echo done"
        );
        let output = run_scoped(backend, handle, coder("/lab/proj-a"), &script);
        assert_eq!(output.exit_code, Some(0), "{}", output.stderr_text());
        let text = output.stdout_text();
        let pids = text
            .lines()
            .find_map(|line| line.strip_prefix("pids:"))
            .expect("pids line");
        let pids: Vec<i32> = pids
            .split_whitespace()
            .filter_map(|pid| pid.parse().ok())
            .collect();
        // Only the exec's own init, shell and its helpers: all small pids of the
        // exec namespace, never the sandbox-wide pid of the other exec.
        assert!(!pids.contains(&other_pid), "other exec visible: {text}");
        assert!(pids.len() <= 6, "too many pids visible: {text}");
        assert!(!text.contains("readable"), "root link readable: {text}");
        assert!(!text.contains("visible"), "other pid visible: {text}");
        assert!(text.trim_end().ends_with("done"), "script failed: {text}");
    });
}
