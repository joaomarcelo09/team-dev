//! GitHub credential broker of the Dark Factory lab.
//!
//! The broker is the only place that decides whether a host-side git/GitHub action may
//! happen. It never talks to the network, never handles a token and never starts a process:
//! it evaluates a [`GithubRequest`] against the lab's [`GithubGrant`], enforces the daily
//! caps and appends one [`AuditKind::GithubAction`] event for every decision.
//!
//! A decision is fail-closed. The request is refused unless the owner is granted, the action
//! is granted, the branch (when the action targets one) is a task branch below the configured
//! prefix, and the action is not a force-push or a delete. Merges are refused unless the grant
//! lists them and the request says the run's merge policy allows them.
//!
//! Daily counters live in `<dir>/github_broker.json`. An in-process mutex and a cross-process
//! `flock` of `<dir>/github_broker.lock` serialize check-and-count, so two brokers on one
//! directory can never exceed a cap together. Counters reset when the UTC day changes, are
//! written through a temporary file and a rename, and a corrupt state file is an error rather
//! than a fresh counter.
//!
//! Commit messages follow Conventional Commits. [`validate_commit_message`] returns the
//! message with attribution trailers and signature lines stripped; [`clean_pr_text`] applies
//! the same stripping to pull-request titles and bodies.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use jgf_core::{AuditKind, AuditLog, GithubAction, GithubGrant, ModelError, ProjectId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default prefix of the branches the broker may push.
pub const DEFAULT_TASK_BRANCH_PREFIX: &str = "jgt/";

/// Maximum length of a GitHub owner name.
const MAX_OWNER_LEN: usize = 39;
/// Maximum length of a GitHub repository name.
const MAX_REPO_LEN: usize = 100;
/// Maximum length of a Git branch name.
const MAX_BRANCH_LEN: usize = 255;
/// Maximum length of a commit header line.
const MAX_COMMIT_HEADER_LEN: usize = 72;

/// File name of the persisted daily counters inside the broker directory.
const STATE_FILE: &str = "github_broker.json";
/// File name of the cross-process lock inside the broker directory.
const LOCK_FILE: &str = "github_broker.lock";
/// Schema version written with every state file.
const SCHEMA_VERSION: u32 = 1;
/// Milliseconds in one UTC day.
const DAY_MS: u64 = 86_400_000;

/// Conventional Commits types accepted in a commit header.
const COMMIT_TYPES: [&str; 11] = [
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore", "revert",
];

/// One git/GitHub action the lab asks the broker to allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRequest {
    /// Action to perform.
    pub action: GithubAction,
    /// Owner (user or organization) the action targets.
    pub owner: String,
    /// Repository the action targets.
    pub repo: String,
    /// Branch the action targets, when it has one.
    pub branch: Option<String>,
    /// Branch or ref the task branch is based on. Required to push.
    pub base: Option<String>,
    /// Whether the caller wants a force-push. Always refused.
    pub force: bool,
    /// Whether the caller wants to delete the repository or branch. Always refused.
    pub delete: bool,
    /// Whether the targeted branch is the repository's default branch.
    pub target_is_default_branch: bool,
    /// Whether the run's merge policy allows this merge. Defaults to `false`.
    pub merge_policy_allows: bool,
    /// Commit message to check and clean, when the action creates a commit.
    pub commit_message: Option<String>,
}

/// An allowed action, with the names the caller may now use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approved {
    /// Action that was allowed.
    pub action: GithubAction,
    /// Owner the action may target.
    pub owner: String,
    /// Repository the action may target.
    pub repo: String,
    /// Branch the action may target, when it has one.
    pub branch: Option<String>,
    /// Base the task branch was cut from, when the action pushes one.
    pub base: Option<String>,
    /// Cleaned commit message, when the request carried one.
    pub commit_message: Option<String>,
}

/// Why the broker refused a request, or why it could not decide.
#[derive(Debug, Error)]
pub enum BrokerDenied {
    /// Reading, writing or auditing failed.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// The persisted counter file is not valid JSON.
    #[error("invalid broker state at {}: {source}", path.display())]
    InvalidState {
        /// Path of the rejected file.
        path: PathBuf,
        /// JSON parser error.
        #[source]
        source: serde_json::Error,
    },

    /// The persisted counter file uses a schema version this build does not know.
    #[error("unsupported broker schema version {version} at {}", path.display())]
    UnsupportedVersion {
        /// Path of the rejected file.
        path: PathBuf,
        /// Version found in the file.
        version: u32,
    },

    /// A request name does not follow the safe name rules.
    #[error("invalid {field} name")]
    InvalidName {
        /// Which request field was rejected.
        field: &'static str,
    },

    /// The owner is not listed in the lab's grant.
    #[error("owner {0:?} is not granted")]
    OwnerNotGranted(String),

    /// The action is not listed in the lab's grant.
    #[error("action {0:?} is not granted")]
    ActionNotGranted(&'static str),

    /// The action needs a branch but the request has none.
    #[error("action {0:?} needs a branch")]
    MissingBranch(&'static str),

    /// Pushing needs the base the task branch was cut from.
    #[error("action {0:?} needs a base")]
    MissingBase(&'static str),

    /// The push targets the repository's default branch.
    #[error("pushing to the default branch is not allowed")]
    DefaultBranch,

    /// Force-pushes are never representable as allowed.
    #[error("force is not allowed")]
    Force,

    /// Deletes are never representable as allowed.
    #[error("delete is not allowed")]
    Delete,

    /// The branch is not below the configured task-branch prefix.
    #[error("branch {branch:?} is not a task branch below {prefix:?}")]
    BranchNotTask {
        /// Branch the request targeted.
        branch: String,
        /// Prefix the branch must start with.
        prefix: String,
    },

    /// Merging needs the grant and an explicit merge policy.
    #[error("merging needs an explicit merge policy")]
    MergePolicyRequired,

    /// The action reached its daily cap.
    #[error("daily cap of {limit} {unit} reached")]
    CapReached {
        /// Unit the cap counts.
        unit: &'static str,
        /// Configured cap.
        limit: u32,
    },

    /// The request's commit message does not follow the commit rules.
    #[error("invalid commit message: {0}")]
    InvalidCommit(String),
}

/// Why a commit message does not follow the commit rules.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommitError {
    /// The message has no header.
    #[error("commit message is empty")]
    Empty,

    /// The header is longer than the allowed maximum.
    #[error("commit header is {len} characters, above the {max} character limit")]
    HeaderTooLong {
        /// Length of the header.
        len: usize,
        /// Maximum accepted length.
        max: usize,
    },

    /// The header is not `type(scope)?!?: summary`.
    #[error("invalid commit header {0:?}")]
    InvalidHeader(String),

    /// The header type is not a Conventional Commits type.
    #[error("unknown commit type {0:?}")]
    UnknownType(String),

    /// The header scope is empty or has unsafe characters.
    #[error("invalid commit scope {0:?}")]
    InvalidScope(String),

    /// The header has no summary.
    #[error("commit summary is empty")]
    EmptySummary,

    /// The whole header is an attribution line instead of a commit message.
    #[error("commit header is only an attribution line")]
    AttributionHeader,
}

/// The broker, persisted as `<dir>/github_broker.json`.
///
/// A broker only decides. The runtime performs the allowed action later, with its own
/// credentials, and never receives the token from this module.
#[derive(Debug)]
pub struct Broker {
    dir: PathBuf,
    path: PathBuf,
    lock_path: PathBuf,
    lab: String,
    grant: GithubGrant,
    task_prefix: String,
    audit: AuditLog,
    state: Mutex<()>,
}

impl Broker {
    /// Opens the broker under `dir` with the lab's `grant`.
    ///
    /// A state file that exists but is corrupt, or whose schema version is unknown, is an
    /// error, not fresh counters.
    pub fn open(
        dir: impl Into<PathBuf>,
        lab: impl Into<String>,
        grant: GithubGrant,
        audit: AuditLog,
    ) -> Result<Self, BrokerDenied> {
        let dir = dir.into();
        let path = dir.join(STATE_FILE);
        let lock_path = dir.join(LOCK_FILE);
        read_state(&path)?;
        Ok(Self {
            dir,
            path,
            lock_path,
            lab: lab.into(),
            grant,
            task_prefix: DEFAULT_TASK_BRANCH_PREFIX.to_string(),
            audit,
            state: Mutex::new(()),
        })
    }

    /// Uses `prefix` (with a trailing `/`) as the task-branch prefix.
    pub fn with_task_branch_prefix(mut self, prefix: impl Into<String>) -> Self {
        let mut prefix = prefix.into();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        self.task_prefix = prefix;
        self
    }

    /// Directory that holds the state file.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the persisted state file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lab name the broker belongs to.
    pub fn lab(&self) -> &str {
        &self.lab
    }

    /// Audit log that records every decision.
    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// Decides whether `req` may happen for `project` on behalf of `worker`.
    ///
    /// The check, the cap increment and the audit append happen under one lock. An allowed
    /// action increments its daily counter; a refused action never does. The returned error
    /// is also audited, so the log holds every decision with its reason.
    pub fn check(
        &self,
        req: &GithubRequest,
        project: &ProjectId,
        worker: &str,
    ) -> Result<Approved, BrokerDenied> {
        let _state = self.lock();
        let lock_file = self.open_lock_file()?;
        let _lock = FileLock::exclusive(&lock_file, &self.lock_path)?;

        let today = utc_day(now_ms());
        let mut state = read_state(&self.path)?;
        if state.day != today {
            state = DailyState::fresh(today);
        }

        let decision = match self.decide(req, &mut state) {
            Ok(approved) => self.persist(&state).map(|()| approved),
            Err(denied) => Err(denied),
        };
        let allowed = decision.is_ok();
        let reason = match &decision {
            Ok(_) => "allowed".to_string(),
            Err(denied) => denied.to_string(),
        };
        self.audit.append(
            Some(project.as_str()),
            Some(worker),
            AuditKind::github_action(
                action_name(req.action),
                &req.owner,
                &req.repo,
                req.branch.clone(),
                allowed,
                reason,
            ),
        )?;
        decision
    }

    /// Evaluates the policy and, only when every check passes, counts the action.
    fn decide(
        &self,
        req: &GithubRequest,
        state: &mut DailyState,
    ) -> Result<Approved, BrokerDenied> {
        if req.force {
            return Err(BrokerDenied::Force);
        }
        if req.delete {
            return Err(BrokerDenied::Delete);
        }
        validate_name("owner", &req.owner, MAX_OWNER_LEN)?;
        validate_name("repo", &req.repo, MAX_REPO_LEN)?;
        if let Some(branch) = req.branch.as_deref() {
            validate_branch(branch)?;
        }
        if !self.grant.owners.iter().any(|owner| owner == &req.owner) {
            return Err(BrokerDenied::OwnerNotGranted(req.owner.clone()));
        }
        if !self.grant.actions.contains(&req.action) {
            return Err(BrokerDenied::ActionNotGranted(action_name(req.action)));
        }

        let branch = self.policy_branch(req)?;
        let base = self.policy_base(req)?;
        let commit_message = req
            .commit_message
            .as_deref()
            .map(validate_commit_message)
            .transpose()
            .map_err(|error| BrokerDenied::InvalidCommit(error.to_string()))?;

        self.apply_cap(req.action, state)?;
        Ok(Approved {
            action: req.action,
            owner: req.owner.clone(),
            repo: req.repo.clone(),
            branch,
            base,
            commit_message,
        })
    }

    /// Applies the per-action branch rules and returns the approved branch.
    fn policy_branch(&self, req: &GithubRequest) -> Result<Option<String>, BrokerDenied> {
        match req.action {
            GithubAction::PushTaskBranch => {
                let branch = req
                    .branch
                    .as_deref()
                    .ok_or(BrokerDenied::MissingBranch("push_task_branch"))?;
                if req.target_is_default_branch {
                    return Err(BrokerDenied::DefaultBranch);
                }
                if !is_task_branch(branch, &self.task_prefix) {
                    return Err(BrokerDenied::BranchNotTask {
                        branch: branch.to_string(),
                        prefix: self.task_prefix.clone(),
                    });
                }
                Ok(Some(branch.to_string()))
            }
            GithubAction::MergePr => {
                let branch = req
                    .branch
                    .as_deref()
                    .ok_or(BrokerDenied::MissingBranch("merge_pr"))?;
                if !req.merge_policy_allows {
                    return Err(BrokerDenied::MergePolicyRequired);
                }
                Ok(Some(branch.to_string()))
            }
            GithubAction::OpenPr => {
                let branch = req
                    .branch
                    .as_deref()
                    .ok_or(BrokerDenied::MissingBranch("open_pr"))?;
                if !is_task_branch(branch, &self.task_prefix) {
                    return Err(BrokerDenied::BranchNotTask {
                        branch: branch.to_string(),
                        prefix: self.task_prefix.clone(),
                    });
                }
                Ok(Some(branch.to_string()))
            }
            _ => Ok(req.branch.clone()),
        }
    }

    /// Applies the per-action branch rules and returns the approved base.
    fn policy_base(&self, req: &GithubRequest) -> Result<Option<String>, BrokerDenied> {
        match req.action {
            GithubAction::PushTaskBranch => {
                let base = req
                    .base
                    .as_deref()
                    .ok_or(BrokerDenied::MissingBase("push_task_branch"))?;
                if !branch_is_valid(base) {
                    return Err(BrokerDenied::InvalidName { field: "base" });
                }
                Ok(Some(base.to_string()))
            }
            _ => Ok(None),
        }
    }

    /// Refuses the action when its daily cap is zero or already reached, else counts it.
    fn apply_cap(&self, action: GithubAction, state: &mut DailyState) -> Result<(), BrokerDenied> {
        let (counter, limit, unit) = match action {
            GithubAction::OpenPr => (&mut state.prs, self.grant.max_prs_per_day, "pull requests"),
            GithubAction::PushTaskBranch => {
                (&mut state.pushes, self.grant.max_pushes_per_day, "pushes")
            }
            _ => return Ok(()),
        };
        if limit == 0 || *counter >= limit {
            return Err(BrokerDenied::CapReached { unit, limit });
        }
        *counter += 1;
        Ok(())
    }

    /// Opens (creating when needed) the file used for the cross-process lock.
    fn open_lock_file(&self) -> Result<File, ModelError> {
        fs::create_dir_all(&self.dir).map_err(|source| io_error(&self.dir, source))?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|source| io_error(&self.lock_path, source))
    }

    /// Saves `state` with an atomic replacement of `<dir>/github_broker.json`.
    fn persist(&self, state: &DailyState) -> Result<(), BrokerDenied> {
        let bytes = serde_json::to_vec_pretty(&StateFileRef {
            version: SCHEMA_VERSION,
            state,
        })
        .map_err(|source| BrokerDenied::Model(ModelError::Json(source)))?;
        write_atomic(&self.path, &bytes).map_err(BrokerDenied::from)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Daily caps counted per UTC day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct DailyState {
    /// UTC day as the number of days since the Unix epoch.
    day: u64,
    /// Pull requests opened today.
    prs: u32,
    /// Pushes made today.
    pushes: u32,
}

impl DailyState {
    fn fresh(day: u64) -> Self {
        Self {
            day,
            prs: 0,
            pushes: 0,
        }
    }
}

/// Borrowed on-disk form of the counters, including their schema version.
#[derive(Debug, Serialize)]
struct StateFileRef<'a> {
    version: u32,
    #[serde(flatten)]
    state: &'a DailyState,
}

/// First field read from a state file, before the counters are parsed.
#[derive(Debug, Deserialize)]
struct VersionHeader {
    version: u32,
}

pub(crate) fn action_name(action: GithubAction) -> &'static str {
    match action {
        GithubAction::CreateRepo => "create_repo",
        GithubAction::PushTaskBranch => "push_task_branch",
        GithubAction::OpenPr => "open_pr",
        GithubAction::MergePr => "merge_pr",
        GithubAction::ReadCiStatus => "read_ci_status",
    }
}

/// A single safe path segment for an owner or repository name.
fn validate_name(field: &'static str, value: &str, max: usize) -> Result<(), BrokerDenied> {
    if name_is_valid(value, max) {
        Ok(())
    } else {
        Err(BrokerDenied::InvalidName { field })
    }
}

fn name_is_valid(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.chars().count() <= max
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !value.contains("..")
}

/// A Git branch name without refspec syntax or unsafe characters.
fn validate_branch(branch: &str) -> Result<(), BrokerDenied> {
    if branch_is_valid(branch) {
        Ok(())
    } else {
        Err(BrokerDenied::InvalidName { field: "branch" })
    }
}

fn branch_is_valid(branch: &str) -> bool {
    !branch.is_empty()
        && branch.chars().count() <= MAX_BRANCH_LEN
        && !branch.starts_with(['-', '.', '/'])
        && !branch.ends_with(['/', '.'])
        && !branch.contains("..")
        && !branch.contains("//")
        && !branch.starts_with("refs/")
        && branch
            .split('/')
            .all(|segment| !segment.starts_with(['-', '.']))
        && !branch
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\0')
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Whether `branch` is `prefix` followed by a non-empty task key.
fn is_task_branch(branch: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let Some(key) = branch.strip_prefix(prefix) else {
        return false;
    };
    !key.is_empty()
        && !key.starts_with(['-', '.', '/'])
        && !key.contains("..")
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Current wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// UTC day of `now_ms` as days since the Unix epoch.
fn utc_day(now_ms: u64) -> u64 {
    now_ms / DAY_MS
}

/// Reads and validates the counters at `path`, or fresh counters when it is absent.
fn read_state(path: &Path) -> Result<DailyState, BrokerDenied> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == ErrorKind::NotFound => {
            return Ok(DailyState::fresh(utc_day(now_ms())));
        }
        Err(source) => return Err(io_error(path, source).into()),
    };
    let header = serde_json::from_slice::<VersionHeader>(&bytes).map_err(|source| {
        BrokerDenied::InvalidState {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if header.version != SCHEMA_VERSION {
        return Err(BrokerDenied::UnsupportedVersion {
            path: path.to_path_buf(),
            version: header.version,
        });
    }
    serde_json::from_slice::<DailyState>(&bytes).map_err(|source| BrokerDenied::InvalidState {
        path: path.to_path_buf(),
        source,
    })
}

/// Writes `contents` to `path` through a unique temporary file, fsyncing file and directory.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), ModelError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;

    let tmp = unique_temp_path(path);
    let written = write_file(&tmp, contents)
        .and_then(|()| fs::rename(&tmp, path).map_err(|source| io_error(path, source)));
    if let Err(error) = written {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }

    let dir = fs::File::open(parent).map_err(|source| io_error(parent, source))?;
    dir.sync_all().map_err(|source| io_error(parent, source))
}

/// A temporary file beside `path`, unique across processes and calls.
fn unique_temp_path(path: &Path) -> PathBuf {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{sequence}.tmp", std::process::id()));
    path.with_file_name(name)
}

fn write_file(path: &Path, contents: &[u8]) -> Result<(), ModelError> {
    let mut file = fs::File::create(path).map_err(|source| io_error(path, source))?;
    file.write_all(contents)
        .map_err(|source| io_error(path, source))?;
    file.sync_all().map_err(|source| io_error(path, source))
}

/// Exclusive or shared advisory lock on an open lock file, released on drop.
struct FileLock<'a> {
    file: &'a File,
}

impl<'a> FileLock<'a> {
    fn exclusive(file: &'a File, path: &Path) -> Result<Self, ModelError> {
        Self::acquire(file, path, libc::LOCK_EX)
    }

    fn acquire(file: &'a File, path: &Path, operation: libc::c_int) -> Result<Self, ModelError> {
        loop {
            // SAFETY: `file` holds a valid descriptor for the lifetime of the lock.
            let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
            if result == 0 {
                return Ok(Self { file });
            }
            let source = io::Error::last_os_error();
            if source.kind() != ErrorKind::Interrupted {
                return Err(io_error(path, source));
            }
        }
    }
}

impl Drop for FileLock<'_> {
    fn drop(&mut self) {
        // SAFETY: the descriptor stays open until this borrow ends.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn io_error(path: impl Into<PathBuf>, source: io::Error) -> ModelError {
    ModelError::Io {
        path: path.into(),
        source,
    }
}

/// Checks a Conventional Commits message and returns it with attribution stripped.
///
/// The header must be `type(scope)?!?: summary`, with a Conventional Commits type and a
/// non-empty summary. Body lines that are `Co-Authored-By:`, `Generated with ...`,
/// `Generated by ...` or a `🤖` signature are removed, case-insensitively and in any spacing
/// or dash variant. `Signed-off-by:` lines are kept, and trailing blank lines are removed. A
/// header that is only an attribution line is refused instead of stripped.
pub fn validate_commit_message(message: &str) -> Result<String, CommitError> {
    let normalized = message.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.split('\n');
    let header = lines.next().unwrap_or("").trim();
    if header.is_empty() {
        return Err(CommitError::Empty);
    }
    if is_attribution_line(header) {
        return Err(CommitError::AttributionHeader);
    }
    let header_len = header.chars().count();
    if header_len > MAX_COMMIT_HEADER_LEN {
        return Err(CommitError::HeaderTooLong {
            len: header_len,
            max: MAX_COMMIT_HEADER_LEN,
        });
    }
    validate_commit_header(header)?;

    let body = clean_lines(lines);
    Ok(join_message(header, &body))
}

/// Removes attribution lines from pull-request text, such as a title or body.
///
/// The same lines as [`validate_commit_message`] are removed, case-insensitively, and
/// trailing blank lines are dropped. Text that is only attribution becomes empty.
pub fn clean_pr_text(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    clean_lines(normalized.split('\n')).join("\n")
}

/// Validates the header shape and Conventional Commits type.
fn validate_commit_header(header: &str) -> Result<(), CommitError> {
    let Some((prefix, rest)) = header.split_once(':') else {
        return Err(CommitError::InvalidHeader(header.to_string()));
    };
    if rest.trim().is_empty() {
        return Err(CommitError::EmptySummary);
    }
    if !rest.starts_with(' ') {
        return Err(CommitError::InvalidHeader(header.to_string()));
    }

    let prefix = prefix.strip_suffix('!').unwrap_or(prefix);
    let (kind, scope) = match prefix.find('(') {
        Some(open) => {
            if !prefix.ends_with(')') || open + 1 >= prefix.len() {
                return Err(CommitError::InvalidHeader(header.to_string()));
            }
            (&prefix[..open], Some(&prefix[open + 1..prefix.len() - 1]))
        }
        None => (prefix, None),
    };
    if kind.is_empty() {
        return Err(CommitError::InvalidHeader(header.to_string()));
    }
    if !COMMIT_TYPES.contains(&kind) {
        return Err(CommitError::UnknownType(kind.to_string()));
    }
    if let Some(scope) = scope
        && !scope_is_valid(scope)
    {
        return Err(CommitError::InvalidScope(scope.to_string()));
    }
    Ok(())
}

fn scope_is_valid(scope: &str) -> bool {
    !scope.is_empty()
        && scope
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Filters attribution lines, trims line ends and drops blank lines at both edges.
fn clean_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut kept: Vec<String> = lines
        .filter(|line| !is_attribution_line(line))
        .map(|line| line.trim_end().to_string())
        .collect();
    let first = kept
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(kept.len());
    kept.drain(..first);
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }
    kept
}

/// Joins a header and its cleaned body with one blank line between them.
fn join_message(header: &str, body: &[String]) -> String {
    if body.is_empty() {
        header.to_string()
    } else {
        format!("{header}\n\n{}", body.join("\n"))
    }
}

/// Whether any line of `message` is an attribution trailer or a model signature.
///
/// The header is included: a message whose header is only attribution is refused by
/// [`validate_commit_message`], and a pushed commit carrying any attribution line is refused
/// by the executor.
pub(crate) fn contains_attribution(message: &str) -> bool {
    let normalized = message.replace("\r\n", "\n").replace('\r', "\n");
    normalized.split('\n').any(is_attribution_line)
}

/// Whether `line` is an attribution trailer or a model signature line.
///
/// Only lines that *start* with a marker are attribution: prose such as "this was generated
/// with the new parser" keeps its place. Matching normalises first (case, Unicode dashes and
/// whitespace), so every spacing and dash variant of the same marker is recognised.
fn is_attribution_line(line: &str) -> bool {
    let normalized = normalize_attribution(line);
    if normalized.contains('🤖') {
        return true;
    }
    normalized.starts_with("coauthoredby")
        || normalized.starts_with("generatedwith")
        || normalized.starts_with("generatedby")
}

/// Lowercases `line`, maps Unicode dashes and whitespace to ASCII, then drops the characters
/// that only vary the spelling of an attribution marker: whitespace, `-` and `_`.
fn normalize_attribution(line: &str) -> String {
    line.trim()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            c if c.is_whitespace() => ' ',
            other => other,
        })
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        thread,
    };

    use jgf_core::AuditEvent;

    use super::*;

    /// Temporary broker directory removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jgf-hub-broker-{}-{suffix}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn project(raw: &str) -> ProjectId {
        ProjectId::parse(raw).expect("valid project id")
    }

    fn grant(actions: &[GithubAction], owners: &[&str], prs: u32, pushes: u32) -> GithubGrant {
        GithubGrant {
            owners: owners.iter().map(|owner| (*owner).to_string()).collect(),
            actions: actions.to_vec(),
            max_prs_per_day: prs,
            max_pushes_per_day: pushes,
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

    fn permissive_grant() -> GithubGrant {
        grant(&all_actions(), &["acme"], 5, 5)
    }

    fn open(root: &TempRoot, grant: GithubGrant) -> Broker {
        let audit = AuditLog::open(root.path(), "lab").expect("open audit log");
        Broker::open(root.path(), "lab", grant, audit).expect("open broker")
    }

    fn request(action: GithubAction, owner: &str, repo: &str) -> GithubRequest {
        GithubRequest {
            action,
            owner: owner.to_string(),
            repo: repo.to_string(),
            branch: None,
            base: None,
            force: false,
            delete: false,
            target_is_default_branch: false,
            merge_policy_allows: false,
            commit_message: None,
        }
    }

    fn valid_request(action: GithubAction) -> GithubRequest {
        let mut req = request(action, "acme", "app");
        match action {
            GithubAction::PushTaskBranch => {
                req.branch = Some("jgt/JGT-97".to_string());
                req.base = Some("main".to_string());
            }
            GithubAction::OpenPr => req.branch = Some("jgt/JGT-97".to_string()),
            GithubAction::MergePr => {
                req.branch = Some("jgt/JGT-97".to_string());
                req.merge_policy_allows = true;
            }
            _ => {}
        }
        req
    }

    fn push_request() -> GithubRequest {
        valid_request(GithubAction::PushTaskBranch)
    }

    fn check(broker: &Broker, req: &GithubRequest) -> Result<Approved, BrokerDenied> {
        broker.check(req, &project("alpha"), "worker-1")
    }

    #[test]
    fn every_granted_action_is_allowed_with_a_matching_request() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());
        for action in all_actions() {
            let approved = check(&broker, &valid_request(action))
                .unwrap_or_else(|error| panic!("expected {action:?} to be allowed: {error}"));
            assert_eq!(approved.action, action);
        }
    }

    #[test]
    fn an_empty_grant_allows_nothing() {
        let root = TempRoot::new();
        let broker = open(&root, GithubGrant::default());
        for action in all_actions() {
            assert!(
                check(&broker, &valid_request(action)).is_err(),
                "{action:?}"
            );
        }
    }

    #[test]
    fn an_owner_outside_the_grant_is_denied() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());
        let req = request(GithubAction::CreateRepo, "other", "app");
        assert!(matches!(
            check(&broker, &req),
            Err(BrokerDenied::OwnerNotGranted(owner)) if owner == "other"
        ));
    }

    #[test]
    fn an_action_outside_the_grant_is_denied() {
        let root = TempRoot::new();
        let broker = open(&root, grant(&[GithubAction::ReadCiStatus], &["acme"], 5, 5));
        assert!(matches!(
            check(&broker, &valid_request(GithubAction::CreateRepo)),
            Err(BrokerDenied::ActionNotGranted("create_repo"))
        ));
    }

    #[test]
    fn push_requires_a_task_branch() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());

        let mut missing = push_request();
        missing.branch = None;
        assert!(matches!(
            check(&broker, &missing),
            Err(BrokerDenied::MissingBranch(_))
        ));

        let mut wrong = push_request();
        wrong.branch = Some("feature/work".to_string());
        assert!(matches!(
            check(&broker, &wrong),
            Err(BrokerDenied::BranchNotTask { .. })
        ));
    }

    #[test]
    fn push_requires_a_valid_base() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());

        let mut missing = push_request();
        missing.base = None;
        assert!(matches!(
            check(&broker, &missing),
            Err(BrokerDenied::MissingBase(_))
        ));

        for base in ["../main", "-main", "main..x", "refs/heads/main", "main x"] {
            let mut hostile = push_request();
            hostile.base = Some(base.to_string());
            assert!(
                matches!(
                    check(&broker, &hostile),
                    Err(BrokerDenied::InvalidName { field: "base" })
                ),
                "base {base:?}"
            );
        }
    }

    #[test]
    fn push_never_targets_the_default_branch() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());
        let mut req = push_request();
        req.target_is_default_branch = true;
        assert!(matches!(
            check(&broker, &req),
            Err(BrokerDenied::DefaultBranch)
        ));
    }

    #[test]
    fn the_task_branch_prefix_is_configurable() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant()).with_task_branch_prefix("lab");
        let mut req = push_request();
        req.branch = Some("lab/JGT-97".to_string());
        assert!(check(&broker, &req).is_ok());

        let mut wrong = push_request();
        wrong.branch = Some("jgt/JGT-97".to_string());
        assert!(matches!(
            check(&broker, &wrong),
            Err(BrokerDenied::BranchNotTask { .. })
        ));
    }

    #[test]
    fn force_and_delete_are_always_denied() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());

        let mut force = request(GithubAction::ReadCiStatus, "acme", "app");
        force.force = true;
        assert!(matches!(check(&broker, &force), Err(BrokerDenied::Force)));

        let mut delete = request(GithubAction::CreateRepo, "acme", "app");
        delete.delete = true;
        assert!(matches!(check(&broker, &delete), Err(BrokerDenied::Delete)));
    }

    #[test]
    fn hostile_names_are_denied() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());

        let owners = ["../x", "-x", "a b", "a/b", "a\0b", "a:b", "a+b"];
        for owner in owners {
            let req = request(GithubAction::CreateRepo, owner, "app");
            assert!(
                matches!(
                    check(&broker, &req),
                    Err(BrokerDenied::InvalidName { field: "owner" })
                ),
                "owner {owner:?}"
            );
        }

        let repos = ["../x", "-x", "a b", "a/b", "a\0b", "a:b"];
        for repo in repos {
            let req = request(GithubAction::CreateRepo, "acme", repo);
            assert!(
                matches!(
                    check(&broker, &req),
                    Err(BrokerDenied::InvalidName { field: "repo" })
                ),
                "repo {repo:?}"
            );
        }

        let branches = [
            "jgt/../x",
            "jgt/-x",
            "jgt/x y",
            "jgt/x:y",
            "jgt/+x:y",
            "jgt/x\0y",
            "jgt//x",
            "jgt/x/",
            "refs/heads/jgt/x",
            "jgt/x..y",
        ];
        for branch in branches {
            let mut req = push_request();
            req.branch = Some(branch.to_string());
            assert!(
                matches!(
                    check(&broker, &req),
                    Err(BrokerDenied::InvalidName { field: "branch" })
                ),
                "branch {branch:?}"
            );
        }
    }

    #[test]
    fn merge_needs_an_explicit_policy() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());
        let mut req = request(GithubAction::MergePr, "acme", "app");
        req.branch = Some("jgt/JGT-97".to_string());
        assert!(matches!(
            check(&broker, &req),
            Err(BrokerDenied::MergePolicyRequired)
        ));

        req.merge_policy_allows = true;
        assert!(check(&broker, &req).is_ok());
    }

    #[test]
    fn open_pr_needs_a_task_branch() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());
        let mut req = request(GithubAction::OpenPr, "acme", "app");

        assert!(matches!(
            check(&broker, &req),
            Err(BrokerDenied::MissingBranch("open_pr"))
        ));

        req.branch = Some("feature/untrusted".to_string());
        assert!(matches!(
            check(&broker, &req),
            Err(BrokerDenied::BranchNotTask { .. })
        ));

        req.branch = Some("jgt/JGT-97".to_string());
        assert!(check(&broker, &req).is_ok());
    }

    #[test]
    fn merge_pr_needs_a_branch() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());
        let mut req = request(GithubAction::MergePr, "acme", "app");
        req.merge_policy_allows = true;

        assert!(matches!(
            check(&broker, &req),
            Err(BrokerDenied::MissingBranch("merge_pr"))
        ));
    }

    #[test]
    fn caps_are_enforced_and_persisted_across_reopen() {
        let root = TempRoot::new();
        let broker = open(&root, grant(&all_actions(), &["acme"], 2, 1));

        assert!(check(&broker, &valid_request(GithubAction::OpenPr)).is_ok());
        assert!(check(&broker, &valid_request(GithubAction::OpenPr)).is_ok());
        assert!(matches!(
            check(&broker, &valid_request(GithubAction::OpenPr)),
            Err(BrokerDenied::CapReached {
                unit: "pull requests",
                limit: 2
            })
        ));

        assert!(check(&broker, &push_request()).is_ok());
        assert!(matches!(
            check(&broker, &push_request()),
            Err(BrokerDenied::CapReached {
                unit: "pushes",
                limit: 1
            })
        ));

        drop(broker);
        let reopened = open(&root, grant(&all_actions(), &["acme"], 2, 1));
        assert!(matches!(
            check(&reopened, &valid_request(GithubAction::OpenPr)),
            Err(BrokerDenied::CapReached { .. })
        ));
        assert!(matches!(
            check(&reopened, &push_request()),
            Err(BrokerDenied::CapReached { .. })
        ));
    }

    #[test]
    fn a_zero_cap_denies_the_action() {
        let root = TempRoot::new();
        let broker = open(&root, grant(&all_actions(), &["acme"], 0, 0));
        assert!(matches!(
            check(&broker, &valid_request(GithubAction::OpenPr)),
            Err(BrokerDenied::CapReached { limit: 0, .. })
        ));
        assert!(matches!(
            check(&broker, &push_request()),
            Err(BrokerDenied::CapReached { limit: 0, .. })
        ));
    }

    #[test]
    fn two_brokers_share_one_cap_under_threads() {
        let root = TempRoot::new();
        let grant = grant(&[GithubAction::PushTaskBranch], &["acme"], 0, 1);
        let first = Arc::new(open(&root, grant.clone()));
        let second = Arc::new(open(&root, grant));

        let handles: Vec<_> = (0..8)
            .map(|index| {
                let broker = if index % 2 == 0 {
                    Arc::clone(&first)
                } else {
                    Arc::clone(&second)
                };
                thread::spawn(move || {
                    let req = push_request();
                    broker
                        .check(&req, &project("alpha"), &format!("worker-{index}"))
                        .is_ok()
                })
            })
            .collect();

        let allowed = handles
            .into_iter()
            .map(|handle| handle.join().expect("join worker"))
            .filter(|allowed| *allowed)
            .count();
        assert_eq!(allowed, 1, "the cap must hold across both brokers");
    }

    #[test]
    fn a_corrupt_state_file_is_an_error() {
        let root = TempRoot::new();
        fs::write(root.path().join(STATE_FILE), b"{not json").expect("write corrupt state");

        let audit = AuditLog::open(root.path(), "lab").expect("open audit log");
        let result = Broker::open(root.path(), "lab", permissive_grant(), audit);
        assert!(
            matches!(result, Err(BrokerDenied::InvalidState { .. })),
            "expected invalid state, got {result:?}"
        );
    }

    #[test]
    fn every_decision_is_audited_and_the_chain_verifies() {
        let root = TempRoot::new();
        let broker = open(&root, permissive_grant());

        assert!(check(&broker, &valid_request(GithubAction::CreateRepo)).is_ok());
        let mut denied = request(GithubAction::CreateRepo, "other", "app");
        denied.repo = "../app".to_string();
        assert!(check(&broker, &denied).is_err());

        let log = AuditLog::open(root.path(), "lab").expect("open audit log");
        let events: Vec<AuditEvent> = log.read_all().expect("read audit log");
        assert_eq!(events.len(), 2);
        assert_eq!(log.verify().expect("verify audit chain"), None);

        let AuditKind::GithubAction {
            action,
            owner,
            repo,
            allowed,
            reason,
            ..
        } = &events[0].kind
        else {
            panic!("expected a GitHub action event");
        };
        assert_eq!(action, "create_repo");
        assert_eq!(owner, "acme");
        assert_eq!(repo, "app");
        assert!(*allowed);
        assert_eq!(reason, "allowed");

        let AuditKind::GithubAction { allowed, .. } = &events[1].kind else {
            panic!("expected a GitHub action event");
        };
        assert!(!allowed);
    }

    #[test]
    fn commit_headers_survive_unchanged() {
        for message in [
            "feat: add x",
            "fix(scope): repair y",
            "refactor!: drop z",
            "revert: undo w",
        ] {
            assert_eq!(
                validate_commit_message(message).expect("valid message"),
                message
            );
        }
    }

    #[test]
    fn attribution_trailers_and_signatures_are_stripped() {
        let message = "\
feat: add x

Co-Authored-By: Someone <someone@example.com>
Generated with [Claude Code](https://example.com)
generated by a tool
🤖 Signed by a model
Signed-off-by: Someone <someone@example.com>
";
        let cleaned = validate_commit_message(message).expect("valid message");
        assert_eq!(
            cleaned,
            "feat: add x\n\nSigned-off-by: Someone <someone@example.com>"
        );
    }

    #[test]
    fn attribution_is_stripped_case_insensitively() {
        let message = "\
fix: repair y

CO-AUTHORED-BY: Someone <someone@example.com>
GENERATED WITH a tool
GeNeRaTeD bY a tool
Some prose.
";
        let cleaned = validate_commit_message(message).expect("valid message");
        assert_eq!(cleaned, "fix: repair y\n\nSome prose.");
    }

    #[test]
    fn attribution_markers_match_every_spacing_and_dash_variant() {
        // Only lines that START with the marker are attribution; prose and sign-offs are kept.
        let cases: [(&str, bool); 16] = [
            ("Co-Authored-By: Someone <someone@example.com>", true),
            ("CO-AUTHORED-BY: Someone <someone@example.com>", true),
            ("Co Authored By: Someone <someone@example.com>", true),
            ("Co Authored-By: Someone <someone@example.com>", true),
            ("Co-Authored By: Someone <someone@example.com>", true),
            ("Co\u{2010}Authored\u{2010}By", true),
            ("Co\u{2013}Authored\u{2014}By: Someone", true),
            ("Co\u{2212}Authored\u{2212}By: Someone", true),
            ("Generated with a tool", true),
            ("Generated  with a tool", true),
            ("Generated\twith a tool", true),
            ("Generated-with: a tool", true),
            ("Generated by a tool", true),
            ("🤖 a model wrote this", true),
            ("Signed-off-by: Someone <someone@example.com>", false),
            ("This was generated with the new parser", false),
        ];
        for (line, expected) in cases {
            assert_eq!(is_attribution_line(line), expected, "{line:?}");
        }

        let mut message = String::from("feat: add x\n\nBody prose.\n");
        for (line, expected) in cases {
            if expected {
                message.push_str(line);
                message.push('\n');
            }
        }
        message.push_str("Signed-off-by: Someone <someone@example.com>\n");
        assert_eq!(
            validate_commit_message(&message).expect("valid message"),
            "feat: add x\n\nBody prose.\nSigned-off-by: Someone <someone@example.com>"
        );
    }

    #[test]
    fn a_header_that_is_only_attribution_is_rejected() {
        for message in [
            "Co-Authored-By: Someone <someone@example.com>",
            "Generated with a tool",
            "🤖 a model wrote this",
        ] {
            assert!(
                matches!(
                    validate_commit_message(message),
                    Err(CommitError::AttributionHeader)
                ),
                "{message:?}"
            );
        }
    }

    #[test]
    fn invalid_commit_headers_are_rejected() {
        let cases: [(&str, CommitError); 6] = [
            ("", CommitError::Empty),
            ("add x", CommitError::InvalidHeader("add x".to_string())),
            (
                "feat:add x",
                CommitError::InvalidHeader("feat:add x".to_string()),
            ),
            ("wip: add x", CommitError::UnknownType("wip".to_string())),
            ("feat(): add x", CommitError::InvalidScope(String::new())),
            ("feat:   ", CommitError::EmptySummary),
        ];
        for (message, expected) in cases {
            assert_eq!(validate_commit_message(message), Err(expected));
        }

        let long = format!("feat: {}", "x".repeat(MAX_COMMIT_HEADER_LEN));
        assert_eq!(
            validate_commit_message(&long),
            Err(CommitError::HeaderTooLong {
                len: long.chars().count(),
                max: MAX_COMMIT_HEADER_LEN,
            })
        );
    }

    #[test]
    fn clean_pr_text_strips_the_same_lines() {
        let text = "\
feat: add x

Co-Authored-By: Someone <someone@example.com>
Generated by a tool
Body prose.
Signed-off-by: Someone <someone@example.com>

";
        assert_eq!(
            clean_pr_text(text),
            "feat: add x\n\nBody prose.\nSigned-off-by: Someone <someone@example.com>"
        );

        assert_eq!(clean_pr_text("🤖 a model wrote this"), String::new());
    }
}
