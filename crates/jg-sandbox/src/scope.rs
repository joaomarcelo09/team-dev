//! Per-exec project scoping: which part of the lab one command may see.

use std::path::{Component, Path, PathBuf};

use crate::error::{Result, SandboxError};

/// The lab directory inside the sandbox; every scope path lives below it.
pub const LAB_ROOT: &str = "/lab";
/// Where a scoped exec sees its worktree.
pub const WORKSPACE: &str = "/workspace";
/// The private `/tmp` every scoped exec gets.
pub(crate) const TMP_DIR: &str = "/tmp";
/// `HOME` of a scoped exec when the request does not set one.
pub const SCOPED_HOME: &str = "/tmp/home";

/// Whether a scoped exec may change its worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeAccess {
    /// The worktree is writable (coder).
    ReadWrite,
    /// The worktree is mounted read-only (reviewer, tester).
    ReadOnly,
}

/// The slice of the lab a single exec is allowed to see.
///
/// All paths are absolute paths inside the sandbox, below `/lab`, and come from
/// the trusted host caller; they are never read from the worktree itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecScope {
    /// Read-write for a coder, read-only for a reviewer or tester.
    pub access: ScopeAccess,
    /// The project worktree, mounted at `/workspace`.
    pub worktree: PathBuf,
    /// The worktree's own git directory (`<common>/worktrees/<name>`).
    pub git_dir: Option<PathBuf>,
    /// The repository's common git directory (`.git`).
    pub common_dir: Option<PathBuf>,
}

impl ExecScope {
    /// A scope with the given access and worktree and no git metadata.
    pub fn new(access: ScopeAccess, worktree: impl Into<PathBuf>) -> Self {
        Self {
            access,
            worktree: worktree.into(),
            git_dir: None,
            common_dir: None,
        }
    }

    /// Set the worktree's own git directory.
    pub fn with_git_dir(mut self, git_dir: impl Into<PathBuf>) -> Self {
        self.git_dir = Some(git_dir.into());
        self
    }

    /// Set the repository's common git directory.
    pub fn with_common_dir(mut self, common_dir: impl Into<PathBuf>) -> Self {
        self.common_dir = Some(common_dir.into());
        self
    }

    /// Cheap, syntactic checks: every path is absolute, has no `..` and is below `/lab`.
    pub fn validate(&self) -> Result<()> {
        validate_scope_path(&self.worktree, "scope worktree")?;
        if let Some(git_dir) = &self.git_dir {
            validate_scope_path(git_dir, "scope git dir")?;
        }
        if let Some(common_dir) = &self.common_dir {
            validate_scope_path(common_dir, "scope common dir")?;
        }
        Ok(())
    }
}

fn validate_scope_path(path: &Path, what: &str) -> Result<()> {
    if !path.is_absolute() {
        return Err(scope_error(format!(
            "{what} {} is not absolute",
            path.display()
        )));
    }
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(scope_error(format!(
            "{what} {} contains ..",
            path.display()
        )));
    }
    if !is_below_lab(path) {
        return Err(scope_error(format!(
            "{what} {} is not below {LAB_ROOT}",
            path.display()
        )));
    }
    Ok(())
}

/// Whether `path` is strictly below `/lab` (the lab root itself does not count).
pub(crate) fn is_below_lab(path: &Path) -> bool {
    path.starts_with(LAB_ROOT) && path != Path::new(LAB_ROOT)
}

fn scope_error(reason: String) -> SandboxError {
    SandboxError::ExecFailed { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(path: &str) -> ExecScope {
        ExecScope::new(ScopeAccess::ReadWrite, path)
    }

    #[test]
    fn worktree_below_lab_is_valid() {
        assert!(scope("/lab/projects/a").validate().is_ok());
    }

    #[test]
    fn relative_parent_and_outside_paths_are_rejected() {
        for path in [
            "lab/a",
            "/lab/a/../b",
            "/lab",
            "/labx/a",
            "/etc",
            "/workspace",
        ] {
            assert!(scope(path).validate().is_err(), "{path} must be rejected");
        }
    }

    #[test]
    fn git_paths_are_validated_too() {
        let bad_git_dir = scope("/lab/a").with_git_dir("/etc/git");
        assert!(bad_git_dir.validate().is_err());
        let bad_common = scope("/lab/a").with_common_dir("/lab/../x");
        assert!(bad_common.validate().is_err());
        let good = scope("/lab/a")
            .with_git_dir("/lab/r/.git/worktrees/a")
            .with_common_dir("/lab/r/.git");
        assert!(good.validate().is_ok());
    }
}
