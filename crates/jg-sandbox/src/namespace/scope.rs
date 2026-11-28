//! Narrowing one exec to a single project: a private mount namespace in which
//! only the worktree (at `/workspace`), its git metadata and a private `/tmp`
//! are visible.

use std::os::{
    fd::{AsRawFd, OwnedFd},
    unix::fs::MetadataExt,
};
use std::path::{Path, PathBuf};

use nix::fcntl::{OFlag, open};
use nix::mount::{MntFlags, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::sys::stat::{Mode, fstat};

use crate::error::{Result, SandboxError};
use crate::namespace::util;
use crate::scope::{
    ExecScope, FONT_ROOT, LAB_ROOT, SCOPED_HOME, ScopeAccess, TMP_DIR, WORKSPACE, is_below_lab,
};

/// Replace the lab-wide view of the current process with the scoped one.
///
/// Must run after joining the sandbox namespaces and before `chdir` and the
/// privilege drop. Every failure is an error: the caller must abort the exec and
/// never fall back to the unscoped view.
pub(crate) fn enter_scope(scope: &ExecScope) -> Result<()> {
    scope.validate()?;
    let scratch_home = open_scratch_home(scope)?;
    let font_project = open_font_project(scope)?;
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
    // The private `/tmp` and the scratch home come before the lab cover: the
    // scratch home is bound by its `/lab` path, which the cover hides.
    mount_private_tmp(scope)?;
    mount_scratch_home(scratch_home)?;
    cover_lab(&git_mounts, scope.access)?;
    apply_font_access(scope.font_access, font_project)
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

struct FontProjectMount {
    target: PathBuf,
    source_path: PathBuf,
    _source: OwnedFd,
}

fn open_font_project(scope: &ExecScope) -> Result<Option<FontProjectMount>> {
    let Some(project) = scope.font_project.as_deref() else {
        return Ok(None);
    };
    let font = Path::new(FONT_ROOT);
    let font_metadata =
        std::fs::symlink_metadata(font).map_err(|source| SandboxError::io(font, source))?;
    if !font_metadata.is_dir() || font_metadata.file_type().is_symlink() {
        return Err(scope_error(format!(
            "{} is not a plain directory",
            font.display()
        )));
    }
    let target = font.join(project);
    let metadata =
        std::fs::symlink_metadata(&target).map_err(|source| SandboxError::io(&target, source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(scope_error(format!(
            "font project {} is not a plain directory",
            target.display()
        )));
    }
    let canonical_font =
        std::fs::canonicalize(font).map_err(|source| SandboxError::io(font, source))?;
    let canonical_target =
        std::fs::canonicalize(&target).map_err(|source| SandboxError::io(&target, source))?;
    if canonical_target.parent() != Some(canonical_font.as_path()) {
        return Err(scope_error(format!(
            "font project {} escapes {}",
            target.display(),
            font.display()
        )));
    }
    let flags = OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let source = open(&canonical_target, flags, Mode::empty()).map_err(|errno| {
        scope_error(format!(
            "open font project {}: {errno}",
            canonical_target.display()
        ))
    })?;
    Ok(Some(FontProjectMount {
        target,
        source_path: canonical_target,
        _source: source,
    }))
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

/// A scratch home pinned by an `O_PATH` descriptor and its canonical `/lab` path.
struct ScratchHome {
    source: OwnedFd,
    path: PathBuf,
}

/// Open the scratch home before the scoped mount namespace is created.
fn open_scratch_home(scope: &ExecScope) -> Result<Option<ScratchHome>> {
    let Some(path) = scope.scratch_home.as_deref() else {
        return Ok(None);
    };
    let path = resolve_dir(path, "scope scratch home")?;
    let flags = OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let source = open(&path, flags, Mode::empty())
        .map_err(|errno| scope_error(format!("open scratch home {}: {errno}", path.display())))?;
    Ok(Some(ScratchHome { source, path }))
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

/// Apply the role's project-directory access inside its private mount namespace.
fn apply_font_access(access: Option<ScopeAccess>, project: Option<FontProjectMount>) -> Result<()> {
    let font = Path::new(FONT_ROOT);
    let metadata =
        std::fs::symlink_metadata(font).map_err(|source| SandboxError::io(font, source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(scope_error(format!(
            "{} is not a plain directory",
            font.display()
        )));
    }

    if let Some(project) = project {
        let target = &project.target;
        let staging = Path::new(TMP_DIR).join(".jgf-font-project");
        std::fs::create_dir(&staging).map_err(|source| SandboxError::io(&staging, source))?;
        let pinned = fstat(&project._source)
            .map_err(|source| SandboxError::nix("inspect font project", source))?;
        let current = std::fs::metadata(&project.source_path)
            .map_err(|source| SandboxError::io(&project.source_path, source))?;
        if pinned.st_dev as u64 != current.dev() || pinned.st_ino as u64 != current.ino() {
            return Err(scope_error(
                "font project changed while preparing its scoped mount".to_string(),
            ));
        }
        util::bind_non_recursive(&project.source_path, &staging)?;
        util::mount_tmpfs(font, "mode=0755,size=64k")?;
        std::fs::create_dir_all(target).map_err(|source| SandboxError::io(target, source))?;
        util::bind_non_recursive(&staging, target)?;
        umount2(&staging, MntFlags::MNT_DETACH)
            .map_err(|source| SandboxError::nix("unmount staged font project", source))?;
        std::fs::remove_dir(&staging).map_err(|source| SandboxError::io(&staging, source))?;
        util::remount_read_only_keeping_flags(font)?;
        util::verify_read_only(font)?;
        if access == Some(ScopeAccess::ReadOnly) {
            util::remount_read_only_keeping_flags(target)?;
            util::verify_read_only(target)?;
        }
        return Ok(());
    }

    match access {
        Some(ScopeAccess::ReadWrite) => Ok(()),
        Some(ScopeAccess::ReadOnly) => {
            util::remount_read_only_keeping_flags(font)?;
            util::verify_read_only(font)?;
            // The rootfs binds /font non-recursively, so it has no inherited submounts.
            Ok(())
        }
        None => {
            util::mount_tmpfs(font, "mode=0555,size=64k")?;
            util::remount_read_only_keeping_flags(font)?;
            util::verify_read_only(font)?;
            Ok(())
        }
    }
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
///
/// The bind uses the `/lab` path inside the scoped mount namespace: the kernel
/// refuses (`EINVAL`) to bind a descriptor whose mount belongs to the sandbox's
/// previous namespace. The descriptor pins the directory, so a path swapped
/// since it was opened is refused instead of mounted.
fn mount_scratch_home(scratch_home: Option<ScratchHome>) -> Result<()> {
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
    let pinned = fstat(&scratch_home.source)
        .map_err(|source| SandboxError::nix("inspect scratch home", source))?;
    let current = std::fs::symlink_metadata(&scratch_home.path)
        .map_err(|source| SandboxError::io(&scratch_home.path, source))?;
    if !current.is_dir()
        || pinned.st_dev as u64 != current.dev()
        || pinned.st_ino as u64 != current.ino()
    {
        return Err(scope_error(
            "scratch home changed while preparing its scoped mount".to_string(),
        ));
    }
    util::bind_non_recursive(&scratch_home.path, target)
}
