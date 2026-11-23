//! Per-exec project scope: git metadata mounts and the private `/tmp`.

mod support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use jg_sandbox::{
    Backend, ExecOutput, ExecRequest, ExecScope, NamespaceBackend, SandboxHandle, ScopeAccess,
};
use support::{LabFixture, create_or_skip, fixture, run_ok, run_shell};

const COMMON: &str = "/lab/repo/.git";
const GIT_DIR: &str = "/lab/repo/.git/worktrees/wt";
const WORKTREE: &str = "/lab/wt";

fn host_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("run host git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with one commit and a linked worktree, laid out as the sandbox
/// sees it: `/lab/repo` and `/lab/wt`. A second, unrelated repository sits in
/// `/lab/other`.
fn git_fixture() -> LabFixture {
    let lab = fixture();
    let root = &lab.spec.lab_dir;
    for (name, file) in [("repo", "tracked.txt"), ("other", "other.txt")] {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("repo dir");
        host_git(&dir, &["init", "-q"]);
        std::fs::write(dir.join(file), "one\n").expect("tracked file");
        host_git(&dir, &["add", file]);
        host_git(&dir, &["commit", "-q", "-m", "first"]);
    }
    host_git(
        &root.join("repo"),
        &["worktree", "add", "-q", "-b", "feature", "../wt"],
    );
    // Git recorded host paths; the sandbox sees the lab at `/lab`.
    std::fs::write(root.join("wt/.git"), format!("gitdir: {GIT_DIR}\n")).expect("gitfile");
    lab
}

fn scope(access: ScopeAccess) -> ExecScope {
    ExecScope::new(access, WORKTREE)
        .with_git_dir(GIT_DIR)
        .with_common_dir(COMMON)
}

fn run_scoped(
    backend: NamespaceBackend,
    handle: &SandboxHandle,
    scope: ExecScope,
    script: &str,
) -> ExecOutput {
    let request = ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        script.to_string(),
    ])
    .with_timeout(Duration::from_secs(30))
    .with_env("GIT_CONFIG_NOSYSTEM", "1")
    .with_env("GIT_CONFIG_COUNT", "1")
    .with_env("GIT_CONFIG_KEY_0", "safe.directory")
    .with_env("GIT_CONFIG_VALUE_0", "*")
    .with_scope(scope);
    match backend.exec(handle, &request) {
        Ok(output) => output,
        Err(error) => panic!("scoped exec failed: {error}"),
    }
}

fn assert_erofs(output: &ExecOutput, what: &str) {
    assert_ne!(output.exit_code, Some(0), "{what} must fail");
    assert!(
        output.stderr_text().contains("Read-only file system"),
        "{what} did not fail with EROFS: {}",
        output.stderr_text()
    );
}

#[test]
fn coder_can_use_git_but_not_write_the_common_dir() {
    let lab = git_fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let coder = || scope(ScopeAccess::ReadWrite);

    let status = run_scoped(
        backend,
        handle,
        coder(),
        "echo two >> tracked.txt && echo new > untracked.txt && git status --short",
    );
    assert_eq!(status.exit_code, Some(0), "{}", status.stderr_text());
    assert_eq!(status.stdout_text(), " M tracked.txt\n?? untracked.txt\n");
    let diff = run_scoped(backend, handle, coder(), "git diff");
    assert_eq!(diff.exit_code, Some(0), "{}", diff.stderr_text());
    assert!(
        diff.stdout_text().contains("+two"),
        "{}",
        diff.stdout_text()
    );

    // The worktree's own git dir is writable: index and HEAD updates work.
    let git_dir_write = run_scoped(
        backend,
        handle,
        coder(),
        "git update-index --assume-unchanged tracked.txt && \
         git symbolic-ref HEAD refs/heads/renamed && git symbolic-ref HEAD",
    );
    assert_eq!(
        git_dir_write.exit_code,
        Some(0),
        "{}",
        git_dir_write.stderr_text()
    );
    assert_eq!(git_dir_write.stdout_text(), "refs/heads/renamed\n");
    let host_head = std::fs::read_to_string(lab.spec.lab_dir.join("repo/.git/worktrees/wt/HEAD"))
        .expect("host reads HEAD");
    assert_eq!(host_head, "ref: refs/heads/renamed\n");

    // The common dir is read-only: objects and refs cannot be written.
    for script in [
        "touch /lab/repo/.git/objects/x",
        "touch /lab/repo/.git/refs/heads/x",
        "echo x > /lab/repo/.git/config",
        "git add untracked.txt",
    ] {
        let output = run_scoped(backend, handle, coder(), script);
        assert_erofs(&output, script);
    }
    assert!(!lab.spec.lab_dir.join("repo/.git/objects/x").exists());
}

#[test]
fn reviewer_git_metadata_is_read_only() {
    let lab = git_fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let reviewer = || scope(ScopeAccess::ReadOnly);

    let read = run_scoped(
        backend,
        handle,
        reviewer(),
        "git status --short; git diff; git log --oneline | wc -l",
    );
    assert_eq!(read.exit_code, Some(0), "{}", read.stderr_text());
    assert_eq!(read.stdout_text(), "1\n");

    for script in [
        "touch /lab/repo/.git/objects/x",
        "touch /lab/repo/.git/worktrees/wt/index.lock",
        "git symbolic-ref HEAD refs/heads/renamed",
        "echo x > /workspace/new.txt",
    ] {
        let output = run_scoped(backend, handle, reviewer(), script);
        assert_erofs(&output, script);
    }
    let head = std::fs::read_to_string(lab.spec.lab_dir.join("repo/.git/worktrees/wt/HEAD"))
        .expect("host reads HEAD");
    assert_eq!(head, "ref: refs/heads/feature\n");
}

#[test]
fn scoped_exec_cannot_see_another_projects_git_dir() {
    let lab = git_fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let output = run_scoped(
        backend,
        guard.handle(),
        scope(ScopeAccess::ReadWrite),
        "ls -A /lab; ls /lab/other/.git; cat /lab/other/other.txt; ls -A /lab/repo",
    );

    assert_eq!(output.stdout_text(), "repo\n.git\n");
    assert!(output.stderr_text().contains("/lab/other"));
    assert!(!output.stdout_text().contains("other.txt"));
    // The main working tree is not part of the scope either.
    assert!(!output.stdout_text().contains("tracked.txt"));
    let unscoped = run_ok(backend, guard.handle(), "ls /lab/other");
    assert!(unscoped.stdout_text().contains("other.txt"));
}

#[test]
fn git_paths_must_be_inside_the_lab_and_the_common_dir() {
    let lab = git_fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    std::os::unix::fs::symlink("/usr", lab.spec.lab_dir.join("escape")).expect("escape link");

    let bad_scopes = [
        scope(ScopeAccess::ReadWrite).with_git_dir("/lab/other/.git"),
        scope(ScopeAccess::ReadWrite).with_common_dir("/lab/other/.git"),
        scope(ScopeAccess::ReadWrite).with_git_dir("/lab/escape"),
        scope(ScopeAccess::ReadWrite).with_common_dir("/usr"),
        scope(ScopeAccess::ReadWrite).with_common_dir("/lab/missing"),
        scope(ScopeAccess::ReadWrite).with_git_dir("/lab/repo/.git/../../other/.git"),
        scope(ScopeAccess::ReadOnly).with_git_dir("/lab/repo/.git/HEAD"),
    ];
    for bad in bad_scopes {
        let request = ExecRequest::new(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "touch /tmp/ran".to_string(),
        ])
        .with_scope(bad.clone());
        let result = backend.exec(guard.handle(), &request);
        assert!(result.is_err(), "scope {bad:?} must be rejected");
    }
    let marker = run_shell(backend, guard.handle(), "test ! -e /tmp/ran");
    assert_eq!(marker.exit_code, Some(0), "a rejected scope still ran");
}

#[test]
fn scoped_execs_have_a_private_tmp_and_unscoped_keeps_the_shared_one() {
    let lab = fixture();
    std::fs::create_dir_all(lab.spec.lab_dir.join("proj-a")).expect("project a");
    std::fs::create_dir_all(lab.spec.lab_dir.join("proj-b")).expect("project b");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let scoped = |project: &str| ExecScope::new(ScopeAccess::ReadWrite, format!("/lab/{project}"));

    let write = run_scoped(
        backend,
        handle,
        scoped("proj-a"),
        "echo a > /tmp/from-a && ls /tmp",
    );
    assert_eq!(write.exit_code, Some(0), "{}", write.stderr_text());
    assert_eq!(write.stdout_text(), "from-a\nhome\n");

    // Neither another scope, nor the same project again, nor the lab-wide view sees it.
    let other = run_scoped(backend, handle, scoped("proj-b"), "ls /tmp");
    assert_eq!(other.stdout_text(), "home\n");
    let again = run_scoped(backend, handle, scoped("proj-a"), "ls /tmp");
    assert_eq!(again.stdout_text(), "home\n");
    run_ok(backend, handle, "test ! -e /tmp/from-a");

    // The unscoped /tmp stays shared between execs and hidden from scoped ones.
    run_ok(backend, handle, "echo shared > /tmp/unscoped-file");
    let read = run_ok(backend, handle, "cat /tmp/unscoped-file");
    assert_eq!(read.stdout_text(), "shared\n");
    let hidden = run_scoped(
        backend,
        handle,
        scoped("proj-a"),
        "test ! -e /tmp/unscoped-file",
    );
    assert_eq!(hidden.exit_code, Some(0), "{}", hidden.stderr_text());
}
