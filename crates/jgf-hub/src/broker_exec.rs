//! Execution half of the GitHub credential broker.
//!
//! The policy lives in [`crate::broker`]: it decides whether an action may happen and never
//! touches a token or a process. This module performs an *approved* action. The token is read
//! from the lab-level [`SecretStore`] only after [`Broker::check`] allowed the request, and it
//! reaches the child process only through `GH_TOKEN`. No error, audit event or captured output
//! ever carries the token value.
//!
//! [`GhCliBackend`] runs `gh`/`git` with a fixed argument vector built from the validated
//! fields of [`BackendOp`]. There is no variant that runs an arbitrary command, no shell is
//! involved, and the child environment is cleared down to `PATH` plus the locked-down git
//! settings. Every commit not already in the remote base branch is checked under that lockdown
//! before anything is pushed, and a push never touches the work tree's git configuration or
//! hooks: the branch and the remote base are fetched into a host-owned bare mirror, and the push
//! leaves from that mirror with the token in its environment.

use std::{
    env,
    fs::{self, DirBuilder},
    io::{self, Read},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use jgf_core::{
    AuditKind, GithubAction, ModelError, ProjectId, Secret, SecretKey, SecretScope, SecretStore,
};
use thiserror::Error;

use crate::broker::{
    Approved, Broker, BrokerDenied, GithubRequest, action_name, clean_pr_text,
    contains_attribution, validate_commit_message,
};

/// Key of the GitHub token in the lab-level secret store.
pub const GITHUB_TOKEN_KEY: &str = "GITHUB_TOKEN";

/// Environment variables a backend child may inherit besides the token.
const CHILD_ENV_KEYS: [&str; 1] = ["PATH"];

/// Git settings every backend child gets, so host or repository configuration cannot change
/// what the process runs.
const GIT_LOCKDOWN_ENV: [(&str, &str); 9] = [
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_GRAFT_FILE", "/dev/null"),
    ("GIT_CONFIG_COUNT", "2"),
    ("GIT_CONFIG_KEY_0", "core.hooksPath"),
    ("GIT_CONFIG_VALUE_0", "/dev/null"),
    ("GIT_CONFIG_KEY_1", "core.fsmonitor"),
    ("GIT_CONFIG_VALUE_1", "false"),
];

/// Git configuration passed on the command line, ahead of any repository-local value.
const GIT_LOCKDOWN_ARGS: [&str; 4] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
];

/// Number of characters of a commit hash used in errors.
const SHORT_HASH_LEN: usize = 7;

/// Maximum length of a refusal reason stored in the audit log.
const AUDIT_REASON_MAX_CHARS: usize = 256;

/// Default time a backend child may run before it is killed.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a running child is polled for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// One action an approved request asks the backend to perform.
///
/// Every field has already passed the broker's name and branch validation. There is
/// deliberately no variant that accepts a raw command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendOp {
    /// Create `owner/repo`.
    CreateRepo { owner: String, repo: String },
    /// Push `branch` from the backend's work tree, based on `base`.
    PushTaskBranch {
        owner: String,
        repo: String,
        branch: String,
        base: String,
        commit_message: Option<String>,
    },
    /// Open a pull request from `head` against `base`.
    OpenPr {
        owner: String,
        repo: String,
        head: Option<String>,
        base: Option<String>,
        title: String,
        body: String,
    },
    /// Merge a pull request identified by `branch` into `merge_target`.
    MergePr {
        owner: String,
        repo: String,
        branch: Option<String>,
        merge_target: String,
    },
    /// Read the latest CI run of `branch`, or of the repository when no branch is known.
    ReadCiStatus {
        owner: String,
        repo: String,
        branch: Option<String>,
    },
}

impl BackendOp {
    /// The GitHub action this operation performs.
    pub fn action(&self) -> GithubAction {
        match self {
            Self::CreateRepo { .. } => GithubAction::CreateRepo,
            Self::PushTaskBranch { .. } => GithubAction::PushTaskBranch,
            Self::OpenPr { .. } => GithubAction::OpenPr,
            Self::MergePr { .. } => GithubAction::MergePr,
            Self::ReadCiStatus { .. } => GithubAction::ReadCiStatus,
        }
    }

    /// Builds the operation for an approved action.
    fn from_approved(approved: &Approved) -> Self {
        let owner = approved.owner.clone();
        let repo = approved.repo.clone();
        match approved.action {
            GithubAction::CreateRepo => Self::CreateRepo { owner, repo },
            GithubAction::PushTaskBranch => Self::PushTaskBranch {
                owner,
                repo,
                branch: approved.branch.clone().unwrap_or_default(),
                base: approved.base.clone().unwrap_or_default(),
                commit_message: approved.commit_message.clone(),
            },
            GithubAction::OpenPr => {
                let (title, body) = pr_text(&approved.commit_message, approved.branch.as_deref());
                Self::OpenPr {
                    owner,
                    repo,
                    head: approved.branch.clone(),
                    base: approved.base.clone(),
                    title,
                    body,
                }
            }
            GithubAction::MergePr => Self::MergePr {
                owner,
                repo,
                branch: approved.branch.clone(),
                merge_target: approved.merge_target.clone().unwrap_or_default(),
            },
            GithubAction::ReadCiStatus => Self::ReadCiStatus {
                owner,
                repo,
                branch: approved.branch.clone(),
            },
        }
    }
}

/// What a backend produced for an approved operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendOutput {
    /// Action that ran.
    pub action: GithubAction,
    /// Captured standard output, with the token redacted.
    pub stdout: String,
    /// Captured standard error, with the token redacted.
    pub stderr: String,
}

/// Why a backend could not complete an operation.
///
/// Every string field is redacted before it is stored, so neither [`std::fmt::Display`] nor
/// [`std::fmt::Debug`] can expose the token.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BackendError {
    /// The child process could not be started.
    #[error("could not start {program}: {message}")]
    Spawn {
        /// Program that failed to start.
        program: String,
        /// Operating-system message.
        message: String,
    },

    /// The child did not finish within the configured timeout and was killed.
    #[error("{program} timed out after {millis} ms")]
    Timeout {
        /// Program that timed out.
        program: String,
        /// Configured timeout in milliseconds.
        millis: u64,
    },

    /// A commit in `base..branch` does not follow the commit rules.
    #[error("commit {commit} does not follow the commit rules: {reason}")]
    CommitRejected {
        /// Short hash of the rejected commit.
        commit: String,
        /// Why the commit was rejected, with the token redacted.
        reason: String,
    },

    /// The commit list of the branch could not be read.
    #[error("could not read the commits of the pushed branch: {message}")]
    CommitLog {
        /// Why the list could not be read, with the token redacted.
        message: String,
    },

    /// The remote base branch could not be fetched into the mirror.
    #[error("could not fetch base branch {base}: {message}")]
    BaseFetch {
        /// Base branch that could not be fetched.
        base: String,
        /// Why the fetch failed, with the token redacted.
        message: String,
    },

    /// An operation is missing a required field.
    #[error("{action} requires {field}")]
    MissingOperationField {
        /// Operation that is missing a field.
        action: &'static str,
        /// Required field name.
        field: &'static str,
    },

    /// The pull request's real base branch could not be read.
    #[error("could not read the base branch of pull request {branch}: {message}")]
    MergeBaseUnreadable {
        /// Branch that identifies the pull request.
        branch: String,
        /// Why the base could not be read, with the token redacted.
        message: String,
    },

    /// The pull request merges into a branch the broker did not approve.
    #[error("pull request {branch} targets {actual:?}, not the approved merge target {expected:?}")]
    MergeTargetMismatch {
        /// Branch that identifies the pull request.
        branch: String,
        /// Branch the broker approved.
        expected: String,
        /// Branch the pull request really targets.
        actual: String,
    },

    /// The host-owned push workspace could not be prepared.
    #[error("could not prepare the push workspace: {message}")]
    Workspace {
        /// Operating-system or git message, with the token redacted.
        message: String,
    },

    /// The child finished with a non-zero status.
    #[error("{program} exited with {status}: {stderr}")]
    Failed {
        /// Program that failed.
        program: String,
        /// Exit status, as printed by the operating system.
        status: String,
        /// Captured standard output, redacted.
        stdout: String,
        /// Captured standard error, redacted.
        stderr: String,
    },

    /// Waiting for the child failed.
    #[error("could not wait for {program}: {message}")]
    Wait {
        /// Program that could not be waited for.
        program: String,
        /// Operating-system message.
        message: String,
    },
}

impl BackendError {
    /// Removes the token from every message returned by a backend implementation.
    fn redact_token(self, token: &Secret) -> Self {
        match self {
            Self::Spawn { program, message } => Self::Spawn {
                program: redact(&program, token),
                message: redact(&message, token),
            },
            Self::Timeout { program, millis } => Self::Timeout {
                program: redact(&program, token),
                millis,
            },
            Self::CommitRejected { commit, reason } => Self::CommitRejected {
                commit: redact(&commit, token),
                reason: redact(&reason, token),
            },
            Self::CommitLog { message } => Self::CommitLog {
                message: redact(&message, token),
            },
            Self::BaseFetch { base, message } => Self::BaseFetch {
                base: redact(&base, token),
                message: redact(&message, token),
            },
            Self::Workspace { message } => Self::Workspace {
                message: redact(&message, token),
            },
            Self::Failed {
                program,
                status,
                stdout,
                stderr,
            } => Self::Failed {
                program: redact(&program, token),
                status: redact(&status, token),
                stdout: redact(&stdout, token),
                stderr: redact(&stderr, token),
            },
            Self::Wait { program, message } => Self::Wait {
                program: redact(&program, token),
                message: redact(&message, token),
            },
            Self::MergeBaseUnreadable { branch, message } => Self::MergeBaseUnreadable {
                branch: redact(&branch, token),
                message: redact(&message, token),
            },
            Self::MergeTargetMismatch {
                branch,
                expected,
                actual,
            } => Self::MergeTargetMismatch {
                branch: redact(&branch, token),
                expected: redact(&expected, token),
                actual: redact(&actual, token),
            },
            missing @ Self::MissingOperationField { .. } => missing,
        }
    }
}

/// The host-side process runner an approved request is handed to.
pub trait GithubBackend {
    /// Runs `op` with `token` and returns its redacted output.
    fn run(&self, op: &BackendOp, token: &Secret) -> Result<BackendOutput, BackendError>;
}

/// Runs allowed operations with the `gh` and `git` command-line tools.
#[derive(Debug)]
pub struct GhCliBackend {
    workdir: PathBuf,
    timeout: Duration,
    #[cfg(test)]
    override_invocation: Option<Invocation>,
    #[cfg(test)]
    override_merge_base: Option<Invocation>,
    #[cfg(test)]
    remote_url: Option<String>,
}

impl GhCliBackend {
    /// A backend whose `git` commands run inside `workdir`.
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
            timeout: DEFAULT_TIMEOUT,
            #[cfg(test)]
            override_invocation: None,
            #[cfg(test)]
            override_merge_base: None,
            #[cfg(test)]
            remote_url: None,
        }
    }

    /// Uses `timeout` for every child process instead of the default minute.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Points pushes at a local destination instead of GitHub; used by the process tests.
    #[cfg(test)]
    fn with_remote_url(mut self, url: impl Into<String>) -> Self {
        self.remote_url = Some(url.into());
        self
    }

    /// The fixed command a non-push operation maps to.
    ///
    /// Pushes are prepared from a host-owned mirror in [`Self::push_task_branch`], so they
    /// have no single invocation.
    fn invocation(&self, op: &BackendOp) -> Result<Option<Invocation>, BackendError> {
        #[cfg(test)]
        if let Some(invocation) = &self.override_invocation {
            return Ok(Some(invocation.clone()));
        }
        self.invocation_for(op)
    }

    fn invocation_for(&self, op: &BackendOp) -> Result<Option<Invocation>, BackendError> {
        let invocation = match op {
            BackendOp::CreateRepo { owner, repo } => Invocation::new(
                "gh",
                vec![
                    "repo".to_string(),
                    "create".to_string(),
                    format!("{owner}/{repo}"),
                    "--private".to_string(),
                ],
            ),
            BackendOp::PushTaskBranch { .. } => return Ok(None),
            BackendOp::OpenPr {
                owner,
                repo,
                head,
                base,
                title,
                body,
            } => {
                let head = head.as_deref().ok_or(BackendError::MissingOperationField {
                    action: "open_pr",
                    field: "head",
                })?;
                let base = base.as_deref().ok_or(BackendError::MissingOperationField {
                    action: "open_pr",
                    field: "base",
                })?;
                let mut args = vec![
                    "pr".to_string(),
                    "create".to_string(),
                    "--repo".to_string(),
                    format!("{owner}/{repo}"),
                    "--head".to_string(),
                    head.to_string(),
                    "--base".to_string(),
                    base.to_string(),
                ];
                args.extend([
                    "--title".to_string(),
                    title.clone(),
                    "--body".to_string(),
                    body.clone(),
                ]);
                Invocation::new("gh", args)
            }
            BackendOp::MergePr {
                owner,
                repo,
                branch,
                ..
            } => {
                let branch = branch
                    .as_deref()
                    .ok_or(BackendError::MissingOperationField {
                        action: "merge_pr",
                        field: "branch",
                    })?;
                let mut args = vec!["pr".to_string(), "merge".to_string(), branch.to_string()];
                args.extend([
                    "--repo".to_string(),
                    format!("{owner}/{repo}"),
                    "--merge".to_string(),
                ]);
                Invocation::new("gh", args)
            }
            BackendOp::ReadCiStatus {
                owner,
                repo,
                branch,
            } => {
                let mut args = vec![
                    "run".to_string(),
                    "list".to_string(),
                    "--repo".to_string(),
                    format!("{owner}/{repo}"),
                ];
                if let Some(branch) = branch {
                    args.push("--branch".to_string());
                    args.push(branch.clone());
                }
                args.extend(["--limit".to_string(), "1".to_string()]);
                Invocation::new("gh", args)
            }
        };
        Ok(Some(invocation))
    }

    /// The `gh` command that reads the pull request's real base branch.
    fn lookup_base_invocation(&self, owner: &str, repo: &str, branch: &str) -> Invocation {
        #[cfg(test)]
        if let Some(invocation) = &self.override_merge_base {
            return invocation.clone();
        }
        base_lookup_invocation(owner, repo, branch)
    }

    /// Refuses the merge unless the pull request really targets the approved branch.
    fn verify_merge_base(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        expected: &str,
        token: &Secret,
    ) -> Result<(), BackendError> {
        let invocation = self.lookup_base_invocation(owner, repo, branch);
        let neutral = TempDir::new().map_err(|source| BackendError::Workspace {
            message: source.to_string(),
        })?;
        let (stdout, _stderr) = self.run_command(&invocation, neutral.path(), Some(token))?;
        parse_merge_base(branch, expected, &stdout)
    }

    /// Runs the fixed `gh` invocation of a non-push operation.
    fn run_fixed_gh(
        &self,
        op: &BackendOp,
        token: &Secret,
    ) -> Result<(String, String), BackendError> {
        let invocation = self
            .invocation(op)?
            .ok_or(BackendError::MissingOperationField {
                action: "non-push operation",
                field: "command",
            })?;
        let neutral = TempDir::new().map_err(|source| BackendError::Workspace {
            message: source.to_string(),
        })?;
        self.run_command(&invocation, neutral.path(), Some(token))
    }

    /// The validated push destination; only tests point it somewhere else.
    fn remote_url(&self, owner: &str, repo: &str) -> String {
        #[cfg(test)]
        if let Some(url) = &self.remote_url {
            return url.clone();
        }
        format!("https://github.com/{owner}/{repo}.git")
    }

    /// Fetches the branch into a host-owned mirror, checks its commits and pushes it.
    fn push_task_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        base: &str,
        token: &Secret,
    ) -> Result<(String, String), BackendError> {
        if base.is_empty() {
            return Err(BackendError::MissingOperationField {
                action: "push_task_branch",
                field: "base",
            });
        }
        let workdir =
            fs::canonicalize(&self.workdir).map_err(|source| BackendError::Workspace {
                message: redact(&format!("{}: {source}", self.workdir.display()), token),
            })?;

        let url = self.remote_url(owner, repo);
        let gh = if url.starts_with("https://") {
            Some(resolve_program("gh").ok_or_else(|| BackendError::Spawn {
                program: "gh".to_string(),
                message: "gh is not on PATH".to_string(),
            })?)
        } else {
            None
        };

        let mirror = TempRepo::new().map_err(|source| BackendError::Workspace {
            message: source.to_string(),
        })?;
        self.prepare_mirror(mirror.path(), &workdir, branch)?;
        self.fetch_base(mirror.path(), gh.as_deref(), &url, base, token)?;
        let sha = self.check_commits(mirror.path(), branch, base, token)?;

        let invocation = push_invocation(mirror.path(), gh.as_deref(), &url, &sha, branch);
        self.run_command(&invocation, mirror.path(), Some(token))
    }

    /// Creates a bare mirror and fetches only the pushed branch into it.
    fn prepare_mirror(
        &self,
        mirror: &Path,
        workdir: &Path,
        branch: &str,
    ) -> Result<(), BackendError> {
        let parent = mirror.parent().unwrap_or_else(|| Path::new("."));
        let init = git_invocation(vec![
            "init".to_string(),
            "--bare".to_string(),
            "--quiet".to_string(),
            mirror.display().to_string(),
        ]);
        self.run_command(&init, parent, None)?;

        let fetch = git_invocation(vec![
            "--git-dir".to_string(),
            mirror.display().to_string(),
            "fetch".to_string(),
            "--quiet".to_string(),
            "--no-tags".to_string(),
            workdir.display().to_string(),
            format!("refs/heads/{branch}:refs/heads/{branch}"),
        ]);
        self.run_command(&fetch, mirror, None)?;
        if mirror.join("shallow").exists() {
            return Err(BackendError::CommitLog {
                message: "cannot validate commits from a shallow worktree".to_string(),
            });
        }
        Ok(())
    }

    /// Fetches the remote base into the mirror, under the same git lockdown as the push.
    fn fetch_base(
        &self,
        mirror: &Path,
        gh: Option<&Path>,
        url: &str,
        base: &str,
        token: &Secret,
    ) -> Result<(), BackendError> {
        let fetch = fetch_base_invocation(mirror, gh, url, base);
        self.run_command(&fetch, mirror, Some(token))
            .map(|_| ())
            .map_err(|source| BackendError::BaseFetch {
                base: base.to_string(),
                message: source.to_string(),
            })
    }

    /// Resolves the fetched branch only from the host-owned mirror.
    fn resolve_branch_sha(&self, mirror: &Path, branch: &str) -> Result<String, BackendError> {
        let resolve = git_invocation(vec![
            "--git-dir".to_string(),
            mirror.display().to_string(),
            "rev-parse".to_string(),
            "--verify".to_string(),
            format!("refs/heads/{branch}^{{commit}}"),
        ]);
        let (resolved, _) = self.run_command(&resolve, mirror, None)?;
        let sha = resolved.trim();
        if !matches!(sha.len(), 40 | 64) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(BackendError::CommitLog {
                message: format!("unexpected branch object {sha:?}"),
            });
        }
        Ok(sha.to_string())
    }

    /// Validates all commits on the fetched branch not already in the remote base.
    fn check_commits(
        &self,
        mirror: &Path,
        branch: &str,
        base: &str,
        token: &Secret,
    ) -> Result<String, BackendError> {
        let sha = self.resolve_branch_sha(mirror, branch)?;

        let log = git_invocation(vec![
            "--git-dir".to_string(),
            mirror.display().to_string(),
            "--no-replace-objects".to_string(),
            "log".to_string(),
            "--no-show-signature".to_string(),
            "--format=%H%x00%B%x00".to_string(),
            sha.clone(),
            "--not".to_string(),
            format!("refs/remotes/origin/{base}"),
        ]);
        let (stdout, _) = self.run_command(&log, mirror, None)?;

        for (hash, message) in parse_commit_log(&stdout, token)? {
            let reason = match validate_commit_message(message) {
                Ok(_) if !contains_attribution(message) => continue,
                Ok(_) => "commit message contains an attribution trailer".to_string(),
                Err(error) => error.to_string(),
            };
            return Err(BackendError::CommitRejected {
                commit: short_hash(hash),
                reason: redact(&reason, token),
            });
        }
        Ok(sha)
    }

    /// Builds the child command with a cleared environment, git lockdown and optional token.
    fn command(&self, invocation: &Invocation, workdir: &Path, token: Option<&Secret>) -> Command {
        let mut command = Command::new(&invocation.program);
        command.args(&invocation.args);
        command.current_dir(workdir);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.env_clear();
        for key in CHILD_ENV_KEYS {
            if let Some(value) = env::var_os(key) {
                command.env(key, value);
            }
        }
        for (key, value) in GIT_LOCKDOWN_ENV {
            command.env(key, value);
        }
        command.env("GIT_CEILING_DIRECTORIES", workdir);
        if let Some(token) = token {
            command.env("GH_TOKEN", token.expose());
        }
        command
    }

    /// Runs one child under the timeout and returns its redacted output.
    fn run_command(
        &self,
        invocation: &Invocation,
        workdir: &Path,
        token: Option<&Secret>,
    ) -> Result<(String, String), BackendError> {
        let mut command = self.command(invocation, workdir, token);
        let mut child = command.spawn().map_err(|source| BackendError::Spawn {
            program: invocation.program.clone(),
            message: redact_optional(&source.to_string(), token),
        })?;

        let stdout = spawn_reader(child.stdout.take());
        let stderr = spawn_reader(child.stderr.take());
        let start = Instant::now();

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if start.elapsed() >= self.timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(BackendError::Timeout {
                            program: invocation.program.clone(),
                            millis: duration_millis(self.timeout),
                        });
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                Err(source) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BackendError::Wait {
                        program: invocation.program.clone(),
                        message: redact_optional(&source.to_string(), token),
                    });
                }
            }
        };

        let remaining = self
            .timeout
            .saturating_sub(start.elapsed())
            .min(Duration::from_millis(500));
        let stdout = stdout.recv_timeout(remaining).unwrap_or_default();
        let stderr = stderr.recv_timeout(remaining).unwrap_or_default();
        let stdout = redact_optional(&stdout, token);
        let stderr = redact_optional(&stderr, token);

        if status.success() {
            Ok((stdout, stderr))
        } else {
            Err(BackendError::Failed {
                program: invocation.program.clone(),
                status: status.to_string(),
                stdout,
                stderr,
            })
        }
    }
}

impl Default for GhCliBackend {
    fn default() -> Self {
        Self::new(".")
    }
}

impl GithubBackend for GhCliBackend {
    fn run(&self, op: &BackendOp, token: &Secret) -> Result<BackendOutput, BackendError> {
        let (stdout, stderr) = match op {
            BackendOp::PushTaskBranch {
                owner,
                repo,
                branch,
                base,
                ..
            } => self.push_task_branch(owner, repo, branch, base, token)?,
            BackendOp::MergePr {
                owner,
                repo,
                branch,
                merge_target,
            } => {
                let branch = branch
                    .as_deref()
                    .ok_or(BackendError::MissingOperationField {
                        action: "merge_pr",
                        field: "branch",
                    })?;
                if merge_target.is_empty() {
                    return Err(BackendError::MissingOperationField {
                        action: "merge_pr",
                        field: "merge_target",
                    });
                }
                self.verify_merge_base(owner, repo, branch, merge_target, token)?;
                self.run_fixed_gh(op, token)?
            }
            _ => self.run_fixed_gh(op, token)?,
        };
        Ok(BackendOutput {
            action: op.action(),
            stdout,
            stderr,
        })
    }
}

/// Why an approved request could not be executed.
#[derive(Debug, Error)]
pub enum BrokerError {
    /// The broker refused the request.
    #[error(transparent)]
    Denied(Box<BrokerDenied>),

    /// The lab-level token is not configured.
    #[error("github token {} is not set", .key.as_str())]
    MissingToken {
        /// Key that was looked up.
        key: SecretKey,
    },

    /// The secret store could not be read.
    #[error("could not read github token {}: {source}", .key.as_str())]
    Store {
        /// Key that was looked up.
        key: SecretKey,
        /// Store failure.
        #[source]
        source: Box<ModelError>,
    },

    /// The backend could not complete the operation.
    #[error(transparent)]
    Backend(#[from] BackendError),

    /// Writing the audit log failed.
    #[error(transparent)]
    Audit(Box<ModelError>),
}

impl From<BrokerDenied> for BrokerError {
    fn from(denied: BrokerDenied) -> Self {
        Self::Denied(Box::new(denied))
    }
}

impl From<ModelError> for BrokerError {
    fn from(source: ModelError) -> Self {
        Self::Audit(Box::new(source))
    }
}

impl Broker {
    /// Runs an approved request, loading the lab token only after [`Broker::check`] allowed it.
    ///
    /// A denied request never reads the secret store and never calls `backend`. A missing
    /// token is a typed error, is audited, and also never reaches the backend.
    pub fn execute<B, S>(
        &self,
        req: &GithubRequest,
        project: &ProjectId,
        worker: &str,
        backend: &B,
        store: &S,
    ) -> Result<BackendOutput, BrokerError>
    where
        B: GithubBackend + ?Sized,
        S: SecretStore + ?Sized,
    {
        let approved = self.check(req, project, worker)?;
        let key = token_key();
        let token = match store.get(&SecretScope::Lab, &key) {
            Ok(token) => token,
            Err(source) => {
                self.audit_refusal(req, project, worker, &source.to_string(), None)?;
                return Err(BrokerError::Store {
                    key: key.clone(),
                    source: Box::new(source),
                });
            }
        };
        let Some(token) = token else {
            self.audit_refusal(req, project, worker, "github token is not set", None)?;
            return Err(BrokerError::MissingToken { key });
        };

        let op = BackendOp::from_approved(&approved);
        match backend.run(&op, &token) {
            Ok(output) => Ok(output),
            Err(error) => {
                let error = error.redact_token(&token);
                self.audit_refusal(req, project, worker, &error.to_string(), Some(&token))?;
                Err(BrokerError::Backend(error))
            }
        }
    }

    /// Records why an allowed request could not be executed.
    fn audit_refusal(
        &self,
        req: &GithubRequest,
        project: &ProjectId,
        worker: &str,
        reason: &str,
        token: Option<&Secret>,
    ) -> Result<(), ModelError> {
        let reason = redact_optional(reason, token);
        let reason: String = reason.chars().take(AUDIT_REASON_MAX_CHARS).collect();
        self.audit()
            .append(
                Some(project.as_str()),
                Some(worker),
                AuditKind::github_action(
                    action_name(req.action),
                    &req.owner,
                    &req.repo,
                    req.branch.clone(),
                    false,
                    &reason,
                ),
            )
            .map(|_| ())
    }
}

/// The lab-level token key, built from the static constant.
fn token_key() -> SecretKey {
    SecretKey::parse(GITHUB_TOKEN_KEY).expect("the token key constant is valid")
}

/// Splits cleaned commit text into a pull-request title and body.
fn pr_text(commit_message: &Option<String>, branch: Option<&str>) -> (String, String) {
    let Some(message) = commit_message else {
        return (clean_pr_text(branch.unwrap_or("")), String::new());
    };
    let mut lines = message.splitn(2, '\n');
    let title = clean_pr_text(lines.next().unwrap_or_default());
    let body = clean_pr_text(lines.next().unwrap_or_default());
    let title = if title.is_empty() {
        clean_pr_text(branch.unwrap_or(""))
    } else {
        title
    };
    (title, body)
}

/// Replaces every occurrence of the token value with `[redacted]`.
fn redact(text: &str, token: &Secret) -> String {
    let value = token.expose();
    if value.is_empty() {
        return text.to_string();
    }
    text.replace(value, "[redacted]")
}

/// Redacts `text` when the child carried the token, else leaves it untouched.
fn redact_optional(text: &str, token: Option<&Secret>) -> String {
    match token {
        Some(token) => redact(text, token),
        None => text.to_string(),
    }
}

/// The fixed `gh` command that reads a pull request's real base branch.
fn base_lookup_invocation(owner: &str, repo: &str, branch: &str) -> Invocation {
    Invocation::new(
        "gh",
        vec![
            "pr".to_string(),
            "view".to_string(),
            branch.to_string(),
            "--repo".to_string(),
            format!("{owner}/{repo}"),
            "--json".to_string(),
            "baseRefName".to_string(),
        ],
    )
}

/// Refuses the merge unless `gh` reported exactly `expected` as the pull request's base.
///
/// An unreadable or malformed response is a refusal too: a merge is never allowed on a base
/// the broker could not confirm.
fn parse_merge_base(branch: &str, expected: &str, stdout: &str) -> Result<(), BackendError> {
    let actual =
        parse_base_ref_name(stdout).map_err(|message| BackendError::MergeBaseUnreadable {
            branch: branch.to_string(),
            message,
        })?;
    if actual != expected {
        return Err(BackendError::MergeTargetMismatch {
            branch: branch.to_string(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Reads `baseRefName` out of a `gh pr view --json baseRefName` response.
fn parse_base_ref_name(stdout: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|error| format!("invalid JSON: {error}"))?;
    value
        .get("baseRefName")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "response has no baseRefName".to_string())
}

/// A `git` invocation with the host-side lockdown flags ahead of `args`.
fn git_invocation(args: Vec<String>) -> Invocation {
    let mut locked: Vec<String> = GIT_LOCKDOWN_ARGS
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
    locked.extend(args);
    Invocation::new("git", locked)
}

/// The push from the host-owned mirror: explicit URL, explicit credential helper, no shell.
fn push_invocation(
    git_dir: &Path,
    gh: Option<&Path>,
    url: &str,
    sha: &str,
    branch: &str,
) -> Invocation {
    let mut args = vec!["--git-dir".to_string(), git_dir.display().to_string()];
    args.extend(GIT_LOCKDOWN_ARGS.iter().map(|arg| (*arg).to_string()));
    args.extend(["-c".to_string(), "credential.helper=".to_string()]);
    if let Some(gh) = gh {
        args.push("-c".to_string());
        args.push(format!(
            "credential.helper={} auth git-credential",
            gh.display()
        ));
    }
    args.extend([
        "push".to_string(),
        "--no-verify".to_string(),
        url.to_string(),
        format!("{sha}:refs/heads/{branch}"),
    ]);
    Invocation::new("git", args)
}

/// The base-branch fetch into the host-owned mirror: explicit URL, explicit credential helper.
fn fetch_base_invocation(git_dir: &Path, gh: Option<&Path>, url: &str, base: &str) -> Invocation {
    let mut args = vec!["--git-dir".to_string(), git_dir.display().to_string()];
    args.extend(GIT_LOCKDOWN_ARGS.iter().map(|arg| (*arg).to_string()));
    args.extend(["-c".to_string(), "credential.helper=".to_string()]);
    if let Some(gh) = gh {
        args.push("-c".to_string());
        args.push(format!(
            "credential.helper={} auth git-credential",
            gh.display()
        ));
    }
    args.extend([
        "fetch".to_string(),
        "--quiet".to_string(),
        "--no-tags".to_string(),
        url.to_string(),
        format!("refs/heads/{base}:refs/remotes/origin/{base}"),
    ]);
    Invocation::new("git", args)
}

/// Splits `git log --format=%H%x00%B%x00` output into commit hash and message pairs.
fn parse_commit_log<'a>(
    output: &'a str,
    token: &Secret,
) -> Result<Vec<(&'a str, &'a str)>, BackendError> {
    let mut commits = Vec::new();
    let mut fields = output.split('\0');
    while let Some(hash) = fields.next() {
        let hash = hash.trim();
        if hash.is_empty() {
            break;
        }
        if hash.len() < SHORT_HASH_LEN || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(BackendError::CommitLog {
                message: redact(&format!("unexpected commit hash {hash:?}"), token),
            });
        }
        let Some(message) = fields.next() else {
            return Err(BackendError::CommitLog {
                message: format!("commit {hash} has no message"),
            });
        };
        commits.push((hash, message));
    }
    Ok(commits)
}

/// The short form of a commit hash used in errors.
fn short_hash(hash: &str) -> String {
    hash.chars().take(SHORT_HASH_LEN).collect()
}

/// Resolves an executable on `PATH` to an absolute path, without a shell.
fn resolve_program(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

/// A host-owned temporary directory removed when it goes out of scope.
struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "jgf-hub-push-{}-{sequence}.git",
                std::process::id()
            ));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// An empty host-owned working directory for a `gh` invocation.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("jgf-hub-gh-{}-{sequence}", std::process::id()));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// One fixed child invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    program: String,
    args: Vec<String>,
}

impl Invocation {
    fn new(program: &str, args: Vec<String>) -> Self {
        Self {
            program: program.to_string(),
            args,
        }
    }
}

/// Reads a child pipe on a thread so a full pipe can never block the child.
fn spawn_reader<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            let _ = sender.send(String::from_utf8_lossy(&buffer).into_owned());
        });
    }
    receiver
}

/// A recording backend for tests and downstream checks.
pub mod testing {
    use std::sync::Mutex;

    use jgf_core::Secret;

    use super::{BackendError, BackendOp, BackendOutput, GithubBackend};

    /// One call a [`FakeBackend`] saw.
    #[derive(Debug, Clone)]
    pub struct RecordedCall {
        /// Operation the broker asked for.
        pub op: BackendOp,
        /// Token the operation carried. It is a [`Secret`], so its `Debug` is redacted.
        pub token: Secret,
    }

    /// A [`GithubBackend`] that records every operation and token instead of running anything.
    #[derive(Debug, Default)]
    pub struct FakeBackend {
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl FakeBackend {
        /// An empty recording backend.
        pub fn new() -> Self {
            Self::default()
        }

        /// Calls seen so far, in order.
        pub fn calls(&self) -> Vec<RecordedCall> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        /// Number of calls seen so far.
        pub fn call_count(&self) -> usize {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
        }
    }

    impl GithubBackend for FakeBackend {
        fn run(&self, op: &BackendOp, token: &Secret) -> Result<BackendOutput, BackendError> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(RecordedCall {
                    op: op.clone(),
                    token: token.clone(),
                });
            Ok(BackendOutput {
                action: op.action(),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }
}

/// A recording backend, also reachable as [`testing::FakeBackend`].
pub use testing::FakeBackend;

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    };

    use jgf_core::{AuditEvent, AuditLog, FileSecretStore, GithubGrant};

    use super::*;

    const TOKEN: &str = "ghp_DISTINCTIVE_TOKEN_0123456789";

    struct RefusalBackend;

    impl GithubBackend for RefusalBackend {
        fn run(&self, _op: &BackendOp, _token: &Secret) -> Result<BackendOutput, BackendError> {
            Err(BackendError::CommitRejected {
                commit: "deadbee".to_string(),
                reason: format!("{}{}", TOKEN, "x".repeat(400)),
            })
        }
    }

    /// Temporary root removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jgf-hub-broker-exec-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp root");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A store that counts token lookups and delegates to a file store.
    struct SpyStore {
        inner: FileSecretStore,
        gets: AtomicUsize,
    }

    impl SpyStore {
        fn new(root: &TempRoot) -> Self {
            Self {
                inner: FileSecretStore::new(root.path()),
                gets: AtomicUsize::new(0),
            }
        }

        fn gets(&self) -> usize {
            self.gets.load(Ordering::SeqCst)
        }
    }

    impl SecretStore for SpyStore {
        fn get(&self, scope: &SecretScope, key: &SecretKey) -> Result<Option<Secret>, ModelError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.inner.get(scope, key)
        }

        fn set(
            &self,
            scope: &SecretScope,
            key: &SecretKey,
            secret: Secret,
        ) -> Result<(), ModelError> {
            self.inner.set(scope, key, secret)
        }

        fn remove(&self, scope: &SecretScope, key: &SecretKey) -> Result<(), ModelError> {
            self.inner.remove(scope, key)
        }

        fn list_keys(&self, scope: &SecretScope) -> Result<Vec<SecretKey>, ModelError> {
            self.inner.list_keys(scope)
        }
    }

    fn project(raw: &str) -> ProjectId {
        ProjectId::parse(raw).expect("valid project id")
    }

    fn grant(actions: &[GithubAction]) -> GithubGrant {
        GithubGrant {
            owners: vec!["acme".to_string()],
            actions: actions.to_vec(),
            max_prs_per_day: 5,
            max_pushes_per_day: 5,
        }
    }

    fn all_actions() -> Vec<GithubAction> {
        vec![
            GithubAction::CreateRepo,
            GithubAction::PushTaskBranch,
            GithubAction::OpenPr,
            GithubAction::MergePr,
            GithubAction::ReadCiStatus,
        ]
    }

    fn open(root: &TempRoot, grant: GithubGrant) -> Broker {
        let audit = AuditLog::open(root.path(), "lab").expect("open audit log");
        Broker::open(root.path(), "lab", grant, audit).expect("open broker")
    }

    fn request(action: GithubAction) -> GithubRequest {
        GithubRequest {
            action,
            owner: "acme".to_string(),
            repo: "app".to_string(),
            branch: None,
            base: None,
            force: false,
            delete: false,
            target_is_default_branch: false,
            merge_target: None,
            merge_context: None,
            commit_message: None,
        }
    }

    fn push_request() -> GithubRequest {
        let mut req = request(GithubAction::PushTaskBranch);
        req.branch = Some("jgt/JGT-97".to_string());
        req.base = Some("main".to_string());
        req.commit_message = Some(
            "feat: add x\n\nCo-Authored-By: A <a@example.com>\nGenerated with a tool\n\nBody"
                .to_string(),
        );
        req
    }

    fn store_token(store: &dyn SecretStore) {
        store
            .set(&SecretScope::Lab, &token_key(), Secret::new(TOKEN))
            .expect("store token");
    }

    fn audit_text(root: &TempRoot) -> String {
        let log = AuditLog::open(root.path(), "lab").expect("open audit log");
        let events: Vec<AuditEvent> = log.read_all().expect("read audit log");
        format!("{events:?}")
    }

    #[test]
    fn a_denied_request_never_reads_the_store_or_calls_the_backend() {
        let root = TempRoot::new();
        let broker = open(&root, grant(&[GithubAction::ReadCiStatus]));
        let store = SpyStore::new(&root);
        let backend = testing::FakeBackend::new();

        let result = broker.execute(
            &request(GithubAction::CreateRepo),
            &project("alpha"),
            "worker-1",
            &backend,
            &store,
        );

        assert!(matches!(result, Err(BrokerError::Denied(_))));
        assert_eq!(store.gets(), 0, "denied requests must not touch the store");
        assert_eq!(backend.call_count(), 0, "denied requests must not run");
    }

    #[test]
    fn an_approved_request_gives_the_backend_the_token_and_never_leaks_it() {
        let root = TempRoot::new();
        let broker = open(&root, grant(&all_actions()));
        let store = SpyStore::new(&root);
        store_token(&store);
        let backend = testing::FakeBackend::new();

        let output = broker
            .execute(
                &request(GithubAction::CreateRepo),
                &project("alpha"),
                "worker-1",
                &backend,
                &store,
            )
            .expect("approved request");

        assert_eq!(output.action, GithubAction::CreateRepo);
        assert_eq!(store.gets(), 1);
        let calls = backend.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].token.expose(), TOKEN);

        assert!(!audit_text(&root).contains(TOKEN), "audit leaked the token");
    }

    #[test]
    fn a_missing_token_is_typed_audited_and_never_calls_the_backend() {
        let root = TempRoot::new();
        let broker = open(&root, grant(&all_actions()));
        let store = SpyStore::new(&root);
        let backend = testing::FakeBackend::new();

        let result = broker.execute(
            &request(GithubAction::CreateRepo),
            &project("alpha"),
            "worker-1",
            &backend,
            &store,
        );

        let error = result.expect_err("missing token must fail");
        assert!(matches!(error, BrokerError::MissingToken { .. }));
        assert!(!format!("{error:?}").contains(TOKEN));
        assert_eq!(backend.call_count(), 0);

        let log = AuditLog::open(root.path(), "lab").expect("open audit log");
        let events = log.read_all().expect("read audit log");
        let denied = events.iter().rev().find_map(|event| match &event.kind {
            AuditKind::GithubAction {
                allowed, reason, ..
            } => Some((*allowed, reason.clone())),
            _ => None,
        });
        assert_eq!(denied, Some((false, "github token is not set".to_string())));
        assert_eq!(log.verify().expect("verify missing-token audit"), None);
    }

    #[test]
    fn backend_refusals_are_redacted_capped_and_audited() {
        let root = TempRoot::new();
        let broker = open(&root, grant(&all_actions()));
        let store = SpyStore::new(&root);
        store_token(&store);
        let backend = RefusalBackend;

        let result = broker.execute(
            &request(GithubAction::CreateRepo),
            &project("alpha"),
            "worker-1",
            &backend,
            &store,
        );
        let error = result.expect_err("the backend refusal is returned");
        assert!(matches!(&error, BrokerError::Backend(_)));
        assert!(!error.to_string().contains(TOKEN));
        assert!(!format!("{error:?}").contains(TOKEN));

        let log = AuditLog::open(root.path(), "lab").expect("open audit log");
        let events = log.read_all().expect("read audit log");
        let refusal = events.iter().rev().find_map(|event| match &event.kind {
            AuditKind::GithubAction {
                allowed: false,
                reason,
                ..
            } => Some(reason),
            _ => None,
        });
        let reason = refusal.expect("backend refusal audit entry");
        assert!(!reason.contains(TOKEN));
        assert!(reason.chars().count() <= AUDIT_REASON_MAX_CHARS);
        assert_eq!(log.verify().expect("verify refusal audit"), None);
    }

    #[test]
    fn commit_message_reaches_the_backend_cleaned() {
        let root = TempRoot::new();
        let broker = open(&root, grant(&all_actions()));
        let store = SpyStore::new(&root);
        store_token(&store);
        let backend = testing::FakeBackend::new();

        broker
            .execute(
                &push_request(),
                &project("alpha"),
                "worker-1",
                &backend,
                &store,
            )
            .expect("approved push");

        let calls = backend.calls();
        let BackendOp::PushTaskBranch {
            commit_message,
            branch,
            ..
        } = &calls[0].op
        else {
            panic!("expected a push operation");
        };
        assert_eq!(branch, "jgt/JGT-97");
        assert_eq!(commit_message.as_deref(), Some("feat: add x\n\nBody"));
    }

    #[test]
    fn pull_request_text_is_cleaned_before_the_backend() {
        let approved = Approved {
            action: GithubAction::OpenPr,
            owner: "acme".to_string(),
            repo: "app".to_string(),
            branch: Some("jgt/JGT-97".to_string()),
            base: Some("develop".to_string()),
            merge_target: None,
            commit_message: Some(
                "feat: add x\n\nCo-Authored-By: A <a@example.com>\nBody prose.".to_string(),
            ),
        };

        let op = BackendOp::from_approved(&approved);
        let BackendOp::OpenPr { title, body, .. } = op else {
            panic!("expected an open-pr operation");
        };
        assert_eq!(title, "feat: add x");
        assert_eq!(body, "Body prose.");
    }

    fn invocation(op: &BackendOp) -> Invocation {
        GhCliBackend::new("/work/tree")
            .invocation(op)
            .expect("valid non-push operation")
            .expect("non-push operations have a fixed command")
    }

    fn op_with_token() -> (GhCliBackend, Invocation, Secret) {
        let backend = GhCliBackend::new("/work/tree");
        let op = BackendOp::CreateRepo {
            owner: "acme".to_string(),
            repo: "app".to_string(),
        };
        let invocation = backend
            .invocation(&op)
            .expect("valid create operation")
            .expect("create repo has a fixed command");
        (backend, invocation, Secret::new(TOKEN))
    }

    #[test]
    fn open_pr_and_merge_pr_require_explicit_identifiers() {
        let backend = GhCliBackend::new("/work/tree");
        let open = BackendOp::OpenPr {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            head: None,
            base: Some("develop".to_string()),
            title: "title".to_string(),
            body: String::new(),
        };
        assert!(matches!(
            backend.invocation(&open),
            Err(BackendError::MissingOperationField {
                action: "open_pr",
                field: "head"
            })
        ));

        let no_base = BackendOp::OpenPr {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            head: Some("jgt/JGT-97".to_string()),
            base: None,
            title: "title".to_string(),
            body: String::new(),
        };
        assert!(matches!(
            backend.invocation(&no_base),
            Err(BackendError::MissingOperationField {
                action: "open_pr",
                field: "base"
            })
        ));

        let merge = BackendOp::MergePr {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            branch: None,
            merge_target: "develop".to_string(),
        };
        assert!(matches!(
            backend.invocation(&merge),
            Err(BackendError::MissingOperationField {
                action: "merge_pr",
                field: "branch"
            })
        ));
    }

    #[test]
    fn create_repo_has_a_fixed_argument_vector() {
        let invocation = invocation(&BackendOp::CreateRepo {
            owner: "acme".to_string(),
            repo: "app".to_string(),
        });
        assert_eq!(invocation.program, "gh");
        assert_eq!(invocation.args, ["repo", "create", "acme/app", "--private"]);
    }

    #[test]
    fn push_is_locked_down_and_has_a_fixed_argument_vector() {
        let invocation = push_invocation(
            Path::new("/host/mirror.git"),
            Some(Path::new("/usr/bin/gh")),
            "https://github.com/acme/app.git",
            "0123456789abcdef0123456789abcdef01234567",
            "jgt/JGT-97",
        );
        assert_eq!(invocation.program, "git");
        assert_eq!(
            invocation.args,
            [
                "--git-dir",
                "/host/mirror.git",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "credential.helper=",
                "-c",
                "credential.helper=/usr/bin/gh auth git-credential",
                "push",
                "--no-verify",
                "https://github.com/acme/app.git",
                "0123456789abcdef0123456789abcdef01234567:refs/heads/jgt/JGT-97",
            ]
        );
    }

    #[test]
    fn open_pr_has_a_fixed_argument_vector() {
        let invocation = invocation(&BackendOp::OpenPr {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            head: Some("jgt/JGT-97".to_string()),
            base: Some("develop".to_string()),
            title: "feat: add x".to_string(),
            body: "Body".to_string(),
        });
        assert_eq!(invocation.program, "gh");
        assert_eq!(
            invocation.args,
            [
                "pr",
                "create",
                "--repo",
                "acme/app",
                "--head",
                "jgt/JGT-97",
                "--base",
                "develop",
                "--title",
                "feat: add x",
                "--body",
                "Body",
            ]
        );
    }

    #[test]
    fn merge_pr_has_a_fixed_argument_vector() {
        let invocation = invocation(&BackendOp::MergePr {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            branch: Some("jgt/JGT-97".to_string()),
            merge_target: "develop".to_string(),
        });
        assert_eq!(invocation.program, "gh");
        assert_eq!(
            invocation.args,
            ["pr", "merge", "jgt/JGT-97", "--repo", "acme/app", "--merge"]
        );
    }

    #[test]
    fn merge_base_lookup_has_a_fixed_argument_vector() {
        let invocation =
            GhCliBackend::new("/work/tree").lookup_base_invocation("acme", "app", "jgt/JGT-97");
        assert_eq!(invocation.program, "gh");
        assert_eq!(
            invocation.args,
            [
                "pr",
                "view",
                "jgt/JGT-97",
                "--repo",
                "acme/app",
                "--json",
                "baseRefName",
            ]
        );
    }

    fn merge_op() -> BackendOp {
        BackendOp::MergePr {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            branch: Some("jgt/JGT-97".to_string()),
            merge_target: "develop".to_string(),
        }
    }

    fn shell_invocation(script: &str) -> Invocation {
        Invocation::new("sh", vec!["-c".to_string(), script.to_string()])
    }

    fn merge_backend(lookup: Invocation) -> GhCliBackend {
        GhCliBackend {
            workdir: PathBuf::from("."),
            timeout: Duration::from_secs(5),
            override_invocation: Some(shell_invocation("echo merged")),
            override_merge_base: Some(lookup),
            remote_url: None,
        }
    }

    #[test]
    fn a_merge_is_refused_when_the_real_base_differs_from_the_approved_target() {
        let backend = merge_backend(shell_invocation("echo '{\"baseRefName\":\"main\"}'"));

        let error = backend
            .run(&merge_op(), &Secret::new(TOKEN))
            .expect_err("the real base is not the approved one");

        assert!(matches!(
            error,
            BackendError::MergeTargetMismatch {
                expected,
                actual,
                ..
            } if expected == "develop" && actual == "main"
        ));
    }

    #[test]
    fn a_merge_is_refused_when_the_base_lookup_fails() {
        let backend = merge_backend(shell_invocation("exit 1"));

        let error = backend
            .run(&merge_op(), &Secret::new(TOKEN))
            .expect_err("an unreadable base must fail closed");

        assert!(matches!(error, BackendError::Failed { .. }));
    }

    #[test]
    fn a_merge_is_refused_when_the_base_response_is_garbage() {
        let backend = merge_backend(shell_invocation("echo not-json"));

        let error = backend
            .run(&merge_op(), &Secret::new(TOKEN))
            .expect_err("a malformed base response must fail closed");

        assert!(matches!(error, BackendError::MergeBaseUnreadable { .. }));
    }

    #[test]
    fn a_merge_is_allowed_when_the_real_base_matches_the_approved_target() {
        let backend = merge_backend(shell_invocation("echo '{\"baseRefName\":\"develop\"}'"));

        let output = backend
            .run(&merge_op(), &Secret::new(TOKEN))
            .expect("the real base is the approved one");

        assert_eq!(output.action, GithubAction::MergePr);
        assert!(output.stdout.contains("merged"));
    }

    #[test]
    fn read_ci_status_has_a_fixed_argument_vector() {
        let invocation = invocation(&BackendOp::ReadCiStatus {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            branch: Some("jgt/JGT-97".to_string()),
        });
        assert_eq!(invocation.program, "gh");
        assert_eq!(
            invocation.args,
            [
                "run",
                "list",
                "--repo",
                "acme/app",
                "--branch",
                "jgt/JGT-97",
                "--limit",
                "1",
            ]
        );
    }

    #[test]
    fn the_child_environment_holds_only_the_allowed_keys_and_the_token() {
        let (backend, invocation, token) = op_with_token();
        let neutral = TempDir::new().expect("create neutral directory");
        let command = backend.command(&invocation, neutral.path(), Some(&token));

        let keys: Vec<String> = command
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for key in &keys {
            let allowed = CHILD_ENV_KEYS.contains(&key.as_str())
                || GIT_LOCKDOWN_ENV.iter().any(|(name, _)| name == key)
                || key == "GIT_CEILING_DIRECTORIES"
                || key == "GH_TOKEN";
            assert!(allowed, "unexpected child environment key {key:?}");
        }
        assert_eq!(command.get_current_dir(), Some(neutral.path()));
        assert_eq!(
            fs::read_dir(neutral.path())
                .expect("read neutral directory")
                .count(),
            0
        );
        assert!(!keys.iter().any(|key| key == "HOME"));
        assert!(keys.iter().any(|key| key == "GH_TOKEN"));
        for (key, value) in GIT_LOCKDOWN_ENV {
            assert!(command.get_envs().any(|(actual, actual_value)| {
                actual == key && actual_value == Some(std::ffi::OsStr::new(value))
            }));
        }
        assert!(command.get_envs().any(|(key, value)| {
            key == "GIT_CEILING_DIRECTORIES" && value == Some(neutral.path().as_os_str())
        }));
        assert!(
            command.get_envs().any(|(key, value)| key == "GH_TOKEN"
                && value == Some(std::ffi::OsStr::new(TOKEN)))
        );
    }

    #[test]
    fn every_gh_operation_uses_the_neutral_directory_and_git_lockdown() {
        let backend = GhCliBackend::new("/work/tree");
        let token = Secret::new(TOKEN);
        let neutral = TempDir::new().expect("create neutral directory");
        let operations = [
            BackendOp::CreateRepo {
                owner: "acme".to_string(),
                repo: "app".to_string(),
            },
            BackendOp::OpenPr {
                owner: "acme".to_string(),
                repo: "app".to_string(),
                head: Some("jgt/JGT-97".to_string()),
                base: Some("develop".to_string()),
                title: "title".to_string(),
                body: String::new(),
            },
            BackendOp::MergePr {
                owner: "acme".to_string(),
                repo: "app".to_string(),
                branch: Some("jgt/JGT-97".to_string()),
                merge_target: "develop".to_string(),
            },
            BackendOp::ReadCiStatus {
                owner: "acme".to_string(),
                repo: "app".to_string(),
                branch: Some("jgt/JGT-97".to_string()),
            },
        ];

        for op in operations {
            let invocation = backend
                .invocation(&op)
                .expect("operation validates")
                .expect("gh operation has an invocation");
            if matches!(&op, BackendOp::CreateRepo { .. }) {
                assert!(
                    invocation.args.iter().any(|arg| arg == "acme/app"),
                    "create_repo lacks its positional repository"
                );
            } else {
                assert!(
                    invocation.args.iter().any(|arg| arg == "--repo"),
                    "{:?} lacks --repo",
                    op.action()
                );
            }
            let command = backend.command(&invocation, neutral.path(), Some(&token));
            assert_eq!(command.get_current_dir(), Some(neutral.path()));
            for (key, value) in GIT_LOCKDOWN_ENV {
                assert!(command.get_envs().any(|(actual, actual_value)| {
                    actual == key && actual_value == Some(std::ffi::OsStr::new(value))
                }));
            }
            assert!(command.get_envs().any(|(key, value)| {
                key == "GIT_CEILING_DIRECTORIES" && value == Some(neutral.path().as_os_str())
            }));
        }
    }

    #[test]
    fn a_child_without_a_token_gets_none_in_its_environment() {
        let (backend, invocation, _) = op_with_token();
        let command = backend.command(&invocation, Path::new("/work/tree"), None);

        assert!(!command.get_envs().any(|(key, _)| key == "GH_TOKEN"));
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "GIT_CONFIG_GLOBAL"
                    && value == Some(std::ffi::OsStr::new("/dev/null")))
        );
    }

    #[test]
    fn captured_output_is_redacted() {
        let backend = GhCliBackend {
            workdir: PathBuf::from("."),
            timeout: Duration::from_secs(5),
            override_invocation: Some(Invocation::new("printenv", vec!["GH_TOKEN".to_string()])),
            override_merge_base: None,
            remote_url: None,
        };
        let token = Secret::new(TOKEN);

        let output = backend
            .run(
                &BackendOp::CreateRepo {
                    owner: "acme".to_string(),
                    repo: "app".to_string(),
                },
                &token,
            )
            .expect("printenv succeeds");

        assert!(!output.stdout.contains(TOKEN));
        assert!(output.stdout.contains("[redacted]"));
    }

    #[test]
    fn a_failed_child_is_redacted_in_display_and_debug() {
        let backend = GhCliBackend {
            workdir: PathBuf::from("."),
            timeout: Duration::from_secs(5),
            override_invocation: Some(Invocation::new(
                "sh",
                vec!["-c".to_string(), "printenv GH_TOKEN; exit 7".to_string()],
            )),
            override_merge_base: None,
            remote_url: None,
        };
        let token = Secret::new(TOKEN);

        let error = backend
            .run(
                &BackendOp::CreateRepo {
                    owner: "acme".to_string(),
                    repo: "app".to_string(),
                },
                &token,
            )
            .expect_err("the child exits non-zero");

        assert!(matches!(error, BackendError::Failed { .. }));
        assert!(!error.to_string().contains(TOKEN));
        assert!(!format!("{error:?}").contains(TOKEN));
    }

    #[test]
    fn a_child_that_outlives_the_timeout_is_killed() {
        let backend = GhCliBackend {
            workdir: PathBuf::from("."),
            timeout: Duration::from_millis(100),
            override_invocation: Some(Invocation::new("sleep", vec!["5".to_string()])),
            override_merge_base: None,
            remote_url: None,
        };
        let token = Secret::new(TOKEN);

        let start = Instant::now();
        let error = backend
            .run(
                &BackendOp::CreateRepo {
                    owner: "acme".to_string(),
                    repo: "app".to_string(),
                },
                &token,
            )
            .expect_err("sleep must time out");

        assert!(matches!(error, BackendError::Timeout { .. }));
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    /// A work repository on `main` with one clean base commit.
    fn repo_with_base(root: &TempRoot) -> PathBuf {
        let work = root.path().join("work");
        fs::create_dir_all(&work).expect("create work tree");
        git_ok(&work, &["-c", "init.defaultBranch=main", "init", "-q", "."]);
        fs::write(work.join("base.txt"), "base").expect("write base file");
        git_ok(&work, &["add", "base.txt"]);
        git_ok(
            &work,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                "feat: base",
            ],
        );
        work
    }

    /// Commits the staged changes with `message` as the whole commit message.
    fn commit_staged(work: &Path, message: &str) {
        git_ok(
            work,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
    }

    /// Cuts `jgt/T1` from `main` and commits with the given `commit` message arguments.
    fn commit_on_task_branch(work: &Path, message_args: &[&str]) {
        git_ok(work, &["checkout", "-q", "-b", "jgt/T1"]);
        fs::write(work.join("task.txt"), "task").expect("write task file");
        git_ok(work, &["add", "task.txt"]);
        let mut args = vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-q",
        ];
        args.extend_from_slice(message_args);
        git_ok(work, &args);
    }

    fn bare_repo(root: &TempRoot, name: &str) -> PathBuf {
        let path = root.path().join(name);
        git_ok(
            root.path(),
            &["init", "-q", "--bare", &path.display().to_string()],
        );
        path
    }

    /// A bare remote that already holds `work`'s current base branch.
    fn remote_with_base(root: &TempRoot, work: &Path) -> PathBuf {
        let remote = bare_repo(root, "remote.git");
        git_ok(
            work,
            &[
                "push",
                "-q",
                &remote.display().to_string(),
                "refs/heads/main:refs/heads/main",
            ],
        );
        remote
    }

    fn push_op() -> BackendOp {
        BackendOp::PushTaskBranch {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            branch: "jgt/T1".to_string(),
            base: "main".to_string(),
            commit_message: None,
        }
    }

    fn push_to(work: &Path, remote: &Path) -> Result<BackendOutput, BackendError> {
        let backend = GhCliBackend::new(work).with_remote_url(remote.display().to_string());
        backend.run(&push_op(), &Secret::new(TOKEN))
    }

    fn remote_has_branch(remote: &Path, branch: &str) -> bool {
        Command::new("git")
            .arg("--git-dir")
            .arg(remote)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn git_output(work: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(work)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn git_ok(work: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(work)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn push_ignores_a_pre_push_hook_in_the_work_tree() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(&work, &["-m", "feat: add task"]);
        let remote = remote_with_base(&root, &work);

        let marker = root.path().join("hook-ran");
        let hook = work.join(".git/hooks/pre-push");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).expect("write hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))
            .expect("make hook executable");

        push_to(&work, &remote).expect("clean push");

        assert!(!marker.exists(), "the work tree pre-push hook ran");
        assert!(remote_has_branch(&remote, "jgt/T1"));
    }

    #[test]
    fn push_ignores_a_repo_local_push_instead_of_redirect() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(&work, &["-m", "feat: add task"]);

        let remote = remote_with_base(&root, &work);
        let evil = bare_repo(&root, "evil.git");
        git_ok(
            &work,
            &[
                "config",
                &format!("url.{}.pushInsteadOf", evil.display()),
                &remote.display().to_string(),
            ],
        );

        push_to(&work, &remote).expect("clean push");

        assert!(remote_has_branch(&remote, "jgt/T1"));
        assert!(
            !remote_has_branch(&evil, "jgt/T1"),
            "the push was redirected"
        );
    }

    #[test]
    fn a_moved_worktree_base_cannot_hide_a_rejected_commit() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(&work, &["-m", "add task"]);
        let remote = remote_with_base(&root, &work);
        let branch_sha = git_output(&work, &["rev-parse", "refs/heads/jgt/T1"]);
        git_ok(&work, &["branch", "-f", "main", branch_sha.trim()]);

        let error = push_to(&work, &remote).expect_err("bad commit must be rejected");

        assert!(
            matches!(error, BackendError::CommitRejected { .. }),
            "unexpected moved-base result: {error:?}"
        );
        assert!(!remote_has_branch(&remote, "jgt/T1"));
    }

    #[test]
    fn a_tag_named_like_the_branch_cannot_shadow_the_fetched_branch() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(
            &work,
            &[
                "-m",
                "feat: add task",
                "-m",
                "Co-Authored-By: Someone <someone@example.com>",
            ],
        );
        let base_sha = git_output(&work, &["rev-parse", "refs/heads/main"]);
        git_ok(&work, &["tag", "jgt/T1", base_sha.trim()]);
        let remote = remote_with_base(&root, &work);

        let error = push_to(&work, &remote).expect_err("attribution must be rejected");

        assert!(matches!(error, BackendError::CommitRejected { .. }));
        assert!(!remote_has_branch(&remote, "jgt/T1"));
    }

    #[test]
    fn a_git_dir_pseudoref_named_like_the_branch_cannot_shadow_it() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(&work, &["-m", "add task"]);
        let base_sha = git_output(&work, &["rev-parse", "refs/heads/main"]);
        let shadow = work.join(".git/jgt/T1");
        fs::create_dir_all(shadow.parent().expect("branch path parent"))
            .expect("create pseudoref parent");
        fs::write(&shadow, base_sha.trim()).expect("write pseudoref");
        let remote = remote_with_base(&root, &work);

        let error = push_to(&work, &remote).expect_err("bad commit must be rejected");

        assert!(matches!(error, BackendError::CommitRejected { .. }));
        assert!(!remote_has_branch(&remote, "jgt/T1"));
    }

    #[test]
    fn a_shallow_worktree_does_not_hide_older_commits() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(&work, &["-m", "add task"]);
        let remote = remote_with_base(&root, &work);
        fs::write(work.join("followup.txt"), "followup").expect("write follow-up");
        git_ok(&work, &["add", "followup.txt"]);
        git_ok(
            &work,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                "fix: follow up",
            ],
        );
        let tip = git_output(&work, &["rev-parse", "refs/heads/jgt/T1"]);
        fs::write(work.join(".git/shallow"), tip.trim()).expect("mark worktree shallow");

        let error = push_to(&work, &remote).expect_err("older bad commit must be rejected");

        assert!(
            matches!(
                error,
                BackendError::CommitLog { .. } | BackendError::Failed { .. }
            ),
            "unexpected shallow-worktree refusal: {error:?}"
        );
        assert!(!remote_has_branch(&remote, "jgt/T1"));
    }

    #[test]
    fn clean_push_updates_the_exact_sha_that_was_checked() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(&work, &["-m", "feat: add task"]);
        let expected = git_output(&work, &["rev-parse", "refs/heads/jgt/T1"]);
        let remote = remote_with_base(&root, &work);

        push_to(&work, &remote).expect("clean push");

        let actual = git_output(
            root.path(),
            &[
                "--git-dir",
                &remote.display().to_string(),
                "rev-parse",
                "refs/heads/jgt/T1",
            ],
        );
        assert_eq!(actual.trim(), expected.trim());
    }

    #[test]
    fn a_pushed_attribution_trailer_is_rejected_before_the_push() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(
            &work,
            &[
                "-m",
                "feat: add task",
                "-m",
                "Co-Authored-By: Someone <someone@example.com>",
            ],
        );
        let remote = remote_with_base(&root, &work);

        let error = push_to(&work, &remote).expect_err("attribution must be rejected");
        let BackendError::CommitRejected { commit, reason } = error else {
            panic!("expected a commit rejection");
        };
        assert_eq!(commit.chars().count(), SHORT_HASH_LEN);
        assert!(reason.contains("attribution"), "reason was {reason:?}");
        assert!(!remote_has_branch(&remote, "jgt/T1"));
        assert!(
            git_output(&work, &["rev-parse", "HEAD"]).starts_with(&commit),
            "the rejection must name the pushed commit"
        );
    }

    #[test]
    fn a_pushed_non_conventional_commit_is_rejected_before_the_push() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(&work, &["-m", "add task"]);
        let remote = remote_with_base(&root, &work);

        let error = push_to(&work, &remote).expect_err("bad header must be rejected");
        let BackendError::CommitRejected { commit, reason } = error else {
            panic!("expected a commit rejection");
        };
        assert_eq!(commit.chars().count(), SHORT_HASH_LEN);
        assert!(reason.contains("header"), "reason was {reason:?}");
        assert!(!remote_has_branch(&remote, "jgt/T1"));
    }

    #[test]
    fn non_conventional_base_history_does_not_block_a_clean_push() {
        let root = TempRoot::new();
        let work = root.path().join("work");
        fs::create_dir_all(&work).expect("create work tree");
        git_ok(&work, &["-c", "init.defaultBranch=main", "init", "-q", "."]);
        fs::write(work.join("base.txt"), "base").expect("write base file");
        git_ok(&work, &["add", "base.txt"]);
        commit_staged(&work, "Initial commit");

        git_ok(&work, &["checkout", "-q", "-b", "feature"]);
        fs::write(work.join("feature.txt"), "feature").expect("write feature file");
        git_ok(&work, &["add", "feature.txt"]);
        commit_staged(&work, "feat: feature");

        git_ok(&work, &["checkout", "-q", "main"]);
        fs::write(work.join("main.txt"), "main").expect("write main file");
        git_ok(&work, &["add", "main.txt"]);
        commit_staged(&work, "feat: main");
        git_ok(
            &work,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "merge",
                "-q",
                "--no-ff",
                "feature",
                "-m",
                "Merge pull request #1 from acme/feature",
            ],
        );

        commit_on_task_branch(&work, &["-m", "feat: add task"]);
        let expected = git_output(&work, &["rev-parse", "refs/heads/jgt/T1"]);
        let remote = remote_with_base(&root, &work);

        push_to(&work, &remote).expect("non-conventional base history must be accepted");

        let actual = git_output(
            root.path(),
            &[
                "--git-dir",
                &remote.display().to_string(),
                "rev-parse",
                "refs/heads/jgt/T1",
            ],
        );
        assert_eq!(actual.trim(), expected.trim());
    }

    #[test]
    fn an_unreachable_base_refuses_the_push() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(&work, &["-m", "feat: add task"]);
        let remote = bare_repo(&root, "remote.git");

        let error = push_to(&work, &remote).expect_err("missing base must refuse the push");

        assert!(
            matches!(error, BackendError::BaseFetch { .. }),
            "unexpected base refusal: {error:?}"
        );
        assert!(!remote_has_branch(&remote, "jgt/T1"));
    }

    #[test]
    fn a_push_without_a_base_is_refused() {
        let root = TempRoot::new();
        let work = repo_with_base(&root);
        commit_on_task_branch(&work, &["-m", "feat: add task"]);
        let remote = bare_repo(&root, "remote.git");
        let backend = GhCliBackend::new(&work).with_remote_url(remote.display().to_string());
        let op = BackendOp::PushTaskBranch {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            branch: "jgt/T1".to_string(),
            base: String::new(),
            commit_message: None,
        };

        let error = backend
            .run(&op, &Secret::new(TOKEN))
            .expect_err("missing base must refuse the push");

        assert!(matches!(
            error,
            BackendError::MissingOperationField {
                action: "push_task_branch",
                field: "base"
            }
        ));
        assert!(!remote_has_branch(&remote, "jgt/T1"));
    }
}
