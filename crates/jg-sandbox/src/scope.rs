//! Per-exec project scoping: which part of the lab one command may see.

use std::path::{Component, Path, PathBuf};

use crate::error::{Result, SandboxError};
use crate::spec::MIN_DISK_BYTES;

/// The lab directory inside the sandbox; every scope path lives below it.
pub const LAB_ROOT: &str = "/lab";
/// The lab's projects directory at the sandbox root.
pub const FONT_ROOT: &str = "/font";
/// Where a scoped exec sees its worktree.
pub const WORKSPACE: &str = "/workspace";
/// The private `/tmp` every scoped exec gets.
pub(crate) const TMP_DIR: &str = "/tmp";
/// `HOME` of a scoped exec when the request does not set one.
pub const SCOPED_HOME: &str = "/tmp/home";
/// Default size of the private `/tmp` tmpfs of a scoped exec (256 MiB).
pub const DEFAULT_SCOPED_TMP_BYTES: u64 = 256 * 1024 * 1024;

/// Whether a scoped exec may change its worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeAccess {
    /// The worktree is writable (coder).
    ReadWrite,
    /// The worktree is mounted read-only (reviewer, tester).
    ReadOnly,
}

/// Resource limits for one project inside a lab.
///
/// Every field is optional; a missing value leaves that resource unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProjectLimits {
    /// CPU quota where 100 means one full core.
    pub cpu_percent: Option<u32>,
    /// Memory limit in bytes.
    pub memory_bytes: Option<u64>,
    /// Maximum number of tasks in the project cgroup.
    pub pids: Option<u64>,
}

/// The slice of the lab a single exec is allowed to see.
///
/// All paths are absolute paths inside the sandbox, below `/lab`, and come from
/// the trusted host caller; they are never read from the worktree itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecScope {
    /// Read-write for a coder, read-only for a reviewer or tester.
    pub access: ScopeAccess,
    /// How this exec may access the lab's project directory; `None` hides it.
    pub font_access: Option<ScopeAccess>,
    /// Restrict the font mount to one project directory.
    pub font_project: Option<String>,
    /// The project worktree, mounted at `/workspace`.
    pub worktree: PathBuf,
    /// The worktree's own git directory (`<common>/worktrees/<name>`).
    pub git_dir: Option<PathBuf>,
    /// The repository's common git directory (`.git`).
    pub common_dir: Option<PathBuf>,
    /// Host scratch home mounted read-write at `/tmp/home` for this exec.
    pub scratch_home: Option<PathBuf>,
    /// The project whose cgroup and limits the exec shares.
    pub project: Option<String>,
    /// Limits for the project cgroup; only valid together with a project.
    pub limits: Option<ProjectLimits>,
    /// Size cap of the private `/tmp` tmpfs, in bytes; `None` uses the default.
    pub tmp_bytes: Option<u64>,
}

impl ExecScope {
    /// A scope with the given access and worktree and no git metadata.
    pub fn new(access: ScopeAccess, worktree: impl Into<PathBuf>) -> Self {
        Self {
            access,
            font_access: None,
            font_project: None,
            worktree: worktree.into(),
            git_dir: None,
            common_dir: None,
            scratch_home: None,
            project: None,
            limits: None,
            tmp_bytes: None,
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

    /// Set the scratch home mounted at `/tmp/home` inside this scoped exec.
    pub fn with_scratch_home(mut self, scratch_home: impl Into<PathBuf>) -> Self {
        self.scratch_home = Some(scratch_home.into());
        self
    }

    /// Set access to the lab's project directory at `/font`.
    pub fn with_font_access(mut self, access: ScopeAccess) -> Self {
        self.font_access = Some(access);
        self
    }

    /// Limit the font mount to one project's directory.
    pub fn with_font_project(mut self, project: impl Into<String>) -> Self {
        self.font_project = Some(project.into());
        self
    }

    /// Share the cgroup of the named project.
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Set the resource limits of the project cgroup.
    pub fn with_limits(mut self, limits: ProjectLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Set the size cap of the private `/tmp` tmpfs, in bytes.
    pub fn with_tmp_bytes(mut self, tmp_bytes: u64) -> Self {
        self.tmp_bytes = Some(tmp_bytes);
        self
    }

    /// The private `/tmp` size in bytes, using the default when unset.
    pub fn effective_tmp_bytes(&self) -> u64 {
        self.tmp_bytes.unwrap_or(DEFAULT_SCOPED_TMP_BYTES)
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
        if let Some(scratch_home) = &self.scratch_home {
            validate_scope_path(scratch_home, "scope scratch home")?;
            if scratch_home.starts_with(&self.worktree) || self.worktree.starts_with(scratch_home) {
                return Err(scope_error(
                    "scope scratch home must be separate from the worktree".to_string(),
                ));
            }
        }
        match &self.project {
            Some(project) => validate_project_name(project)?,
            None if self.limits.is_some() => {
                return Err(scope_error(
                    "project limits require a project name".to_string(),
                ));
            }
            None => {}
        }
        if let Some(font_project) = &self.font_project {
            validate_project_name(font_project)?;
            if self.font_access.is_none() {
                return Err(scope_error(
                    "a font project requires font access".to_string(),
                ));
            }
            if self
                .project
                .as_deref()
                .is_some_and(|project| project != font_project)
            {
                return Err(scope_error(
                    "font project must match the scoped project".to_string(),
                ));
            }
        }
        if let Some(limits) = &self.limits {
            validate_project_limits(limits)?;
        }
        if self.effective_tmp_bytes() < MIN_DISK_BYTES {
            return Err(scope_error(format!(
                "scoped tmp_bytes {} is below the {MIN_DISK_BYTES} byte minimum",
                self.effective_tmp_bytes()
            )));
        }
        Ok(())
    }

    /// Reject a private `/tmp` larger than the lab-wide `/tmp` cap.
    ///
    /// The lab cap lives in the sandbox spec, which a scope cannot see on its
    /// own, so the exec path calls this with the cap of the sandbox it enters.
    pub(crate) fn validate_tmp_within_lab(&self, lab_tmp_bytes: u64) -> Result<()> {
        let tmp_bytes = self.effective_tmp_bytes();
        if tmp_bytes > lab_tmp_bytes {
            return Err(scope_error(format!(
                "scoped tmp_bytes {tmp_bytes} exceeds the lab tmp_bytes {lab_tmp_bytes}"
            )));
        }
        Ok(())
    }
}

/// A project name is used as a cgroup directory name: ASCII letters, digits,
/// `-` and `_`, at most 64 of them, and never the relative names `.` or `..`.
pub(crate) fn validate_project_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || name == "." || name == ".." {
        return Err(scope_error(format!(
            "project name {name:?} must be 1 to 64 characters and not '.' or '..'"
        )));
    }
    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err(scope_error(format!(
            "project name {name:?} may only contain letters, digits, '-' and '_'"
        )));
    }
    Ok(())
}

fn validate_project_limits(limits: &ProjectLimits) -> Result<()> {
    if limits.cpu_percent == Some(0) {
        return Err(scope_error(
            "project cpu_percent must be greater than zero".to_string(),
        ));
    }
    if limits.memory_bytes == Some(0) {
        return Err(scope_error(
            "project memory_bytes must be greater than zero".to_string(),
        ));
    }
    if limits.pids == Some(0) {
        return Err(scope_error(
            "project pids must be greater than zero".to_string(),
        ));
    }
    Ok(())
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

    #[test]
    fn scratch_home_is_confined_to_the_lab_and_separate_from_worktree() {
        let good =
            scope("/lab/projects/a/worktrees/task").with_scratch_home("/lab/scratch/a/worker");
        assert!(good.validate().is_ok());

        for path in ["/etc/scratch", "/lab/../scratch"] {
            assert!(
                scope("/lab/projects/a/worktrees/task")
                    .with_scratch_home(path)
                    .validate()
                    .is_err(),
                "{path} must be rejected"
            );
        }
        assert!(
            scope("/lab/projects/a/worktrees/task")
                .with_scratch_home("/lab/projects/a/worktrees/task/.scratch")
                .validate()
                .is_err()
        );
    }

    #[test]
    fn project_names_are_checked() {
        for name in ["a", "project-1", "a_b", "A9"] {
            assert!(
                scope("/lab/a").with_project(name).validate().is_ok(),
                "{name} must be accepted"
            );
        }
        for name in ["", ".", "..", "a/b", "a b", &"x".repeat(65)] {
            assert!(
                scope("/lab/a").with_project(name).validate().is_err(),
                "{name:?} must be rejected"
            );
        }
    }

    #[test]
    fn limits_need_a_project_and_must_be_non_zero() {
        let no_project = scope("/lab/a").with_limits(ProjectLimits {
            pids: Some(10),
            ..ProjectLimits::default()
        });
        assert!(no_project.validate().is_err());

        for limits in [
            ProjectLimits {
                cpu_percent: Some(0),
                ..ProjectLimits::default()
            },
            ProjectLimits {
                memory_bytes: Some(0),
                ..ProjectLimits::default()
            },
            ProjectLimits {
                pids: Some(0),
                ..ProjectLimits::default()
            },
        ] {
            let scoped = scope("/lab/a").with_project("a").with_limits(limits);
            assert!(scoped.validate().is_err(), "{limits:?} must be rejected");
        }

        let good = scope("/lab/a")
            .with_project("a")
            .with_limits(ProjectLimits {
                cpu_percent: Some(50),
                memory_bytes: Some(1024),
                pids: Some(10),
            });
        assert!(good.validate().is_ok());
    }

    #[test]
    fn tmp_bytes_use_the_default_and_must_be_sane() {
        let default = scope("/lab/a");
        assert_eq!(default.tmp_bytes, None);
        assert_eq!(default.effective_tmp_bytes(), DEFAULT_SCOPED_TMP_BYTES);
        assert_eq!(DEFAULT_SCOPED_TMP_BYTES, 256 * 1024 * 1024);

        for bytes in [0, 512 * 1024] {
            let bad = scope("/lab/a").with_tmp_bytes(bytes);
            assert!(
                bad.validate().is_err(),
                "tmp_bytes {bytes} must be rejected"
            );
        }
        let good = scope("/lab/a").with_tmp_bytes(MIN_DISK_BYTES);
        assert!(good.validate().is_ok());
        assert_eq!(good.effective_tmp_bytes(), MIN_DISK_BYTES);

        assert!(good.validate_tmp_within_lab(MIN_DISK_BYTES).is_ok());
        assert!(good.validate_tmp_within_lab(MIN_DISK_BYTES - 1).is_err());
    }
}
