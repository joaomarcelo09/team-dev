//! Narrowing one exec to a single project: a private mount namespace in which
//! only the worktree (at `/workspace`), its git metadata and a private `/tmp`
//! are visible.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use nix::fcntl::{OFlag, open};
use nix::sched::{CloneFlags, unshare};
use nix::sys::stat::Mode;

use crate::error::{Result, SandboxError};
use crate::namespace::util;
use crate::scope::{
    ExecScope, LAB_ROOT, SCOPED_HOME, ScopeAccess, TMP_DIR, WORKSPACE, is_below_lab,
};

/// Replace the lab-wide view of the current process with the scoped one.
///
/// Must run after joining the sandbox namespaces and before `chdir` and the
/// privilege drop. Every failure is an error: the caller must abort the exec and
/// never fall back to the unscoped view.
pub(crate) fn enter_scope(scope: &ExecScope) -> Result<()> {
    scope.validate()?;
    let scratch_home = open_scratch_home(scope)?;
    unshare(CloneFlags::CLONE_NEWNS)
        .map_err(|source| SandboxError::nix("unshare scoped mount namespace", source))?;
    // The new namespace must not propagate mounts back to the sandbox.
    util::make_mounts_private()?;
    // The exec already runs in its own pid namespace. A fresh procfs hides the
    // sandbox-wide one (and its `/proc/<pid>/root` links) behind the exec's own pids.
    util::mount_proc(Path::new("/proc"))?;

    let worktree = resolve_worktree(&scope.worktree)?;
    let git_mounts = open_git_mounts(scope)?;
    let workspace = Path::new(WORKSPACE);
    create_workspace_dir(workspace)?;
    // A non-recursive bind, like the read-only binds of the root filesystem:
    // no submount of the worktree is carried along.
    util::bind_non_recursive(&worktree, workspace)?;
    if scope.access == ScopeAccess::ReadOnly {
        util::remount_read_only_keeping_flags(workspace)?;
        util::verify_read_only(workspace)?;
        util::verify_read_only_tree(workspace)?;
    }
    cover_lab(&git_mounts, scope.access)?;
    mount_private_tmp(scope)?;
    mount_scratch_home(scratch_home)
}

/// Create the default `HOME` of a scoped exec on its private `/tmp`.
pub(crate) fn create_scoped_home() -> Result<()> {
    std::fs::create_dir_all(SCOPED_HOME).map_err(|source| SandboxError::io(SCOPED_HOME, source))
}

/// Canonicalize inside the sandbox and require the result to stay below `/lab`,
/// so a symlink in the path cannot lead out of the lab.
fn resolve_worktree(worktree: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(worktree).map_err(|source| SandboxError::ExecFailed {
        reason: format!("scope worktree {}: {source}", worktree.display()),
    })?;
    if !is_below_lab(&canonical) {
        return Err(SandboxError::ExecFailed {
            reason: format!(
                "scope worktree {} resolves to {}, which is not below {LAB_ROOT}",
                worktree.display(),
                canonical.display()
            ),
        });
    }
    if !canonical.is_dir() {
        return Err(SandboxError::ExecFailed {
            reason: format!("scope worktree {} is not a directory", canonical.display()),
        });
    }
    Ok(canonical)
}

/// Create the mount point and refuse a symlink there: the bind would follow it.
fn create_workspace_dir(workspace: &Path) -> Result<()> {
    std::fs::create_dir_all(workspace).map_err(|source| SandboxError::io(workspace, source))?;
    let meta = std::fs::symlink_metadata(workspace)
        .map_err(|source| SandboxError::io(workspace, source))?;
    if !meta.is_dir() {
        return Err(SandboxError::ExecFailed {
            reason: format!("{} is not a plain directory", workspace.display()),
        });
    }
    Ok(())
}

/// A git directory to show inside the cover at its own path.
struct GitMount {
    /// The canonical path, used as the mount point inside the cover.
    path: PathBuf,
    /// The source, opened before the cover hides it.
    source: OwnedFd,
    writable: bool,
}

/// Resolve and open the git directories while the whole lab is still visible.
///
/// The common directory comes first: the worktree's git directory lives inside
/// it and is mounted on top of it.
fn open_git_mounts(scope: &ExecScope) -> Result<Vec<GitMount>> {
    let common = scope
        .common_dir
        .as_deref()
        .map(|path| resolve_dir(path, "scope common dir"))
        .transpose()?;
    let git = scope
        .git_dir
        .as_deref()
        .map(|path| resolve_dir(path, "scope git dir"))
        .transpose()?;
    if let (Some(common), Some(git)) = (&common, &git)
        && !git.starts_with(common)
    {
        return Err(scope_error(format!(
            "scope git dir {} is not inside common dir {}",
            git.display(),
            common.display()
        )));
    }

    let mut mounts = Vec::new();
    if let Some(path) = common {
        mounts.push(open_git_mount(path, false)?);
    }
    if let Some(path) = git {
        mounts.push(open_git_mount(
            path,
            scope.access == ScopeAccess::ReadWrite,
        )?);
    }
    Ok(mounts)
}

/// Open the scratch home before the lab cover hides its `/lab` path.
fn open_scratch_home(scope: &ExecScope) -> Result<Option<OwnedFd>> {
    let Some(path) = scope.scratch_home.as_deref() else {
        return Ok(None);
    };
    let path = resolve_dir(path, "scope scratch home")?;
    let flags = OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    open(&path, flags, Mode::empty())
        .map(Some)
        .map_err(|errno| scope_error(format!("open scratch home {}: {errno}", path.display())))
}

fn open_git_mount(path: PathBuf, writable: bool) -> Result<GitMount> {
    let flags = OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let source = open(&path, flags, Mode::empty())
        .map_err(|errno| scope_error(format!("open git dir {}: {errno}", path.display())))?;
    Ok(GitMount {
        path,
        source,
        writable,
    })
}

/// Canonicalize inside the sandbox; the result must be a directory below `/lab`.
fn resolve_dir(path: &Path, what: &str) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|source| scope_error(format!("{what} {}: {source}", path.display())))?;
    if !is_below_lab(&canonical) {
        return Err(scope_error(format!(
            "{what} {} resolves to {}, which is not below {LAB_ROOT}",
            path.display(),
            canonical.display()
        )));
    }
    if !canonical.is_dir() {
        return Err(scope_error(format!(
            "{what} {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn scope_error(reason: String) -> SandboxError {
    SandboxError::ExecFailed { reason }
}

/// Hide the rest of the lab behind an empty read-only tmpfs, then show the git
/// directories on top of it.
///
/// The mount points are created while the cover is still writable; the sources
/// were opened before the cover and are bound through `/proc/self/fd`.
fn cover_lab(git_mounts: &[GitMount], access: ScopeAccess) -> Result<()> {
    let lab = Path::new(LAB_ROOT);
    util::mount_tmpfs(lab, "mode=0555,size=64k")?;
    for git in git_mounts {
        std::fs::create_dir_all(&git.path).map_err(|source| SandboxError::io(&git.path, source))?;
    }
    util::remount_read_only_keeping_flags(lab)?;
    util::verify_read_only(lab)?;
    for git in git_mounts {
        mount_git(git, access)?;
    }
    Ok(())
}

fn mount_git(git: &GitMount, access: ScopeAccess) -> Result<()> {
    let source = PathBuf::from(format!("/proc/self/fd/{}", git.source.as_raw_fd()));
    util::bind_non_recursive(&source, &git.path)?;
    if git.writable {
        return Ok(());
    }
    util::remount_read_only_keeping_flags(&git.path)?;
    util::verify_read_only(&git.path)?;
    if access == ScopeAccess::ReadOnly {
        util::verify_read_only_tree(&git.path)?;
    }
    Ok(())
}

/// Give the exec its own empty `/tmp`, so nothing is shared with other execs.
fn mount_private_tmp(scope: &ExecScope) -> Result<()> {
    let options = format!("mode=1777,size={}", scope.effective_tmp_bytes());
    util::mount_tmpfs(Path::new(TMP_DIR), &options)
}

/// Mount the per-role host scratch home over the scoped exec's private home.
fn mount_scratch_home(scratch_home: Option<OwnedFd>) -> Result<()> {
    let Some(scratch_home) = scratch_home else {
        return Ok(());
    };
    let target = Path::new(SCOPED_HOME);
    std::fs::create_dir_all(target).map_err(|source| SandboxError::io(target, source))?;
    let metadata =
        std::fs::symlink_metadata(target).map_err(|source| SandboxError::io(target, source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(scope_error(format!(
            "{} is not a plain directory",
            target.display()
        )));
    }
    let source = PathBuf::from(format!("/proc/self/fd/{}", scratch_home.as_raw_fd()));
    util::bind_non_recursive(&source, target)
}
