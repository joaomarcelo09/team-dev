//! The `jg-factory e2e` runner.
//!
//! This piece of the end-to-end scenario creates a private repository, pushes the toy fixture
//! as its default branch and creates the integration branch, then freezes the spec, plans one
//! task, puts it on the Board, runs the coder, commits, pushes the task branch and opens the
//! pull request. It then watches CI through the broker, hands failures to a fresh coder, runs
//! the reviewer, and merges the pull request into the integration branch. Every step goes
//! through the credential broker (when it touches GitHub) and lands in the lab audit log.

use std::{
    cell::Cell,
    collections::VecDeque,
    env, fmt, fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use jgf_core::{
    AuditEvent, AuditKind, AuditLog, EnvProfile, FactoryRole, FileSecretStore, GithubAction,
    GithubGrant, ModelError, Project, ProjectId, ProjectStage, RepoSpec, Secret, SecretKey,
    SecretScope, SecretStore, Spec,
    spec::{freeze_spec, lint_spec, verify_spec},
};
use jgf_hub::{
    Actor, BackendError, BackendOp, BackendOutput, Board, BoardError, Broker, BrokerError, CiError,
    CiState, CiWatch, Column, CommitError, DEFAULT_INTEGRATION_BRANCH, DEFAULT_TASK_BRANCH_PREFIX,
    Decision, FakeBackend, GITHUB_TOKEN_KEY, GhCliBackend, GithubBackend, GithubRequest,
    MAX_FIX_CYCLES, MergeContext, MergeDenied, merge_decision, parse_ci_runs_with_presence,
    validate_commit_message,
};
use thiserror::Error;

use crate::coder::{ClaudeCoderRunner, CoderRequest, CoderRun, CoderRunner, FakeCoderRunner};
use crate::reviewer::{
    ClaudeReviewerRunner, FakeReviewerRunner, ReviewDecision, ReviewRequest, ReviewRun,
    ReviewerRunner,
};

/// Default branch the toy repository starts from.
pub const DEFAULT_BRANCH: &str = "main";
/// Lab the end-to-end run records its audit log under.
pub const E2E_LAB: &str = "e2e";
/// Project id recorded in the audit log and broker requests.
const E2E_PROJECT: &str = "e2e-toy";
/// Worker name recorded in the audit log and broker requests.
const E2E_WORKER: &str = "e2e";
/// Worker name recorded for the task branch and pull request.
const E2E_TASK_WORKER: &str = "git-coder";
/// First commit of the toy repository.
const SETUP_COMMIT: &str = "feat: add toy notes fixture";
/// Directory that holds the broker state file.
const BROKER_DIR: &str = "broker";
/// Broker state directory of dry runs, kept apart so they never spend the real daily caps.
const DRY_RUN_BROKER_DIR: &str = "broker-dry-run";
/// Directory that holds the scratch work tree of each run.
const RUNS_DIR: &str = "e2e";
/// Name of the kill-switch file inside [`RUNS_DIR`].
pub const KILL_FILE: &str = "KILL";
/// Name of the file, inside [`RUNS_DIR`], that points at the lab of the latest run.
const LATEST_RUN_FILE: &str = "LATEST_RUN";
/// Directory, inside the state dir, that holds the scratch home of each run.
const SCRATCH_DIR: &str = "scratch";
/// First argument of the terminal outcome event.
const OUTCOME_MARKER: &str = "e2e-outcome";
/// First argument of the event that reports a removed stale kill file.
const STALE_KILL_MARKER: &str = "e2e-kill";
/// How often a waiting run checks the kill switch.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Steps of the flow, in the order they may first appear.
pub const FLOW_STEPS: [&str; 11] = [
    "setup", "intake", "plan", "board", "coder", "commit", "push", "pr", "ci", "review", "merge",
];
/// Directory that holds the Board of each run.
const BOARD_DIR: &str = "board";
/// Environment variable that overrides the fixture location.
const FIXTURE_ENV: &str = "JGF_E2E_FIXTURE";
/// Longest title (and commit summary) the planner takes from the spec.
const MAX_TITLE_CHARS: usize = 60;
/// Filesystem/network boundary used by real Claude children; full jg-sandbox remains a follow-up.
const BWRAP_AUDIT_NOTE: &str = "real Claude children use bwrap filesystem isolation; work tree writable except .git is read-only; dry-run fakes spawn no child; network open; Claude auth only; full jg-sandbox is a follow-up";
/// Directory that holds the CI watcher state of a run.
const CI_WATCH_DIR: &str = "ci-watch";
/// CI watcher directory of dry runs, kept apart from the real fix-cycle counters.
const DRY_RUN_CI_WATCH_DIR: &str = "ci-watch-dry-run";
/// Directory that holds the failure logs handed to the fix coder.
const LOG_DIR: &str = "e2e-logs";
/// Pull request number used by the single-pull-request toy scenario.
const E2E_PULL_REQUEST: u64 = 1;
/// Files and directories that are never copied out of the fixture.
const IGNORED_NAMES: [&str; 11] = [
    ".git",
    ".env",
    "node_modules",
    "target",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".venv",
    "venv",
    "dist",
];

/// How an end-to-end run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run finished its deliverable.
    Delivered,
    /// The run stopped on an error.
    Failed { reason: String },
    /// The run stopped because its budget was spent.
    BudgetExhausted,
    /// The run was stopped by the kill switch.
    Killed,
}

impl RunOutcome {
    /// A run that stopped on `reason`.
    pub fn failed(reason: impl Into<String>) -> Self {
        Self::Failed {
            reason: reason.into(),
        }
    }

    /// A run that stopped because its coder spent its wall-clock limit or budget.
    pub const fn budget_exhausted() -> Self {
        Self::BudgetExhausted
    }
}

impl fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Delivered => f.write_str("delivered"),
            Self::Failed { reason } => write!(f, "failed: {reason}"),
            Self::BudgetExhausted => f.write_str("budget exhausted"),
            Self::Killed => f.write_str("killed"),
        }
    }
}

/// Hook the runner polls between steps and inside waits.
///
/// The real runner uses [`FileKillSwitch`], which watches `<state-dir>/e2e/KILL`; tests pass
/// their own implementation.
pub trait KillSwitch {
    /// Whether the run must stop before its next step.
    fn killed(&self) -> bool;
}

/// The default [`KillSwitch`]: never set.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoKillSwitch;

impl KillSwitch for NoKillSwitch {
    fn killed(&self) -> bool {
        false
    }
}

/// A [`KillSwitch`] backed by the `<state-dir>/e2e/KILL` file.
#[derive(Debug, Clone)]
pub struct FileKillSwitch {
    path: PathBuf,
}

impl FileKillSwitch {
    /// Watches the kill file of `state_dir`.
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        Self {
            path: kill_file(state_dir.as_ref()),
        }
    }

    /// Path of the watched file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl KillSwitch for FileKillSwitch {
    fn killed(&self) -> bool {
        self.path.exists()
    }
}

/// Path of the kill switch of `state_dir`.
pub fn kill_file(state_dir: &Path) -> PathBuf {
    state_dir.join(RUNS_DIR).join(KILL_FILE)
}

/// Asks the run using `state_dir` to stop by creating its kill file.
pub fn request_kill(state_dir: &Path) -> Result<PathBuf, E2eError> {
    let path = kill_file(state_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    fs::write(&path, "killed\n").map_err(|source| io_error(&path, source))?;
    Ok(path)
}

/// Logical step of a run, used to report where the kill switch stopped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Step {
    /// Before the first step.
    #[default]
    Start,
    /// Repository creation and fixture setup.
    Setup,
    /// Spec intake.
    Intake,
    /// Planning.
    Plan,
    /// Board and task branch.
    Board,
    /// Coder run.
    Coder,
    /// Commit.
    Commit,
    /// Push of the task branch.
    Push,
    /// Pull request.
    Pr,
    /// CI watch and fix cycles.
    Ci,
    /// Review.
    Review,
    /// Merge.
    Merge,
    /// The deliverable is on the integration branch.
    Done,
}

impl Step {
    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Setup => "setup",
            Self::Intake => "intake",
            Self::Plan => "plan",
            Self::Board => "board",
            Self::Coder => "coder",
            Self::Commit => "commit",
            Self::Push => "push",
            Self::Pr => "pr",
            Self::Ci => "ci",
            Self::Review => "review",
            Self::Merge => "merge",
            Self::Done => "done",
        }
    }
}

/// Step the run reached; the terminal event records it.
#[derive(Debug, Default)]
struct Progress {
    step: Cell<Step>,
}

impl Progress {
    fn set(&self, step: Step) {
        self.step.set(step);
    }

    fn current(&self) -> Step {
        self.step.get()
    }
}

/// Stops before `step` when the kill switch is set, otherwise records that the run reached it.
fn begin_step<K>(progress: &Progress, kill: &K, step: Step) -> Result<(), E2eError>
where
    K: KillSwitch,
{
    if kill.killed() {
        return Err(E2eError::Killed);
    }
    progress.set(step);
    Ok(())
}

/// Everything one `jg-factory e2e` invocation needs.
#[derive(Debug, Clone)]
pub struct E2eConfig {
    /// GitHub owner the private repository is created under.
    pub owner: String,
    /// Name of the private repository.
    pub repo: String,
    /// Dark Factory data directory; audit logs, broker state and work trees live here.
    pub state_dir: PathBuf,
    /// Directory holding the toy fixture.
    pub fixture_dir: PathBuf,
    /// Lab name used by the audit log and the broker.
    pub lab: String,
    /// Default branch pushed first.
    pub default_branch: String,
    /// Integration branch task branches merge into.
    pub integration_branch: String,
    /// Maximum pushes the lab may make in one day.
    pub max_pushes_per_day: u32,
    /// Maximum pull requests the lab may open in one day.
    pub max_prs_per_day: u32,
    /// Prefix of the task branch the coder works on.
    pub task_branch_prefix: String,
    /// Hard wall-clock limit of the coder process.
    pub coder_timeout: Duration,
    /// Maximum number of agentic turns of the coder.
    pub max_coder_turns: u32,
    /// Maximum amount of US dollars the coder may spend.
    pub max_coder_budget_usd: f64,
    /// Hard wall-clock limit of the reviewer process.
    pub reviewer_timeout: Duration,
    /// Maximum number of agentic turns of the reviewer.
    pub max_reviewer_turns: u32,
    /// Maximum amount of US dollars the reviewer may spend.
    pub max_reviewer_budget_usd: f64,
    /// How long to wait between two CI polls.
    pub ci_poll_interval: Duration,
    /// Consecutive polls without a run for the pushed SHA before setup is considered broken.
    pub ci_no_run_poll_limit: usize,
    /// Wall-clock limit of the whole run, from intake to merge.
    pub max_wall_time: Duration,
    /// Stage of the toy project; the merge policy reads it.
    pub stage: ProjectStage,
    /// Use the fake backend, the in-memory token, the fake coder and the fake reviewer.
    pub dry_run: bool,
    /// Human operator included in generated commit messages, when available.
    co_author: Option<Operator>,
    co_author_override: bool,
    co_author_skip_note: Option<&'static str>,
}

/// Human operator identity used for a commit trailer.
#[derive(Clone, PartialEq, Eq)]
struct Operator {
    name: String,
    email: String,
}

impl Operator {
    /// Creates an identity only when both parts are safe to put in a commit trailer.
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Option<Self> {
        let name = name.into();
        let email = email.into();
        if !valid_operator_part(&name) || !valid_operator_part(&email) {
            return None;
        }
        Some(Self {
            name: name.trim().to_string(),
            email: email.trim().to_string(),
        })
    }

    /// Parses the CLI form `Name <email>`.
    pub fn parse(value: &str) -> Option<Self> {
        if value.chars().any(char::is_control) {
            return None;
        }
        let (name, email) = value.trim().rsplit_once(" <")?;
        let email = email.strip_suffix('>')?;
        Self::new(name, email)
    }
}

impl fmt::Debug for Operator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Operator")
            .field("name", &"[redacted]")
            .field("email", &"[redacted]")
            .finish()
    }
}

fn valid_operator_part(value: &str) -> bool {
    !value.trim().is_empty()
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '<' | '>'))
}

impl E2eConfig {
    /// A configuration with the standard branches and a non-dry-run default.
    pub fn new(
        owner: impl Into<String>,
        repo: impl Into<String>,
        state_dir: impl Into<PathBuf>,
        fixture_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            state_dir: state_dir.into(),
            fixture_dir: fixture_dir.into(),
            lab: E2E_LAB.to_string(),
            default_branch: DEFAULT_BRANCH.to_string(),
            integration_branch: DEFAULT_INTEGRATION_BRANCH.to_string(),
            max_pushes_per_day: 1000,
            max_prs_per_day: 3,
            task_branch_prefix: DEFAULT_TASK_BRANCH_PREFIX.to_string(),
            coder_timeout: Duration::from_secs(30 * 60),
            max_coder_turns: 20,
            max_coder_budget_usd: 1.0,
            reviewer_timeout: Duration::from_secs(15 * 60),
            max_reviewer_turns: 10,
            max_reviewer_budget_usd: 0.5,
            ci_poll_interval: Duration::from_secs(5),
            ci_no_run_poll_limit: 36,
            max_wall_time: Duration::from_secs(60 * 60),
            stage: ProjectStage::Mvp,
            dry_run: false,
            co_author: None,
            co_author_override: false,
            co_author_skip_note: None,
        }
    }

    /// Uses the supplied co-author string instead of the host's global Git identity.
    /// Invalid values are skipped at run start rather than making the run fail.
    pub fn set_co_author_override(&mut self, value: &str) {
        self.co_author_override = true;
        self.co_author = Operator::parse(value);
        self.co_author_skip_note = self
            .co_author
            .is_none()
            .then_some("co-author trailer skipped: invalid override");
    }

    /// Disables co-author trailers instead of reading the host's global Git identity.
    pub fn disable_co_author(&mut self) {
        self.co_author_override = true;
        self.co_author = None;
        self.co_author_skip_note = Some("co-author trailer skipped: disabled");
    }
}

/// Why setup could not finish.
#[derive(Debug, Error)]
pub enum E2eError {
    /// A local file operation failed.
    #[error("io error at {}: {source}", path.display())]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },

    /// A local git command failed.
    #[error("git command `{command}` failed ({status}): {stderr}")]
    Git {
        /// Command that failed.
        command: String,
        /// Exit status.
        status: String,
        /// Captured standard error.
        stderr: String,
    },

    /// The audit log or another model file failed.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// The broker refused a request.
    #[error(transparent)]
    Denied(#[from] jgf_hub::BrokerDenied),

    /// The broker could not execute a request.
    #[error(transparent)]
    Broker(#[from] BrokerError),

    /// The Board refused an operation.
    #[error(transparent)]
    Board(#[from] BoardError),

    /// The generated commit message is not acceptable.
    #[error(transparent)]
    Commit(#[from] CommitError),

    /// The coder stopped because its wall-clock limit or budget ran out.
    #[error("budget exhausted: {0}")]
    Budget(String),

    /// The coder could not produce a usable result.
    #[error("coder failed: {0}")]
    Coder(String),

    /// The reviewer could not produce a usable result.
    #[error("reviewer failed: {0}")]
    Review(String),

    /// The CI watcher refused a result or failed to persist its state.
    #[error(transparent)]
    Ci(#[from] CiError),

    /// No CI run appeared for the pushed commit after the configured polling limit.
    #[error("no CI run started for {sha}: check that the repo has .github/workflows at its root")]
    NoCiRunStarted {
        /// Commit whose workflow run never appeared.
        sha: String,
    },

    /// The merge policy refused the integration branch.
    #[error("merge denied: {0}")]
    Merge(#[from] MergeDenied),

    /// The fix cycles of the pull request are exhausted.
    #[error("CI fix cycles exhausted after {max} cycles")]
    FixCyclesExhausted {
        /// Limit the CI watcher enforces.
        max: u8,
    },

    /// The kill switch stopped the run.
    #[error("killed by the kill switch")]
    Killed,

    /// The spec could not be taken in.
    #[error("spec intake failed: {0}")]
    Intake(String),

    /// The fixture directory does not exist.
    #[error("e2e fixture not found at {}", .0.display())]
    FixtureNotFound(PathBuf),

    /// The copied work tree has no workflow at the repository root.
    #[error("work tree must contain .github/workflows/*.yml at its root: {}", .0.display())]
    RootCiWorkflowNotFound(PathBuf),

    /// The repository name is not a single safe path segment.
    #[error("invalid repository name {0:?}")]
    InvalidRepoName(String),

    /// The current directory could not be read while looking for the fixture.
    #[error("could not read the current directory: {0}")]
    CurrentDir(#[source] io::Error),
}

/// Runs the flow with the real GitHub backend and agents, or the dry-run fakes.
pub fn run(config: &E2eConfig) -> RunOutcome {
    let config = match start_run(config) {
        Ok(config) => config,
        Err(error) => return RunOutcome::failed(error.to_string()),
    };
    let kill = FileKillSwitch::new(&config.state_dir);
    if config.dry_run {
        let backend = DryRunBackend::new(work_dir(&config), [CiState::Failed, CiState::Green]);
        run_prepared(
            &config,
            &backend,
            &MemorySecretStore::dry_run(),
            &FakeCoderRunner::editing(),
            &FakeReviewerRunner::approving(),
            &kill,
        )
    } else {
        println!(
            "coder/reviewer filesystem isolated via bwrap (host filesystem is not visible except for the listed binds: task work tree is writable except .git, which is read-only; scratch is writable; system directories and the Claude credentials file are read-only; /tmp is a fresh tmpfs; /proc and /dev are fresh views); network is open, so Claude can still reach whatever the network allows with Claude auth only. Full jg-sandbox is a follow-up. Scratch HOME: {}",
            scratch_dir(&config).display(),
        );
        let store = FileSecretStore::new(&config.state_dir);
        run_prepared(
            &config,
            &GhCliBackend::new(work_dir(&config)),
            &store,
            &ClaudeCoderRunner::new(),
            &ClaudeReviewerRunner::new(),
            &kill,
        )
    }
}

/// Runs the flow with an explicit backend, secret store, coder, reviewer and kill switch.
pub fn run_with<B, S, C, R, K>(
    config: &E2eConfig,
    backend: &B,
    store: &S,
    coder: &C,
    reviewer: &R,
    kill: &K,
) -> RunOutcome
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
    C: CoderRunner + ?Sized,
    R: ReviewerRunner + ?Sized,
    K: KillSwitch,
{
    match start_run(config) {
        Ok(config) => run_prepared(&config, backend, store, coder, reviewer, kill),
        Err(error) => RunOutcome::failed(error.to_string()),
    }
}

/// Runs the flow of a prepared config, whose lab names the fresh log of this run.
fn run_prepared<B, S, C, R, K>(
    config: &E2eConfig,
    backend: &B,
    store: &S,
    coder: &C,
    reviewer: &R,
    kill: &K,
) -> RunOutcome
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
    C: CoderRunner + ?Sized,
    R: ReviewerRunner + ?Sized,
    K: KillSwitch,
{
    let progress = Progress::default();
    let outcome = match deliver(config, backend, store, coder, reviewer, kill, &progress) {
        Ok(()) => RunOutcome::Delivered,
        Err(E2eError::Budget(_)) => RunOutcome::budget_exhausted(),
        Err(E2eError::Killed) => RunOutcome::Killed,
        Err(error) => RunOutcome::failed(error.to_string()),
    };
    let outcome = redact_outcome_email(outcome, config);
    if let Err(error) = record_outcome(config, &outcome, progress.current()) {
        return RunOutcome::failed(format!("could not record the run outcome: {error}"));
    }
    outcome
}

fn redact_outcome_email(outcome: RunOutcome, config: &E2eConfig) -> RunOutcome {
    let Some(operator) = &config.co_author else {
        return outcome;
    };
    match outcome {
        RunOutcome::Failed { reason } => {
            RunOutcome::failed(reason.replace(&operator.email, "[redacted]"))
        }
        outcome => outcome,
    }
}

/// Prepares a new run: a fresh lab and audit log, the latest-run pointer and no stale kill file.
fn start_run(config: &E2eConfig) -> Result<E2eConfig, E2eError> {
    let mut config = config.clone();
    config.lab = new_run_lab();
    println!("push cap: {} per day", config.max_pushes_per_day);
    let co_author_note = resolve_co_author(&mut config);
    clean_old_scratch(&config)?;
    write_latest_run(&config)?;
    let audit = AuditLog::open(&config.state_dir, &config.lab)?;
    audit_co_author(&audit, co_author_note)?;
    audit_push_cap(&audit, config.max_pushes_per_day)?;
    remove_stale_kill(&config, &audit)?;
    Ok(config)
}

fn resolve_co_author(config: &mut E2eConfig) -> &'static str {
    if config.co_author_override {
        return if config.co_author.is_some() {
            "co-author trailer added"
        } else {
            config
                .co_author_skip_note
                .unwrap_or("co-author trailer skipped: invalid override")
        };
    }

    resolve_global_operator(config, read_global_operator())
}

fn resolve_global_operator(config: &mut E2eConfig, operator: OperatorRead) -> &'static str {
    match operator {
        OperatorRead::Found(operator) => {
            config.co_author = Some(operator);
            "co-author trailer added"
        }
        OperatorRead::Missing => "co-author trailer skipped: missing global identity",
        OperatorRead::Invalid => "co-author trailer skipped: invalid global identity",
    }
}

enum OperatorRead {
    Found(Operator),
    Missing,
    Invalid,
}

enum GitConfigValue {
    Present(String),
    Missing,
    Invalid,
}

fn read_global_operator() -> OperatorRead {
    let Some(home) = env::var_os("HOME") else {
        return OperatorRead::Missing;
    };
    let name = read_global_git_value(&home, "user.name");
    let email = read_global_git_value(&home, "user.email");
    operator_from_git_values(name, email)
}

fn operator_from_git_values(name: GitConfigValue, email: GitConfigValue) -> OperatorRead {
    match (name, email) {
        (GitConfigValue::Present(name), GitConfigValue::Present(email)) => {
            match Operator::new(name, email) {
                Some(operator) => OperatorRead::Found(operator),
                None => OperatorRead::Invalid,
            }
        }
        (GitConfigValue::Invalid, _) | (_, GitConfigValue::Invalid) => OperatorRead::Invalid,
        _ => OperatorRead::Missing,
    }
}

fn read_global_git_value(home: &std::ffi::OsStr, key: &str) -> GitConfigValue {
    let Some(git) = host_git_executable() else {
        return GitConfigValue::Missing;
    };
    let output = match Command::new(git)
        .args(["config", "--global", "--get", key])
        .env_clear()
        .env("HOME", home)
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return GitConfigValue::Missing,
    };
    let value = match String::from_utf8(output.stdout) {
        Ok(value) => value,
        Err(_) => return GitConfigValue::Invalid,
    };
    match value.strip_suffix('\n') {
        Some(value) => GitConfigValue::Present(value.to_string()),
        None => GitConfigValue::Invalid,
    }
}

fn host_git_executable() -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
}

fn audit_co_author(audit: &AuditLog, note: &str) -> Result<(), E2eError> {
    audit.append(
        Some(E2E_PROJECT),
        Some(E2E_WORKER),
        AuditKind::command(vec![note.to_string()], Some(0), None, false, 0),
    )?;
    Ok(())
}

fn audit_push_cap(audit: &AuditLog, max_pushes_per_day: u32) -> Result<(), E2eError> {
    audit.append(
        Some(E2E_PROJECT),
        Some(E2E_WORKER),
        AuditKind::note(format!("push cap: {max_pushes_per_day} per day")),
    )?;
    Ok(())
}

/// Removes scratch directories from prior generated labs at the start of a new run.
fn clean_old_scratch(config: &E2eConfig) -> Result<(), E2eError> {
    let root = config.state_dir.join(SCRATCH_DIR);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(&root, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| io_error(&root, error))?;
        let lab_name = entry.file_name();
        let lab = lab_name.to_string_lossy();
        if lab != config.lab && is_scratch_lab(&lab) {
            let file_type = entry
                .file_type()
                .map_err(|error| io_error(entry.path(), error))?;
            if file_type.is_dir() {
                fs::remove_dir_all(entry.path()).map_err(|error| io_error(entry.path(), error))?;
            }
        }
    }
    Ok(())
}

fn is_scratch_lab(name: &str) -> bool {
    name == E2E_LAB
        || name.strip_prefix("e2e-").is_some_and(|suffix| {
            suffix.len() == 20 && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Lab name of a new run; the timestamp keeps the log of every run apart.
fn new_run_lab() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{E2E_LAB}-{nanos:020}")
}

fn write_latest_run(config: &E2eConfig) -> Result<(), E2eError> {
    let dir = config.state_dir.join(RUNS_DIR);
    fs::create_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
    let path = dir.join(LATEST_RUN_FILE);
    fs::write(&path, format!("{}\n", config.lab)).map_err(|source| io_error(&path, source))
}

/// Lab of the latest run of `state_dir`, or the base lab when no run pointer exists.
fn latest_run_lab(state_dir: &Path) -> String {
    fs::read_to_string(state_dir.join(RUNS_DIR).join(LATEST_RUN_FILE))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|lab| !lab.is_empty())
        .unwrap_or_else(|| E2E_LAB.to_string())
}

/// Removes the kill file a previous run left behind and audits the removal.
fn remove_stale_kill(config: &E2eConfig, audit: &AuditLog) -> Result<(), E2eError> {
    let path = kill_file(&config.state_dir);
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(&path).map_err(|source| io_error(&path, source))?;
    audit.append(
        Some(E2E_PROJECT),
        Some(E2E_WORKER),
        AuditKind::command(
            vec![
                STALE_KILL_MARKER.to_string(),
                "removed-stale".to_string(),
                path.display().to_string(),
            ],
            Some(0),
            None,
            false,
            0,
        ),
    )?;
    Ok(())
}

/// Appends the single terminal event of a run.
fn record_outcome(config: &E2eConfig, outcome: &RunOutcome, step: Step) -> Result<(), E2eError> {
    let label = match outcome {
        RunOutcome::Delivered => "delivered",
        RunOutcome::Failed { .. } => "failed",
        RunOutcome::BudgetExhausted => "budget_exhausted",
        RunOutcome::Killed => "killed",
    };
    let audit = AuditLog::open(&config.state_dir, &config.lab)?;
    audit.append(
        Some(E2E_PROJECT),
        Some(E2E_WORKER),
        AuditKind::command(
            vec![
                OUTCOME_MARKER.to_string(),
                label.to_string(),
                step.as_str().to_string(),
            ],
            Some(0),
            None,
            false,
            0,
        ),
    )?;
    Ok(())
}

/// What [`verify_run`] found in a run's audit log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Number of events in the log.
    pub events: usize,
    /// Label of the terminal outcome event, when there is exactly one.
    pub terminal: Option<String>,
    /// Everything that is wrong with the log; empty means the run verifies.
    pub problems: Vec<String>,
}

impl Report {
    /// Whether no problem was found.
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Verifies the audit log of the end-to-end run kept in `state_dir`.
///
/// It checks the hash chain, that the flow steps appear in their allowed order, that no
/// event contains the GitHub token or a token-looking string, and that the run has exactly
/// one terminal outcome event.
pub fn verify_run(state_dir: &Path) -> Report {
    let mut report = Report::default();
    let log = match AuditLog::open(state_dir, latest_run_lab(state_dir)) {
        Ok(log) => log,
        Err(error) => {
            report
                .problems
                .push(format!("could not open the audit log: {error}"));
            return report;
        }
    };

    match log.verify() {
        Ok(None) => {}
        Ok(Some(seq)) => report
            .problems
            .push(format!("hash chain is broken at event {seq}")),
        Err(error) => report
            .problems
            .push(format!("could not verify the hash chain: {error}")),
    }

    let events = match log.read_all() {
        Ok(events) => events,
        Err(error) => {
            report
                .problems
                .push(format!("could not read the audit log: {error}"));
            return report;
        }
    };
    report.events = events.len();

    let terminals: Vec<String> = events.iter().filter_map(outcome_label).collect();
    report.terminal = terminals.first().cloned();
    if terminals.len() != 1 {
        report.problems.push(format!(
            "expected exactly one terminal outcome event, found {}",
            terminals.len()
        ));
    } else if !matches!(
        terminals[0].as_str(),
        "delivered" | "failed" | "budget_exhausted" | "killed"
    ) {
        report
            .problems
            .push(format!("unknown terminal outcome {:?}", terminals[0]));
    }

    check_flow_order(&events, &mut report.problems);
    check_for_tokens(state_dir, &events, &mut report.problems);
    report
}

/// Label of a terminal outcome event.
fn outcome_label(event: &AuditEvent) -> Option<String> {
    let AuditKind::Command { argv, .. } = &event.kind else {
        return None;
    };
    if argv.first().map(String::as_str) != Some(OUTCOME_MARKER) {
        return None;
    }
    argv.get(1).cloned()
}

/// Whether the event is the spec intake command.
fn is_intake(event: &AuditEvent) -> bool {
    matches!(&event.kind, AuditKind::Command { argv, .. }
        if argv.first().map(String::as_str) == Some("jgf")
            && argv.get(1).map(String::as_str) == Some("spec"))
}

/// Rank of the flow step an event belongs to, when it belongs to one.
fn flow_rank(event: &AuditEvent, intake_seen: bool) -> Option<usize> {
    match &event.kind {
        AuditKind::Command { argv, .. } => {
            let first = argv.first().map(String::as_str);
            let second = argv.get(1).map(String::as_str);
            match (first, second) {
                (Some("jgf"), Some("spec")) => Some(1),
                (Some("jgf"), Some("plan")) => Some(2),
                (Some("bwrap"), _) => Some(if event.worker.as_deref() == Some("reviewer") {
                    9
                } else {
                    4
                }),
                (Some("claude"), _) => Some(if event.worker.as_deref() == Some("reviewer") {
                    9
                } else {
                    4
                }),
                (Some("review"), _) => Some(9),
                (Some("merge-decision"), _) => Some(10),
                (Some("ci-fix"), _) => Some(8),
                (Some("git"), _) if !intake_seen => Some(0),
                (Some("git"), Some("add")) | (Some("git"), Some("commit")) => Some(5),
                _ => None,
            }
        }
        AuditKind::GithubAction { action, .. } => match action.as_str() {
            "create_repo" | "push_setup_branch" => Some(0),
            "push_task_branch" => Some(6),
            "open_pr" => Some(7),
            "read_ci_status" => Some(8),
            "merge_pr" => Some(10),
            _ => None,
        },
        AuditKind::BoardMove { to, .. } => match to.as_str() {
            "review" => Some(9),
            "done" => Some(10),
            _ => Some(3),
        },
        AuditKind::CiResult { .. } => Some(8),
        AuditKind::Note { .. }
        | AuditKind::Network { .. }
        | AuditKind::DeniedWrite { .. }
        | AuditKind::Cost { .. } => None,
    }
}

/// Reports the first appearance of a step that comes after a later step.
fn check_flow_order(events: &[AuditEvent], problems: &mut Vec<String>) {
    let mut intake_seen = false;
    let mut seen = [false; FLOW_STEPS.len()];
    let mut highest = 0usize;
    for event in events {
        let rank = flow_rank(event, intake_seen);
        if is_intake(event) {
            intake_seen = true;
        }
        let Some(rank) = rank else {
            continue;
        };
        if seen[rank] {
            continue;
        }
        if rank < highest {
            problems.push(format!(
                "flow step {:?} appears after {:?}",
                FLOW_STEPS[rank], FLOW_STEPS[highest]
            ));
        }
        seen[rank] = true;
        highest = highest.max(rank);
    }
}

/// Reports events that carry the GitHub token value or a token-looking string.
fn check_for_tokens(state_dir: &Path, events: &[AuditEvent], problems: &mut Vec<String>) {
    const PATTERNS: [&str; 3] = ["Authorization", "ghp_", "github_pat_"];
    let token = FileSecretStore::new(state_dir)
        .get(&SecretScope::Lab, &token_key())
        .ok()
        .flatten()
        .map(|secret| secret.expose().to_string());
    for event in events {
        let text = format!("{event:?}");
        let pattern = PATTERNS.iter().find(|pattern| text.contains(**pattern));
        let carries_token = token
            .as_deref()
            .is_some_and(|token| !token.is_empty() && text.contains(token));
        if let Some(pattern) = pattern {
            problems.push(format!(
                "event {} contains the token-looking string {pattern:?}",
                event.seq
            ));
        } else if carries_token {
            problems.push(format!(
                "event {} contains the GitHub token value",
                event.seq
            ));
        }
    }
}

/// Scratch work tree of `config`.
pub fn work_dir(config: &E2eConfig) -> PathBuf {
    config.state_dir.join(RUNS_DIR).join(&config.repo)
}

/// Scratch home of the coder and reviewer children of this run.
fn scratch_dir(config: &E2eConfig) -> PathBuf {
    config.state_dir.join(SCRATCH_DIR).join(&config.lab)
}

fn broker_dir(config: &E2eConfig) -> PathBuf {
    let name = if config.dry_run {
        DRY_RUN_BROKER_DIR
    } else {
        BROKER_DIR
    };
    config.state_dir.join(name)
}

fn board_dir(config: &E2eConfig) -> PathBuf {
    config.state_dir.join(BOARD_DIR)
}

fn ci_watch_dir(config: &E2eConfig) -> PathBuf {
    let name = if config.dry_run {
        DRY_RUN_CI_WATCH_DIR
    } else {
        CI_WATCH_DIR
    };
    config.state_dir.join(name)
}

/// Fixture directory: the environment override, then an ancestor's `e2e/toy`, then the
/// directory next to this crate in the source tree.
pub fn default_fixture_dir() -> Result<PathBuf, E2eError> {
    if let Some(path) = env::var_os(FIXTURE_ENV) {
        return Ok(PathBuf::from(path));
    }

    let start = env::current_dir().map_err(E2eError::CurrentDir)?;
    let mut dir = start.as_path();
    loop {
        let candidate = dir.join("e2e").join("toy");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent;
    }

    let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/toy");
    if built.is_dir() {
        Ok(built)
    } else {
        Err(E2eError::FixtureNotFound(built))
    }
}

fn deliver<B, S, C, R, K>(
    config: &E2eConfig,
    backend: &B,
    store: &S,
    coder: &C,
    reviewer: &R,
    kill: &K,
    progress: &Progress,
) -> Result<(), E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
    C: CoderRunner + ?Sized,
    R: ReviewerRunner + ?Sized,
    K: KillSwitch,
{
    if !safe_repo_name(&config.repo) {
        return Err(E2eError::InvalidRepoName(config.repo.clone()));
    }

    begin_step(progress, kill, Step::Setup)?;
    let started = Instant::now();
    let audit = AuditLog::open(&config.state_dir, &config.lab)?;
    let broker_audit = AuditLog::open(&config.state_dir, &config.lab)?;
    let grant = GithubGrant {
        owners: vec![config.owner.clone()],
        actions: vec![
            GithubAction::CreateRepo,
            GithubAction::PushSetupBranch,
            GithubAction::PushTaskBranch,
            GithubAction::OpenPr,
            GithubAction::ReadCiStatus,
            GithubAction::MergePr,
        ],
        max_prs_per_day: config.max_prs_per_day,
        max_pushes_per_day: config.max_pushes_per_day,
    };
    let broker = Broker::open(broker_dir(config), &config.lab, grant, broker_audit)?
        .with_task_branch_prefix(&config.task_branch_prefix);

    fs::create_dir_all(work_dir(config)).map_err(|source| io_error(work_dir(config), source))?;

    setup(config, &broker, &audit, backend, store)?;

    begin_step(progress, kill, Step::Intake)?;
    let (project, spec) = intake(config, &audit)?;
    begin_step(progress, kill, Step::Plan)?;
    let plan = plan_from_spec(&spec);
    let task = plan
        .tasks
        .first()
        .ok_or_else(|| E2eError::Coder("the planner produced no task".to_string()))?;
    audit_plan(&audit, task)?;
    begin_step(progress, kill, Step::Board)?;
    let card = put_on_board(config, &project, task)?;
    let branch = task_branch(config, &card.key);
    create_task_branch(config, &audit, &branch)?;

    let prompt = coder_prompt(&project, &spec, &plan, task, &branch);
    let request = CoderRequest {
        state_dir: config.state_dir.clone(),
        prompt,
        work_dir: work_dir(config),
        scratch_dir: scratch_dir(config),
        timeout: config.coder_timeout,
        max_turns: config.max_coder_turns,
        max_budget_usd: config.max_coder_budget_usd,
    };
    begin_step(progress, kill, Step::Coder)?;
    run_coder(&audit, coder, &request, kill)?;

    let message = task_commit_message(config, task)?;
    begin_step(progress, kill, Step::Commit)?;
    commit(config, &audit, &message)?;
    begin_step(progress, kill, Step::Push)?;
    push_task_branch(&broker, config, &branch, &message, backend, store)?;
    begin_step(progress, kill, Step::Pr)?;
    open_pr(&broker, config, &branch, &message, backend, store)?;

    watch_pr(
        config, &broker, &audit, backend, store, coder, reviewer, kill, progress, &project, &spec,
        &plan, task, &card.key, &branch, started,
    )
}

/// Watches CI, runs the reviewer and merges the pull request into the integration branch.
#[allow(clippy::too_many_arguments)]
fn watch_pr<B, S, C, R, K>(
    config: &E2eConfig,
    broker: &Broker,
    audit: &AuditLog,
    backend: &B,
    store: &S,
    coder: &C,
    reviewer: &R,
    kill: &K,
    progress: &Progress,
    project: &Project,
    spec: &Spec,
    plan: &Plan,
    task: &PlannedTask,
    card_key: &str,
    branch: &str,
    started: Instant,
) -> Result<(), E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
    C: CoderRunner + ?Sized,
    R: ReviewerRunner + ?Sized,
    K: KillSwitch,
{
    let watcher = CiWatch::open(ci_watch_dir(config))?;
    let project_id = project.id.clone();
    let mut failed_sha: Option<String> = None;
    let mut reviewed_sha: Option<String> = None;
    let mut no_run_sha: Option<String> = None;
    let mut no_run_polls = 0usize;

    loop {
        begin_step(progress, kill, Step::Ci)?;
        if started.elapsed() >= config.max_wall_time {
            return Err(E2eError::Budget("run hit its wall-clock limit".to_string()));
        }

        let sha = head_sha(config)?;
        let output = read_ci(config, broker, branch, backend, store)?;
        let parsed_state = parse_ci_runs_with_presence(&output.stdout, &sha)?;
        let Some(state) = parsed_state else {
            if no_run_sha.as_deref() == Some(sha.as_str()) {
                no_run_polls = no_run_polls.saturating_add(1);
            } else {
                no_run_sha = Some(sha.clone());
                no_run_polls = 1;
            }
            audit_ci_status(audit, branch, &sha, CiState::Pending)?;
            if no_run_polls >= config.ci_no_run_poll_limit.max(1) {
                return Err(E2eError::NoCiRunStarted { sha });
            }
            wait_between_polls(kill, config.ci_poll_interval)?;
            continue;
        };
        no_run_sha = None;
        no_run_polls = 0;
        audit_ci_status(audit, branch, &sha, state)?;

        match state {
            CiState::Pending => wait_between_polls(kill, config.ci_poll_interval)?,
            CiState::Failed => {
                if failed_sha.as_deref() == Some(sha.as_str()) {
                    wait_between_polls(kill, config.ci_poll_interval)?;
                    continue;
                }
                failed_sha = Some(sha.clone());
                let log = write_log(
                    config,
                    &format!("ci-{}-{}.log", E2E_PULL_REQUEST, sha),
                    &format!("{}\n{}", output.stdout, output.stderr),
                )?;
                match watcher.decide(
                    &project_id,
                    E2E_PULL_REQUEST,
                    CiState::Failed,
                    false,
                    Some(&log),
                )? {
                    Decision::FixCycle { cycle, log_path } => {
                        audit_fix_cycle(audit, cycle, &log_path)?;
                        fix_with_coder(
                            config, audit, coder, broker, backend, store, project, spec, plan,
                            task, branch, &log_path, "CI", kill, progress,
                        )?;
                    }
                    Decision::Escalate => {
                        return Err(E2eError::FixCyclesExhausted {
                            max: MAX_FIX_CYCLES,
                        });
                    }
                    Decision::Wait | Decision::Done => {
                        return Err(E2eError::Coder(
                            "the CI watcher returned an unexpected decision".to_string(),
                        ));
                    }
                }
            }
            CiState::Green => {
                if reviewed_sha.as_deref() != Some(sha.as_str()) {
                    begin_step(progress, kill, Step::Review)?;
                    move_card(config, card_key, Column::Review, "CI is green")?;
                    let review = run_reviewer(
                        config, audit, reviewer, project, spec, plan, task, branch, kill,
                    )?;
                    reviewed_sha = Some(sha.clone());
                    if review.decision != ReviewDecision::Approved {
                        let log = write_log(
                            config,
                            &format!("review-{}-{}.log", E2E_PULL_REQUEST, sha),
                            &review.stdout,
                        )?;
                        match watcher.decide(
                            &project_id,
                            E2E_PULL_REQUEST,
                            CiState::Failed,
                            false,
                            Some(&log),
                        )? {
                            Decision::FixCycle { cycle, log_path } => {
                                audit_fix_cycle(audit, cycle, &log_path)?;
                                fix_with_coder(
                                    config, audit, coder, broker, backend, store, project, spec,
                                    plan, task, branch, &log_path, "review", kill, progress,
                                )?;
                                continue;
                            }
                            Decision::Escalate => {
                                return Err(E2eError::FixCyclesExhausted {
                                    max: MAX_FIX_CYCLES,
                                });
                            }
                            Decision::Wait | Decision::Done => {
                                return Err(E2eError::Review(
                                    "the CI watcher returned an unexpected decision".to_string(),
                                ));
                            }
                        }
                    }
                }

                match watcher.decide(&project_id, E2E_PULL_REQUEST, CiState::Green, true, None)? {
                    Decision::Done => {
                        begin_step(progress, kill, Step::Merge)?;
                        merge_pr(
                            config, broker, audit, backend, store, project, branch, card_key,
                        )?;
                        watcher.forget(&project_id, E2E_PULL_REQUEST)?;
                        progress.set(Step::Done);
                        return Ok(());
                    }
                    Decision::Wait => wait_between_polls(kill, config.ci_poll_interval)?,
                    Decision::FixCycle { .. } | Decision::Escalate => {
                        return Err(E2eError::Coder(
                            "the CI watcher refused a green result".to_string(),
                        ));
                    }
                }
            }
        }
    }
}

/// Reads the CI runs of the task branch through the broker.
fn read_ci<B, S>(
    config: &E2eConfig,
    broker: &Broker,
    branch: &str,
    backend: &B,
    store: &S,
) -> Result<BackendOutput, E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
{
    let request = GithubRequest {
        action: GithubAction::ReadCiStatus,
        owner: config.owner.clone(),
        repo: config.repo.clone(),
        branch: Some(branch.to_string()),
        base: None,
        force: false,
        delete: false,
        target_is_default_branch: false,
        merge_target: None,
        merge_context: None,
        commit_message: None,
    };
    Ok(broker.execute(&request, &project(), E2E_WORKER, backend, store)?)
}

fn audit_ci_status(
    audit: &AuditLog,
    branch: &str,
    sha: &str,
    state: CiState,
) -> Result<(), E2eError> {
    let status = match state {
        CiState::Pending => "pending",
        CiState::Green => "green",
        CiState::Failed => "failed",
    };
    audit.append(
        Some(E2E_PROJECT),
        Some("ci-watch"),
        AuditKind::ci_result("github", format!("{branch}@{sha}"), status, None),
    )?;
    Ok(())
}

fn audit_fix_cycle(audit: &AuditLog, cycle: u8, log_path: &Path) -> Result<(), E2eError> {
    audit.append(
        Some(E2E_PROJECT),
        Some("ci-watch"),
        AuditKind::command(
            vec![
                "ci-fix".to_string(),
                "cycle".to_string(),
                cycle.to_string(),
                log_path.display().to_string(),
            ],
            Some(0),
            None,
            false,
            0,
        ),
    )?;
    Ok(())
}

/// Runs the reviewer and audits its command and decision.
#[allow(clippy::too_many_arguments)]
fn run_reviewer<R, K>(
    config: &E2eConfig,
    audit: &AuditLog,
    reviewer: &R,
    project: &Project,
    spec: &Spec,
    plan: &Plan,
    task: &PlannedTask,
    branch: &str,
    kill: &K,
) -> Result<ReviewRun, E2eError>
where
    R: ReviewerRunner + ?Sized,
    K: KillSwitch,
{
    enforce_git_safety(audit, &work_dir(config))?;
    audit_bwrap_decision(audit, "reviewer")?;
    let request = ReviewRequest {
        state_dir: config.state_dir.clone(),
        prompt: reviewer_prompt(project, spec, plan, task, branch),
        work_dir: work_dir(config),
        scratch_dir: scratch_dir(config),
        timeout: config.reviewer_timeout,
        max_turns: config.max_reviewer_turns,
        max_budget_usd: config.max_reviewer_budget_usd,
    };
    let run = reviewer.run_with_kill(&request, kill);
    let safety = enforce_git_safety(audit, &request.work_dir);
    audit_reviewer_run(audit, &run, &request.command())?;
    safety?;
    if kill.killed() || run.killed {
        return Err(E2eError::Killed);
    }
    if run.timed_out || run.budget_exhausted {
        return Err(E2eError::Budget(run.failure_reason()));
    }
    if !run.succeeded() {
        return Err(E2eError::Review(run.failure_reason()));
    }
    let approved = run.decision == ReviewDecision::Approved;
    audit_review_decision(audit, approved)?;
    Ok(run)
}

fn audit_reviewer_run(audit: &AuditLog, run: &ReviewRun, argv: &[String]) -> Result<(), E2eError> {
    let mut argv = argv.to_vec();
    if !run.succeeded() {
        append_failure_diagnostic(&mut argv, &run.failure_reason());
    }
    audit.append(
        Some(E2E_PROJECT),
        Some("reviewer"),
        AuditKind::command(
            argv,
            run.exit_code,
            run.signal,
            run.timed_out,
            run.duration_ms,
        ),
    )?;
    Ok(())
}

fn audit_review_decision(audit: &AuditLog, approved: bool) -> Result<(), E2eError> {
    audit.append(
        Some(E2E_PROJECT),
        Some("reviewer"),
        AuditKind::command(
            vec![
                "review".to_string(),
                if approved { "approved" } else { "changes" }.to_string(),
            ],
            Some(0),
            None,
            false,
            0,
        ),
    )?;
    Ok(())
}

/// Hands a failure log or the review feedback to a fresh coder, then commits and pushes.
#[allow(clippy::too_many_arguments)]
fn fix_with_coder<B, S, C, K>(
    config: &E2eConfig,
    audit: &AuditLog,
    coder: &C,
    broker: &Broker,
    backend: &B,
    store: &S,
    project: &Project,
    spec: &Spec,
    plan: &Plan,
    task: &PlannedTask,
    branch: &str,
    log_path: &Path,
    source: &str,
    kill: &K,
    progress: &Progress,
) -> Result<(), E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
    C: CoderRunner + ?Sized,
    K: KillSwitch,
{
    let request = CoderRequest {
        state_dir: config.state_dir.clone(),
        prompt: fix_prompt(project, spec, plan, task, branch, log_path, source),
        work_dir: work_dir(config),
        scratch_dir: scratch_dir(config),
        timeout: config.coder_timeout,
        max_turns: config.max_coder_turns,
        max_budget_usd: config.max_coder_budget_usd,
    };
    begin_step(progress, kill, Step::Coder)?;
    run_coder(audit, coder, &request, kill)?;
    let message = task_commit_message(config, task)?;
    begin_step(progress, kill, Step::Commit)?;
    commit(config, audit, &message)?;
    begin_step(progress, kill, Step::Push)?;
    push_task_branch(broker, config, branch, &message, backend, store)
}

#[allow(clippy::too_many_arguments)]
fn merge_pr<B, S>(
    config: &E2eConfig,
    broker: &Broker,
    audit: &AuditLog,
    backend: &B,
    store: &S,
    project: &Project,
    branch: &str,
    card_key: &str,
) -> Result<(), E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
{
    let context = merge_context(config);
    match merge_decision(&context, &config.integration_branch) {
        Ok(_) => audit_merge_decision(audit, &config.integration_branch, true)?,
        Err(denied) => {
            audit_merge_decision(audit, &config.integration_branch, false)?;
            return Err(E2eError::Merge(denied));
        }
    }

    let request = GithubRequest {
        action: GithubAction::MergePr,
        owner: config.owner.clone(),
        repo: config.repo.clone(),
        branch: Some(branch.to_string()),
        base: None,
        force: false,
        delete: false,
        target_is_default_branch: false,
        merge_target: Some(config.integration_branch.clone()),
        merge_context: Some(context),
        commit_message: None,
    };
    broker.execute(&request, &project.id, E2E_TASK_WORKER, backend, store)?;
    move_card(
        config,
        card_key,
        Column::Done,
        "merged into the integration branch",
    )?;
    Ok(())
}

fn audit_merge_decision(audit: &AuditLog, target: &str, allowed: bool) -> Result<(), E2eError> {
    audit.append(
        Some(E2E_PROJECT),
        Some("lab-coordinator"),
        AuditKind::command(
            vec![
                "merge-decision".to_string(),
                if allowed { "allowed" } else { "denied" }.to_string(),
                target.to_string(),
            ],
            Some(0),
            None,
            false,
            0,
        ),
    )?;
    Ok(())
}

fn move_card(config: &E2eConfig, key: &str, column: Column, reason: &str) -> Result<(), E2eError> {
    let board = Board::open(
        board_dir(config),
        AuditLog::open(&config.state_dir, &config.lab)?,
    )?;
    let actor = Actor::CoCoordinator(project());
    board.move_task(&actor, key, column, reason, Vec::new())?;
    Ok(())
}

/// Commit the task branch points at.
fn head_sha(config: &E2eConfig) -> Result<String, E2eError> {
    let work = work_dir(config);
    let sha = head_sha_of(&work).map_err(|source| io_error(&work, source))?;
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(E2eError::Git {
            command: "git rev-parse HEAD".to_string(),
            status: "ok".to_string(),
            stderr: format!("unexpected commit SHA {sha:?}"),
        });
    }
    Ok(sha)
}

/// Writes a failure log and returns its absolute path.
fn write_log(config: &E2eConfig, name: &str, content: &str) -> Result<PathBuf, E2eError> {
    let dir = config.state_dir.join(LOG_DIR);
    fs::create_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
    let dir = fs::canonicalize(&dir).map_err(|source| io_error(&dir, source))?;
    let path = dir.join(name);
    fs::write(&path, content).map_err(|source| io_error(&path, source))?;
    Ok(path)
}

/// Waits between two CI polls, stopping as soon as the kill switch is set.
fn wait_between_polls<K>(kill: &K, interval: Duration) -> Result<(), E2eError>
where
    K: KillSwitch,
{
    let started = Instant::now();
    loop {
        if kill.killed() {
            return Err(E2eError::Killed);
        }
        let remaining = interval.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(KILL_POLL_INTERVAL));
    }
}

fn setup<B, S>(
    config: &E2eConfig,
    broker: &Broker,
    audit: &AuditLog,
    backend: &B,
    store: &S,
) -> Result<(), E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
{
    prepare_work_tree(config, audit)?;
    validate_root_workflows(&work_dir(config))?;
    create_repository(broker, config, backend, store)?;
    push_setup_branch(
        broker,
        config,
        &config.default_branch,
        None,
        true,
        backend,
        store,
    )?;
    push_setup_branch(
        broker,
        config,
        &config.integration_branch,
        Some(&config.default_branch),
        false,
        backend,
        store,
    )?;
    Ok(())
}

/// Freezes the spec of the work tree and audits the intake.
fn intake(config: &E2eConfig, audit: &AuditLog) -> Result<(Project, Spec), E2eError> {
    let started = Instant::now();
    let source = work_dir(config).join("SPEC.md");
    let text = fs::read_to_string(&source).map_err(|error| io_error(&source, error))?;
    let spec = Spec::parse(&text);
    let issues = lint_spec(&spec);
    if !issues.is_empty() {
        let first = issues
            .first()
            .map(|issue| issue.message.clone())
            .unwrap_or_default();
        return Err(E2eError::Intake(format!(
            "{} lint issue(s): {first}",
            issues.len()
        )));
    }

    let mut project = project_model(config);
    freeze_spec(&config.state_dir, &mut project, &source)?;
    verify_spec(&project)?;
    audit.append(
        Some(E2E_PROJECT),
        Some("intake"),
        AuditKind::command(
            vec![
                "jgf".to_string(),
                "spec".to_string(),
                "freeze".to_string(),
                source.display().to_string(),
            ],
            Some(0),
            None,
            false,
            elapsed_ms(started),
        ),
    )?;
    Ok((project, spec))
}

/// A plan with exactly one task, built deterministically from the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The planned tasks, in order.
    pub tasks: Vec<PlannedTask>,
}

/// One task of a [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTask {
    /// Short title, also used as the commit summary.
    pub title: String,
    /// What the task must deliver.
    pub description: String,
    /// Scope, acceptance criteria, constraints and the actors the task needs.
    pub details: String,
    /// Roles that take part in the task.
    pub actors: Vec<FactoryRole>,
}

/// Builds the one-task plan of `spec`.
pub fn plan_from_spec(spec: &Spec) -> Plan {
    let goal = section_body(spec, "Goal");
    let scope = section_body(spec, "Scope");
    let criteria = section_body(spec, "Acceptance criteria");
    let constraints = section_body(spec, "Constraints");
    let actors = vec![
        FactoryRole::Coder,
        FactoryRole::Tester,
        FactoryRole::Reviewer,
        FactoryRole::GitCoder,
    ];
    let actor_names: Vec<&str> = actors.iter().map(|role| role.as_str()).collect();
    let title = truncate_chars(first_line(&goal), MAX_TITLE_CHARS);
    let details = format!(
        "Scope:\n{scope}\n\nAcceptance criteria:\n{criteria}\n\nConstraints:\n{constraints}\n\nActors: {}",
        actor_names.join(", ")
    );

    Plan {
        tasks: vec![PlannedTask {
            title,
            description: goal,
            details,
            actors,
        }],
    }
}

fn audit_plan(audit: &AuditLog, task: &PlannedTask) -> Result<(), E2eError> {
    audit.append(
        Some(E2E_PROJECT),
        Some("planner"),
        AuditKind::command(
            vec!["jgf".to_string(), "plan".to_string(), task.title.clone()],
            Some(0),
            None,
            false,
            0,
        ),
    )?;
    Ok(())
}

fn put_on_board(
    config: &E2eConfig,
    project: &Project,
    task: &PlannedTask,
) -> Result<jgf_hub::Task, E2eError> {
    let board = Board::open(
        board_dir(config),
        AuditLog::open(&config.state_dir, &config.lab)?,
    )?;
    let actor = Actor::CoCoordinator(project.id.clone());
    let card = board.create(
        &actor,
        project.id.clone(),
        task.title.clone(),
        task.description.clone(),
    )?;
    board.update_details(&actor, &card.key, Some(task.details.clone()))?;
    Ok(board.queue(&actor, &card.key, "the plan is ready")?)
}

fn create_task_branch(config: &E2eConfig, audit: &AuditLog, branch: &str) -> Result<(), E2eError> {
    git(
        &work_dir(config),
        audit,
        &["checkout", "-q", "-b", branch, &config.integration_branch],
    )
}

fn run_coder<C, K>(
    audit: &AuditLog,
    coder: &C,
    request: &CoderRequest,
    kill: &K,
) -> Result<(), E2eError>
where
    C: CoderRunner + ?Sized,
    K: KillSwitch,
{
    enforce_git_safety(audit, &request.work_dir)?;
    audit_bwrap_decision(audit, "coder")?;
    let run = coder.run_with_kill(request, kill);
    let safety = enforce_git_safety(audit, &request.work_dir);
    audit_coder_run(audit, &run, &request.command())?;
    safety?;
    if kill.killed() || run.killed {
        return Err(E2eError::Killed);
    }
    if run.timed_out || run.budget_exhausted {
        return Err(E2eError::Budget(run.failure_reason()));
    }
    if !run.succeeded() {
        return Err(E2eError::Coder(run.failure_reason()));
    }
    Ok(())
}

fn enforce_git_safety(audit: &AuditLog, work: &Path) -> Result<(), E2eError> {
    if let Err(source) = prepare_git_safety(work) {
        audit_git_safety_failure(audit, &[], &source.to_string())?;
        return Err(E2eError::Git {
            command: "git safety check".to_string(),
            status: "refused".to_string(),
            stderr: source.to_string(),
        });
    }
    Ok(())
}

/// Records the filesystem isolation and open-network decision for a Claude child.
fn audit_bwrap_decision(audit: &AuditLog, worker: &str) -> Result<(), E2eError> {
    audit.append(
        Some(E2E_PROJECT),
        Some(worker),
        AuditKind::command(
            vec![
                "bwrap".to_string(),
                "filesystem-isolation".to_string(),
                BWRAP_AUDIT_NOTE.to_string(),
            ],
            None,
            None,
            false,
            0,
        ),
    )?;
    Ok(())
}

fn audit_coder_run(audit: &AuditLog, run: &CoderRun, argv: &[String]) -> Result<(), E2eError> {
    let mut argv = argv.to_vec();
    if !run.succeeded() {
        append_failure_diagnostic(&mut argv, &run.failure_reason());
    }
    audit.append(
        Some(E2E_PROJECT),
        Some("coder"),
        AuditKind::command(
            argv,
            run.exit_code,
            run.signal,
            run.timed_out,
            run.duration_ms,
        ),
    )?;
    Ok(())
}

fn append_failure_diagnostic(argv: &mut Vec<String>, reason: &str) {
    const CHUNK_CHARS: usize = 900;
    let chars: Vec<char> = reason.chars().collect();
    for (index, chunk) in chars.chunks(CHUNK_CHARS).enumerate() {
        argv.push(format!(
            "failure diagnostic {}: {}",
            index + 1,
            chunk.iter().collect::<String>()
        ));
    }
}

fn task_commit_message(config: &E2eConfig, task: &PlannedTask) -> Result<String, E2eError> {
    commit_message(config, &format!("feat: {}", task.title))
}

fn commit_message(config: &E2eConfig, summary: &str) -> Result<String, E2eError> {
    let message = match &config.co_author {
        Some(operator) => format!(
            "{summary}\n\nCo-authored-by: {} <{}>",
            operator.name, operator.email
        ),
        None => summary.to_string(),
    };
    Ok(validate_commit_message(&message)?)
}

fn commit(config: &E2eConfig, audit: &AuditLog, message: &str) -> Result<(), E2eError> {
    let work = work_dir(config);
    git(&work, audit, &["add", "-A"])?;
    git(
        &work,
        audit,
        &["commit", "-q", "--no-gpg-sign", "-m", message],
    )
}

fn push_task_branch<B, S>(
    broker: &Broker,
    config: &E2eConfig,
    branch: &str,
    message: &str,
    backend: &B,
    store: &S,
) -> Result<(), E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
{
    let request = GithubRequest {
        action: GithubAction::PushTaskBranch,
        owner: config.owner.clone(),
        repo: config.repo.clone(),
        branch: Some(branch.to_string()),
        base: Some(config.integration_branch.clone()),
        force: false,
        delete: false,
        target_is_default_branch: false,
        merge_target: None,
        merge_context: None,
        commit_message: Some(message.to_string()),
    };
    broker.execute(&request, &project(), E2E_TASK_WORKER, backend, store)?;
    Ok(())
}

fn open_pr<B, S>(
    broker: &Broker,
    config: &E2eConfig,
    branch: &str,
    message: &str,
    backend: &B,
    store: &S,
) -> Result<(), E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
{
    let context = merge_context(config);
    let request = GithubRequest {
        action: GithubAction::OpenPr,
        owner: config.owner.clone(),
        repo: config.repo.clone(),
        branch: Some(branch.to_string()),
        base: None,
        force: false,
        delete: false,
        target_is_default_branch: false,
        merge_target: None,
        merge_context: Some(context),
        commit_message: Some(message.to_string()),
    };
    broker.execute(&request, &project(), E2E_TASK_WORKER, backend, store)?;
    Ok(())
}

fn project_model(config: &E2eConfig) -> Project {
    Project {
        id: project(),
        name: "E2E Toy".to_string(),
        spec_path: PathBuf::new(),
        spec_sha256: None,
        repos: vec![RepoSpec {
            name: config.repo.clone(),
            remote: Some(format!(
                "https://github.com/{}/{}",
                config.owner, config.repo
            )),
            default_branch: config.default_branch.clone(),
        }],
        stage: config.stage,
        env_profile: EnvProfile::default(),
        recipes: Vec::new(),
    }
}

fn merge_context(config: &E2eConfig) -> MergeContext {
    MergeContext {
        stage: config.stage,
        integration_branch: Some(config.integration_branch.clone()),
        default_branch: config.default_branch.clone(),
        user_approved: false,
    }
}

fn task_branch(config: &E2eConfig, key: &str) -> String {
    format!("{}{}", config.task_branch_prefix, key.to_ascii_lowercase())
}

fn coder_prompt(
    project: &Project,
    spec: &Spec,
    plan: &Plan,
    task: &PlannedTask,
    branch: &str,
) -> String {
    let spec_text = fs::read_to_string(&project.spec_path).unwrap_or_else(|_| render_spec(spec));
    let mut prompt = String::new();
    prompt.push_str("You are the coder of a Dark Factory end-to-end run.\n");
    prompt.push_str("Work only inside this repository and leave the work tree ready to commit.\n");
    prompt.push_str(&format!("Task branch: {branch}\n\n"));
    prompt.push_str("# Specification\n\n");
    prompt.push_str(spec_text.trim());
    prompt.push_str("\n\n# Plan\n\n");
    prompt.push_str(&format!(
        "Task 1: {}\n{}\n\n{}\n",
        task.title, task.description, task.details
    ));
    prompt.push_str("\nActors: ");
    let actors: Vec<&str> = plan
        .tasks
        .iter()
        .flat_map(|task| task.actors.iter().map(|role| role.as_str()))
        .collect();
    prompt.push_str(&actors.join(", "));
    prompt.push_str("\n\nRun the checks named in the acceptance criteria.\n");
    prompt
}

/// Prompt of a fix coder: it names the failure log but never pastes its contents.
fn fix_prompt(
    project: &Project,
    spec: &Spec,
    plan: &Plan,
    task: &PlannedTask,
    branch: &str,
    log_path: &Path,
    source: &str,
) -> String {
    let mut prompt = coder_prompt(project, spec, plan, task, branch);
    prompt.push_str(&format!(
        "\n\n# {source} failure\n\nThe checks failed. Read the failure log at {} and fix the cause.\n",
        log_path.display()
    ));
    prompt.push_str("Keep the change minimal and leave the work tree ready to commit.\n");
    prompt
}

/// Read-only review prompt: it asks for a decision marker and forbids edits.
fn reviewer_prompt(
    project: &Project,
    spec: &Spec,
    plan: &Plan,
    task: &PlannedTask,
    branch: &str,
) -> String {
    let spec_text = fs::read_to_string(&project.spec_path).unwrap_or_else(|_| render_spec(spec));
    let mut prompt = String::new();
    prompt.push_str("You are the reviewer of a Dark Factory end-to-end run.\n");
    prompt.push_str("Review the changes only: do not edit, add or remove any file.\n");
    prompt.push_str(&format!("Branch under review: {branch}\n\n"));
    prompt.push_str("# Specification\n\n");
    prompt.push_str(spec_text.trim());
    prompt.push_str("\n\n# Task\n\n");
    prompt.push_str(&format!(
        "{}\n{}\n\n{}\n",
        task.title, task.description, task.details
    ));
    prompt.push_str(&format!("\nThe plan has {} task(s).\n", plan.tasks.len()));
    prompt.push_str(
        "\nCheck the acceptance criteria and the constraints. Finish with exactly one line:\n\
         REVIEW: APPROVED when the branch is ready to merge, or REVIEW: CHANGES otherwise.\n",
    );
    prompt
}

fn render_spec(spec: &Spec) -> String {
    spec.sections
        .iter()
        .map(|section| format!("## {}\n{}", section.heading, section.body))
        .collect::<Vec<_>>()
        .join("\n")
}

fn section_body(spec: &Spec, name: &str) -> String {
    spec.section(name)
        .map(|section| section.body.trim().to_string())
        .unwrap_or_default()
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn create_repository<B, S>(
    broker: &Broker,
    config: &E2eConfig,
    backend: &B,
    store: &S,
) -> Result<(), E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
{
    let request = GithubRequest {
        action: GithubAction::CreateRepo,
        owner: config.owner.clone(),
        repo: config.repo.clone(),
        branch: None,
        base: None,
        force: false,
        delete: false,
        target_is_default_branch: false,
        merge_target: None,
        merge_context: None,
        commit_message: None,
    };
    broker.execute(&request, &project(), E2E_WORKER, backend, store)?;
    Ok(())
}

fn push_setup_branch<B, S>(
    broker: &Broker,
    config: &E2eConfig,
    branch: &str,
    base: Option<&str>,
    target_is_default_branch: bool,
    backend: &B,
    store: &S,
) -> Result<(), E2eError>
where
    B: GithubBackend + ?Sized,
    S: SecretStore + ?Sized,
{
    let request = GithubRequest {
        action: GithubAction::PushSetupBranch,
        owner: config.owner.clone(),
        repo: config.repo.clone(),
        branch: Some(branch.to_string()),
        base: base.map(str::to_string),
        force: false,
        delete: false,
        target_is_default_branch,
        merge_target: None,
        merge_context: None,
        commit_message: Some(commit_message(config, SETUP_COMMIT)?),
    };
    broker.execute(&request, &project(), E2E_WORKER, backend, store)?;
    Ok(())
}

fn prepare_work_tree(config: &E2eConfig, audit: &AuditLog) -> Result<(), E2eError> {
    if !config.fixture_dir.is_dir() {
        return Err(E2eError::FixtureNotFound(config.fixture_dir.clone()));
    }

    let work = work_dir(config);
    if work.exists() {
        fs::remove_dir_all(&work).map_err(|source| io_error(&work, source))?;
    }
    fs::create_dir_all(&work).map_err(|source| io_error(&work, source))?;
    copy_dir(&config.fixture_dir, &work)?;

    git(
        &work,
        audit,
        &["init", "-q", "--initial-branch", &config.default_branch],
    )?;
    git(&work, audit, &["add", "-A"])?;
    let message = commit_message(config, SETUP_COMMIT)?;
    git(
        &work,
        audit,
        &["commit", "-q", "--no-gpg-sign", "-m", &message],
    )?;
    if config.integration_branch != config.default_branch {
        git(&work, audit, &["branch", &config.integration_branch])?;
    }
    Ok(())
}

fn validate_root_workflows(work: &Path) -> Result<(), E2eError> {
    let workflows = work.join(".github/workflows");
    let entries = fs::read_dir(&workflows).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            E2eError::RootCiWorkflowNotFound(workflows.clone())
        } else {
            io_error(&workflows, error)
        }
    })?;

    for entry in entries {
        let entry = entry.map_err(|error| io_error(&workflows, error))?;
        if entry
            .file_type()
            .map_err(|error| io_error(entry.path(), error))?
            .is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "yml")
        {
            return Ok(());
        }
    }

    Err(E2eError::RootCiWorkflowNotFound(workflows))
}

fn git(work: &Path, audit: &AuditLog, args: &[&str]) -> Result<(), E2eError> {
    git_capture(work, audit, args).map(|_| ())
}

/// Runs `git args`, audits the command and returns its standard output.
fn git_capture(work: &Path, audit: &AuditLog, args: &[&str]) -> Result<String, E2eError> {
    let started = Instant::now();
    let (mut command, _home) = match host_git_command(work, args) {
        Ok(command) => command,
        Err(source) => {
            audit_git_safety_failure(audit, args, &source.to_string())?;
            return Err(E2eError::Git {
                command: format!("git {}", audit_git_args(args).join(" ")),
                status: "refused".to_string(),
                stderr: source.to_string(),
            });
        }
    };
    let output = command
        .env("GIT_AUTHOR_NAME", "Dark Factory")
        .env("GIT_AUTHOR_EMAIL", "factory@example.invalid")
        .env("GIT_COMMITTER_NAME", "Dark Factory")
        .env("GIT_COMMITTER_EMAIL", "factory@example.invalid")
        .output()
        .map_err(|source| io_error(work, source))?;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let argv: Vec<String> = std::iter::once("git".to_string())
        .chain(audit_git_args(args))
        .collect();
    audit.append(
        Some(E2E_PROJECT),
        Some(E2E_WORKER),
        AuditKind::command(argv, output.status.code(), None, false, duration_ms),
    )?;

    if !output.status.success() {
        return Err(E2eError::Git {
            command: format!("git {}", audit_git_args(args).join(" ")),
            status: output.status.to_string(),
            stderr: redact_commit_emails(String::from_utf8_lossy(&output.stderr).trim(), args),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn audit_git_args(args: &[&str]) -> Vec<String> {
    let mut redact_next = false;
    args.iter()
        .map(|arg| {
            if redact_next {
                redact_next = false;
                return "<commit message>".to_string();
            }
            if *arg == "-m" || *arg == "--message" {
                redact_next = true;
                return (*arg).to_string();
            }
            if arg.starts_with("--message=") {
                return "--message=<commit message>".to_string();
            }
            (*arg).to_string()
        })
        .collect()
}

fn redact_commit_emails(stderr: &str, args: &[&str]) -> String {
    let mut redacted = stderr.to_string();
    for pair in args.windows(2) {
        if !matches!(pair[0], "-m" | "--message") {
            continue;
        }
        let message = pair[1];
        for line in message.lines() {
            let Some((marker, trailer)) = line.split_once(':') else {
                continue;
            };
            if !marker.trim().eq_ignore_ascii_case("co-authored-by") {
                continue;
            }
            let Some((_, email)) = trailer.rsplit_once('<') else {
                continue;
            };
            let Some(email) = email.strip_suffix('>') else {
                continue;
            };
            if !email.is_empty() {
                redacted = redacted.replace(email, "[redacted]");
            }
        }
    }
    redacted
}

/// Builds a host Git command with a clean environment and repository configuration.
fn host_git_command(work: &Path, args: &[&str]) -> io::Result<(Command, tempfile::TempDir)> {
    let work = fs::canonicalize(work)?;
    let git_dir = work.join(".git");
    let initializes_work_tree = args.first() == Some(&"init")
        && matches!(
            fs::symlink_metadata(&git_dir),
            Err(ref error) if error.kind() == io::ErrorKind::NotFound
        );
    if !initializes_work_tree {
        prepare_git_safety(&work)?;
    }
    let ceiling = work.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "work tree has no parent for GIT_CEILING_DIRECTORIES",
        )
    })?;

    let home = tempfile::Builder::new()
        .prefix("jgf-e2e-git-home-")
        .tempdir()?;
    let mut command = Command::new("git");
    command.env_clear();
    for (key, value) in env::vars_os() {
        let allowed = match key.to_str() {
            Some("PATH" | "LANG") => true,
            Some(key) => key.starts_with("LC_"),
            None => false,
        };
        if allowed {
            command.env(key, value);
        }
    }
    command
        .env("HOME", home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GIT_DIR", &git_dir)
        .env("GIT_WORK_TREE", &work)
        .env("GIT_CEILING_DIRECTORIES", ceiling)
        .env("GIT_COMMON_DIR", &git_dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "diff.external=",
        ])
        .args(args)
        .current_dir(work);
    Ok((command, home))
}

/// Restores repository metadata the child could have changed and rejects executable attributes.
fn prepare_git_safety(work: &Path) -> io::Result<()> {
    restore_git_config(work)?;
    audit_gitattributes(work)
}

fn restore_git_config(work: &Path) -> io::Result<()> {
    let git_dir = work.join(".git");
    let metadata = fs::symlink_metadata(&git_dir).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("required work-tree .git directory is missing: {error}"),
            )
        } else {
            error
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            ".git must be a real directory",
        ));
    }

    for name in [
        "commondir",
        "gitdir",
        "config.worktree",
        "worktrees",
        "hooks",
    ] {
        remove_git_path(&git_dir.join(name))?;
    }
    remove_git_path(&git_dir.join("config"))?;
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n",
    )?;
    let info_dir = git_dir.join("info");
    match fs::symlink_metadata(&info_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            remove_git_path(&info_dir)?;
            fs::create_dir(&info_dir)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&info_dir)?,
        Err(error) => return Err(error),
    }
    remove_git_path(&info_dir.join("attributes"))
}

fn remove_git_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn audit_gitattributes(work: &Path) -> io::Result<()> {
    let mut directories = vec![work.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let file_type = entry.file_type()?;
            if file_type.is_dir() && name != ".git" {
                directories.push(path);
                continue;
            }
            if name != ".gitattributes" {
                continue;
            }
            if file_type.is_symlink() || !file_type.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "unsafe .gitattributes path {} is not a regular file",
                        path.display()
                    ),
                ));
            }
            let contents = fs::read_to_string(&path)?;
            for (index, line) in contents.lines().enumerate() {
                if line.trim_start().starts_with('#') {
                    continue;
                }
                for attribute in line.split_whitespace().skip(1) {
                    let attribute = attribute.trim_start_matches(['-', '!']);
                    let name = attribute
                        .split_once('=')
                        .map_or(attribute, |(name, _)| name);
                    if matches!(name, "filter" | "diff" | "textconv") {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "unsafe .gitattributes at {}:{} sets {name}; host Git refuses filter, diff, and textconv attributes",
                                path.display(),
                                index + 1
                            ),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn audit_git_safety_failure(audit: &AuditLog, args: &[&str], reason: &str) -> Result<(), E2eError> {
    let mut argv = vec!["git".to_string(), "safety-check".to_string()];
    argv.extend(audit_git_args(args));
    argv.push(format!("refused: {reason}"));
    audit.append(
        Some(E2E_PROJECT),
        Some(E2E_WORKER),
        AuditKind::command(argv, None, None, false, 0),
    )?;
    Ok(())
}

fn copy_dir(source: &Path, dest: &Path) -> Result<(), E2eError> {
    let entries = fs::read_dir(source).map_err(|error| io_error(source, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_error(source, error))?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if IGNORED_NAMES.contains(&name.as_str()) {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| io_error(&path, error))?;
        let target = dest.join(&name);
        if file_type.is_dir() {
            fs::create_dir_all(&target).map_err(|error| io_error(&target, error))?;
            copy_dir(&path, &target)?;
        } else if file_type.is_file() {
            fs::copy(&path, &target).map_err(|error| io_error(&target, error))?;
        }
    }
    Ok(())
}

fn project() -> ProjectId {
    ProjectId::parse(E2E_PROJECT).expect("the e2e project id is valid")
}

fn safe_repo_name(repo: &str) -> bool {
    !repo.is_empty() && repo != "." && repo != ".." && !repo.contains('/') && !repo.contains('\\')
}

fn io_error(path: impl Into<PathBuf>, source: io::Error) -> E2eError {
    E2eError::Io {
        path: path.into(),
        source,
    }
}

/// Name of the GitHub token in the lab secret store.
fn token_key() -> SecretKey {
    SecretKey::parse(GITHUB_TOKEN_KEY).expect("the token key constant is valid")
}

/// A secret store used by dry runs so the broker's token lookup succeeds without touching
/// the host store. The fake backend never sends the placeholder anywhere.
#[derive(Default)]
struct MemorySecretStore {
    values: Mutex<Vec<(SecretScope, SecretKey, Secret)>>,
}

impl MemorySecretStore {
    fn dry_run() -> Self {
        let store = Self::default();
        store
            .set(
                &SecretScope::Lab,
                &token_key(),
                Secret::new("dry-run-token"),
            )
            .expect("in-memory store accepts values");
        store
    }

    fn values(&self) -> std::sync::MutexGuard<'_, Vec<(SecretScope, SecretKey, Secret)>> {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, scope: &SecretScope, key: &SecretKey) -> Result<Option<Secret>, ModelError> {
        Ok(self
            .values()
            .iter()
            .find(|(stored_scope, stored_key, _)| stored_scope == scope && stored_key == key)
            .map(|(_, _, value)| value.clone()))
    }

    fn set(&self, scope: &SecretScope, key: &SecretKey, secret: Secret) -> Result<(), ModelError> {
        let mut values = self.values();
        values.retain(|(stored_scope, stored_key, _)| stored_scope != scope || stored_key != key);
        values.push((scope.clone(), key.clone(), secret));
        Ok(())
    }

    fn remove(&self, scope: &SecretScope, key: &SecretKey) -> Result<(), ModelError> {
        self.values()
            .retain(|(stored_scope, stored_key, _)| stored_scope != scope || stored_key != key);
        Ok(())
    }

    fn list_keys(&self, scope: &SecretScope) -> Result<Vec<SecretKey>, ModelError> {
        let mut keys: Vec<SecretKey> = self
            .values()
            .iter()
            .filter(|(stored_scope, _, _)| stored_scope == scope)
            .map(|(_, stored_key, _)| stored_key.clone())
            .collect();
        keys.sort();
        Ok(keys)
    }
}

/// A [`FakeBackend`] that answers CI reads with a scripted run list.
///
/// It is the dry-run backend and the test double for the post-pull-request steps: every call
/// is still recorded by the wrapped `FakeBackend`, but `ReadCiStatus` returns a run whose
/// `headSha` is the real HEAD of the work tree, so the CI parser accepts it.
struct DryRunBackend {
    inner: FakeBackend,
    work_dir: PathBuf,
    ci: Mutex<VecDeque<Option<CiState>>>,
}

impl DryRunBackend {
    fn new(work_dir: PathBuf, ci: impl IntoIterator<Item = CiState>) -> Self {
        Self::with_runs(work_dir, ci.into_iter().map(Some))
    }

    fn with_runs(work_dir: PathBuf, ci: impl IntoIterator<Item = Option<CiState>>) -> Self {
        Self {
            inner: FakeBackend::new(),
            work_dir,
            ci: Mutex::new(ci.into_iter().collect()),
        }
    }

    #[cfg(test)]
    fn calls(&self) -> Vec<jgf_hub::broker_exec::testing::RecordedCall> {
        self.inner.calls()
    }

    fn next_ci(&self) -> Option<CiState> {
        self.ci
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or(Some(CiState::Green))
    }
}

impl GithubBackend for DryRunBackend {
    fn run(&self, op: &BackendOp, token: &Secret) -> Result<BackendOutput, BackendError> {
        let output = self.inner.run(op, token)?;
        if let BackendOp::ReadCiStatus { .. } = op {
            let sha = head_sha_of(&self.work_dir).map_err(|error| BackendError::Failed {
                program: "git".to_string(),
                status: "failed".to_string(),
                stdout: String::new(),
                stderr: error.to_string(),
            })?;
            return Ok(BackendOutput {
                action: output.action,
                stdout: ci_json(&sha, self.next_ci()),
                stderr: String::new(),
            });
        }
        Ok(output)
    }
}

fn head_sha_of(work: &Path) -> io::Result<String> {
    let (mut command, _home) = host_git_command(work, &["rev-parse", "HEAD"])?;
    let output = command.output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ci_json(sha: &str, state: Option<CiState>) -> String {
    match state {
        None => "[]".to_string(),
        Some(CiState::Pending) => format!(
            r#"[{{"status":"in_progress","conclusion":null,"databaseId":1,"headSha":"{sha}"}}]"#
        ),
        Some(CiState::Green) => format!(
            r#"[{{"status":"completed","conclusion":"success","databaseId":1,"headSha":"{sha}"}}]"#
        ),
        Some(CiState::Failed) => format!(
            r#"[{{"status":"completed","conclusion":"failure","databaseId":1,"headSha":"{sha}"}}]"#
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, path::Path};

    use jgf_core::{AuditEvent, AuditKind, ProjectId, registry::load_project};
    use jgf_hub::BackendOp;
    use tempfile::TempDir;

    use super::*;

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/toy")
    }

    fn config(root: &TempDir) -> E2eConfig {
        let mut config = E2eConfig::new(
            "acme",
            "jgf-e2e-toy-test",
            root.path().to_path_buf(),
            fixture_dir(),
        );
        config.disable_co_author();
        config
    }

    fn audit_log(config: &E2eConfig) -> AuditLog {
        AuditLog::open(&config.state_dir, latest_run_lab(&config.state_dir))
            .expect("open the latest run's audit log")
    }

    /// A fake host home holding a placeholder credentials file the tests never read.
    fn fake_home(root: &Path) -> PathBuf {
        let home = root.join("host-home");
        fs::create_dir_all(home.join(".claude")).expect("host claude dir");
        fs::write(
            home.join(".claude").join(".credentials.json"),
            b"placeholder",
        )
        .expect("fake credentials");
        home
    }

    fn ci_backend(config: &E2eConfig, ci: impl IntoIterator<Item = CiState>) -> DryRunBackend {
        DryRunBackend::new(work_dir(config), ci)
    }

    fn ci_backend_with_runs(
        config: &E2eConfig,
        ci: impl IntoIterator<Item = Option<CiState>>,
    ) -> DryRunBackend {
        DryRunBackend::with_runs(work_dir(config), ci)
    }

    fn run_dry<B, C, R>(config: &E2eConfig, backend: &B, coder: &C, reviewer: &R) -> RunOutcome
    where
        B: GithubBackend + ?Sized,
        C: CoderRunner + ?Sized,
        R: ReviewerRunner + ?Sized,
    {
        run_with(
            config,
            backend,
            &MemorySecretStore::dry_run(),
            coder,
            reviewer,
            &NoKillSwitch,
        )
    }

    #[test]
    fn default_push_cap_is_one_thousand() {
        let root = TempDir::new().expect("temp dir");
        assert_eq!(config(&root).max_pushes_per_day, 1000);
    }

    /// A backend that creates the kill file when CI is first read, so the run is killed while
    /// it waits between polls.
    struct KillingBackend {
        inner: DryRunBackend,
        kill_path: PathBuf,
    }

    #[derive(Clone, Copy)]
    enum GitAttack {
        Hook,
        Fsmonitor,
        Filter,
        Commondir,
    }

    struct GitAttackCoder {
        attack: GitAttack,
        marker: PathBuf,
    }

    impl GitAttackCoder {
        fn write_marker_script(&self, path: &Path) {
            fs::write(
                path,
                format!("#!/bin/sh\nprintf ran > '{}'\n", self.marker.display()),
            )
            .expect("write marker script");
            let mut permissions = fs::metadata(path).expect("script metadata").permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(path, permissions).expect("make script executable");
        }
    }

    impl CoderRunner for GitAttackCoder {
        fn run(&self, request: &CoderRequest) -> CoderRun {
            let git_dir = request.work_dir.join(".git");
            match self.attack {
                GitAttack::Hook => {
                    let hooks = git_dir.join("hooks");
                    fs::create_dir_all(&hooks).expect("create hooks directory");
                    self.write_marker_script(&hooks.join("pre-commit"));
                }
                GitAttack::Fsmonitor => {
                    let script = request.work_dir.join("fsmonitor.sh");
                    self.write_marker_script(&script);
                    fs::write(
                        git_dir.join("config"),
                        format!("[core]\n\tfsmonitor = {}\n", script.display()),
                    )
                    .expect("write fsmonitor config");
                }
                GitAttack::Filter => {
                    let script = request.work_dir.join("filter-clean.sh");
                    self.write_marker_script(&script);
                    fs::write(request.work_dir.join(".gitattributes"), "* filter=x\n")
                        .expect("write gitattributes");
                    fs::write(
                        git_dir.join("config"),
                        format!("[filter \"x\"]\n\tclean = {}\n", script.display()),
                    )
                    .expect("write filter config");
                }
                GitAttack::Commondir => {
                    let evil = request.work_dir.join("evil");
                    fs::create_dir_all(evil.join("info")).expect("create fake common dir");
                    let script = evil.join("filter-clean.sh");
                    self.write_marker_script(&script);
                    fs::write(git_dir.join("commondir"), "../evil\n").expect("write commondir");
                    fs::write(git_dir.join("gitdir"), "../evil/.git\n").expect("write gitdir");
                    fs::write(
                        git_dir.join("config.worktree"),
                        "[unknown]\n\tvalue = injected\n",
                    )
                    .expect("write worktree config");
                    fs::create_dir_all(git_dir.join("worktrees/evil"))
                        .expect("create linked worktree metadata");
                    fs::write(git_dir.join("info/attributes"), "* filter=x\n")
                        .expect("write repository attributes");
                    fs::write(
                        git_dir.join("config"),
                        "[include]\n\tpath = ../evil/config\n\
                         [includeIf \"onbranch:*\"]\n\tpath = ../evil/config\n\
                         [unknown]\n\tvalue = injected\n",
                    )
                    .expect("write include config");
                    fs::write(
                        evil.join("config"),
                        format!("[filter \"x\"]\n\tclean = \"{}\"\n", script.display()),
                    )
                    .expect("write common-dir filter config");
                    fs::write(evil.join("info/attributes"), "* filter=x\n")
                        .expect("write common-dir attributes");
                }
            }
            fs::write(request.work_dir.join("child-output.txt"), "edited\n")
                .expect("write child edit");
            CoderRun {
                exit_code: Some(0),
                ..CoderRun::default()
            }
        }
    }

    struct DeleteGitCoder;

    impl CoderRunner for DeleteGitCoder {
        fn run(&self, request: &CoderRequest) -> CoderRun {
            fs::remove_dir_all(request.work_dir.join(".git")).expect("delete work-tree git dir");
            CoderRun {
                exit_code: Some(0),
                ..CoderRun::default()
            }
        }
    }

    impl GithubBackend for KillingBackend {
        fn run(&self, op: &BackendOp, token: &Secret) -> Result<BackendOutput, BackendError> {
            let output = self.inner.run(op, token)?;
            if matches!(op, BackendOp::ReadCiStatus { .. }) {
                fs::write(&self.kill_path, "killed\n").expect("write the kill file");
            }
            Ok(output)
        }
    }

    /// Step recorded by the terminal outcome event.
    fn outcome_step(events: &[AuditEvent]) -> Option<String> {
        events.iter().rev().find_map(|event| {
            let AuditKind::Command { argv, .. } = &event.kind else {
                return None;
            };
            if argv.first().map(String::as_str) != Some(OUTCOME_MARKER) {
                return None;
            }
            argv.get(2).cloned()
        })
    }

    fn github_events(config: &E2eConfig) -> Vec<(String, Option<String>, bool, String)> {
        let log = audit_log(config);
        log.read_all()
            .expect("read audit log")
            .into_iter()
            .filter_map(|event| match event.kind {
                AuditKind::GithubAction {
                    action,
                    branch,
                    allowed,
                    reason,
                    ..
                } => Some((action, branch, allowed, reason)),
                _ => None,
            })
            .collect()
    }

    fn branch_exists(work: &Path, branch: &str) -> bool {
        let reference = format!("refs/heads/{branch}");
        host_git_command(work, &["rev-parse", "--verify", "--quiet", &reference])
            .and_then(|(mut command, _home)| command.output())
            .is_ok_and(|output| output.status.success())
    }

    fn describe(event: &AuditEvent) -> String {
        match &event.kind {
            AuditKind::GithubAction { action, .. } => format!("github:{action}"),
            AuditKind::BoardMove { to, .. } => format!("board:{to}"),
            AuditKind::Command { argv, .. } => format!(
                "command:{}:{}",
                argv.first().map(String::as_str).unwrap_or(""),
                argv.get(1).map(String::as_str).unwrap_or("")
            ),
            AuditKind::CiResult { status, .. } => format!("ci:{status}"),
            other => format!("other:{}", other.tag()),
        }
    }

    /// Steps from spec intake onwards; the repository setup precedes them.
    fn flow_steps(config: &E2eConfig) -> Vec<String> {
        let log = audit_log(config);
        let events = log.read_all().expect("read audit log");
        let intake = events
            .iter()
            .position(|event| {
                matches!(&event.kind, AuditKind::Command { argv, .. }
                if argv.first().map(String::as_str) == Some("jgf")
                    && argv.get(1).map(String::as_str) == Some("spec"))
            })
            .expect("intake is audited");
        events[intake..].iter().map(describe).collect()
    }

    fn frozen_spec_path(config: &E2eConfig) -> PathBuf {
        config
            .state_dir
            .join("projects")
            .join("e2e-toy")
            .join("spec.md")
    }

    #[test]
    fn a_dry_run_codes_fixes_ci_reviews_and_merges() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Failed, CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::Delivered);

        let calls = backend.calls();
        assert_eq!(calls.len(), 9, "setup, PR, one fix cycle and the merge");
        assert!(matches!(calls[0].op, BackendOp::CreateRepo { .. }));
        assert!(matches!(
            &calls[1].op,
            BackendOp::PushSetupBranch { branch, base: None, .. } if branch == "main"
        ));
        assert!(matches!(
            &calls[2].op,
            BackendOp::PushSetupBranch { branch, base: Some(base), .. }
                if branch == "develop" && base == "main"
        ));
        assert!(matches!(
            &calls[3].op,
            BackendOp::PushTaskBranch { branch, base, commit_message, .. }
                if branch == "jgt/e2e-toy-1"
                    && base == "develop"
                    && commit_message.as_deref() == Some("feat: Let a user remove a note that is no longer needed.")
        ));
        assert!(matches!(
            &calls[4].op,
            BackendOp::OpenPr { head: Some(head), base: Some(base), title, .. }
                if head == "jgt/e2e-toy-1"
                    && base == "develop"
                    && title.contains("remove a note")
        ));
        assert!(matches!(
            &calls[5].op,
            BackendOp::ReadCiStatus { branch: Some(branch), .. } if branch == "jgt/e2e-toy-1"
        ));
        assert!(matches!(
            &calls[6].op,
            BackendOp::PushTaskBranch { branch, .. } if branch == "jgt/e2e-toy-1"
        ));
        assert!(matches!(&calls[7].op, BackendOp::ReadCiStatus { .. }));
        assert!(matches!(
            &calls[8].op,
            BackendOp::MergePr { branch: Some(branch), merge_target, .. }
                if branch == "jgt/e2e-toy-1" && merge_target == "develop"
        ));

        let requests = coder.requests();
        assert_eq!(requests.len(), 2, "the initial coder and one fix cycle");
        assert!(requests[0].prompt.contains("DELETE /notes/{id}"));
        assert!(requests[0].prompt.contains("jgt/e2e-toy-1"));
        assert_eq!(requests[0].max_turns, config.max_coder_turns);
        assert_eq!(requests[0].max_budget_usd, config.max_coder_budget_usd);
        assert!(requests[1].prompt.contains("failure log at"));
        assert!(!requests[1].prompt.contains("dry-run-token"));

        assert_eq!(reviewer.requests().len(), 1);
        assert!(reviewer.requests()[0].prompt.contains("REVIEW: APPROVED"));

        assert_eq!(
            flow_steps(&config),
            vec![
                "command:jgf:spec",
                "command:jgf:plan",
                "board:on_queue",
                "command:git:checkout",
                "command:bwrap:filesystem-isolation",
                "command:claude:-p",
                "command:git:add",
                "command:git:commit",
                "github:push_task_branch",
                "github:open_pr",
                "github:read_ci_status",
                "ci:failed",
                "command:ci-fix:cycle",
                "command:bwrap:filesystem-isolation",
                "command:claude:-p",
                "command:git:add",
                "command:git:commit",
                "github:push_task_branch",
                "github:read_ci_status",
                "ci:green",
                "board:review",
                "command:bwrap:filesystem-isolation",
                "command:claude:-p",
                "command:review:approved",
                "command:merge-decision:allowed",
                "github:merge_pr",
                "board:done",
                "command:e2e-outcome:delivered",
            ]
        );

        let log = audit_log(&config);
        assert!(log.verify().expect("verify audit log").is_none());

        let spec_path = frozen_spec_path(&config);
        assert!(spec_path.is_file());
        let project = load_project(&config.state_dir, &ProjectId::parse("e2e-toy").unwrap())
            .expect("the frozen project is saved");
        verify_spec(&project).expect("the frozen spec matches its hash");
        assert_eq!(project.spec_path, spec_path);

        let work = work_dir(&config);
        assert!(work.join("SPEC.md").is_file());
        assert!(branch_exists(&work, "main"));
        assert!(branch_exists(&work, "develop"));
        assert!(branch_exists(&work, "jgt/e2e-toy-1"));
        assert!(work.join(crate::coder::FAKE_EDIT_FILE).is_file());
    }

    #[test]
    fn a_dry_run_adds_the_injected_operator_to_every_commit_without_auditing_email() {
        let root = TempDir::new().expect("temp dir");
        let mut config = config(&root);
        let email = "operator-private@example.invalid";
        let trailer = format!("Co-authored-by: Human Operator <{email}>");
        config.set_co_author_override(&format!("Human Operator <{email}>"));
        let backend = ci_backend(&config, [CiState::Failed, CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        assert_eq!(
            run_dry(&config, &backend, &coder, &reviewer),
            RunOutcome::Delivered
        );

        let audit = audit_log(&config);
        let messages = git_capture(
            &work_dir(&config),
            &audit,
            &["log", "--format=%B", "-n", "3"],
        )
        .expect("read generated commit messages");
        assert_eq!(messages.matches(&trailer).count(), 3);
        let identities = git_capture(
            &work_dir(&config),
            &audit,
            &["log", "--format=%an%x09%ae%x09%cn%x09%ce", "-n", "3"],
        )
        .expect("read commit identities");
        assert!(identities.lines().all(|identity| {
            identity
                == "Dark Factory\tfactory@example.invalid\tDark Factory\tfactory@example.invalid"
        }));

        let calls = backend.calls();
        for call in &calls[1..3] {
            let BackendOp::PushSetupBranch { commit_message, .. } = &call.op else {
                panic!("expected setup branch push");
            };
            assert!(
                commit_message
                    .as_deref()
                    .is_some_and(|message| message.contains(&trailer))
            );
        }

        let events = audit.read_all().expect("read audit log");
        let audit_text = format!("{events:?}");
        assert!(audit_text.contains("co-author trailer added"));
        assert!(!audit_text.contains(email));
        assert!(events.iter().any(|event| {
            matches!(&event.kind, AuditKind::Note { text } if text == "push cap: 1000 per day")
        }));
        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(report.terminal.as_deref(), Some("delivered"));
    }

    #[test]
    fn commit_audit_and_error_text_redact_the_co_author_email() {
        let email = "operator-private@example.invalid";
        let message = format!("feat: add a feature\n\nCo-authored-by: Human Operator <{email}>");
        let args = ["commit", "-m", message.as_str()];
        assert!(!audit_git_args(&args).join(" ").contains(email));
        let stderr = format!("git rejected this message: {message}");
        let redacted = redact_commit_emails(&stderr, &args);
        assert!(!redacted.contains(email));
        assert!(redacted.contains("[redacted]"));

        let root = TempDir::new().expect("temp dir");
        let mut config = config(&root);
        config.set_co_author_override(&format!("Human Operator <{email}>"));
        let outcome =
            redact_outcome_email(RunOutcome::failed(format!("failure: {email}")), &config);
        assert!(!outcome.to_string().contains(email));
    }

    #[test]
    fn missing_and_invalid_operator_values_are_skipped_without_real_git_config() {
        assert!(matches!(
            operator_from_git_values(GitConfigValue::Missing, GitConfigValue::Missing),
            OperatorRead::Missing
        ));
        assert!(matches!(
            operator_from_git_values(
                GitConfigValue::Present("Invalid\nName".to_string()),
                GitConfigValue::Present("operator-private@example.invalid".to_string()),
            ),
            OperatorRead::Invalid
        ));
        assert!(Operator::parse("Invalid <operator-private@example.invalid\n>").is_none());

        let root = TempDir::new().expect("temp dir");
        let mut unresolved = E2eConfig::new(
            "acme",
            "jgf-e2e-toy-test",
            root.path().to_path_buf(),
            fixture_dir(),
        );
        assert_eq!(
            resolve_global_operator(&mut unresolved, OperatorRead::Missing),
            "co-author trailer skipped: missing global identity"
        );
        assert!(unresolved.co_author.is_none());
        assert_eq!(
            resolve_global_operator(&mut unresolved, OperatorRead::Invalid),
            "co-author trailer skipped: invalid global identity"
        );

        let mut config = config(&root);
        config.set_co_author_override("Invalid\nName <operator-private@example.invalid>");
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        assert_eq!(
            run_dry(&config, &backend, &coder, &reviewer),
            RunOutcome::Delivered
        );
        let audit = audit_log(&config);
        let events = audit.read_all().expect("read audit log");
        let audit_text = format!("{events:?}");
        assert!(audit_text.contains("co-author trailer skipped: invalid override"));
        assert!(!audit_text.contains("operator-private@example.invalid"));
        let messages = git_capture(
            &work_dir(&config),
            &audit,
            &["log", "--format=%B", "-n", "2"],
        )
        .expect("read generated commit messages");
        assert!(!messages.contains("Co-authored-by:"));
        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
    }

    #[test]
    fn child_git_configuration_cannot_run_host_scripts() {
        for (attack, rejected) in [
            (GitAttack::Hook, false),
            (GitAttack::Fsmonitor, false),
            (GitAttack::Filter, true),
            (GitAttack::Commondir, false),
        ] {
            let root = TempDir::new().expect("temp dir");
            let config = config(&root);
            let work = work_dir(&config);
            let marker = root.path().join("host-script-ran");
            let backend = ci_backend(&config, [CiState::Green]);
            let coder = GitAttackCoder {
                attack,
                marker: marker.clone(),
            };
            let reviewer = FakeReviewerRunner::approving();

            let outcome = run_dry(&config, &backend, &coder, &reviewer);

            assert!(!marker.exists(), "child-controlled git script ran");
            if matches!(attack, GitAttack::Commondir) {
                assert!(!work.join(".git/commondir").exists());
                assert!(!work.join(".git/gitdir").exists());
                assert!(!work.join(".git/config.worktree").exists());
                assert!(!work.join(".git/worktrees").exists());
                assert!(!work.join(".git/info/attributes").exists());
                let config =
                    fs::read_to_string(work.join(".git/config")).expect("restored git config");
                assert!(!config.contains("include"));
                assert!(!config.contains("unknown"));
                assert!(work.join("evil/config").is_file());
                assert!(work.join("evil/info/attributes").is_file());
            }
            if rejected {
                let reason = match outcome {
                    RunOutcome::Failed { reason } => reason,
                    other => panic!("expected unsafe attributes to fail the run, got {other:?}"),
                };
                assert!(reason.contains("unsafe .gitattributes"), "{reason}");
                let events = audit_log(&config).read_all().expect("read audit log");
                assert!(
                    events.iter().any(|event| match &event.kind {
                        AuditKind::Command { argv, .. } => argv
                            .iter()
                            .any(|argument| argument.contains("unsafe .gitattributes")),
                        _ => false,
                    }),
                    "unsafe attribute refusal was not audited"
                );
            } else {
                assert_eq!(outcome, RunOutcome::Delivered);
                assert!(
                    !work.join(".git/hooks").exists(),
                    "host-side Git must drop child-created hooks"
                );
                let config =
                    fs::read_to_string(work.join(".git/config")).expect("read restored git config");
                assert!(!config.contains("fsmonitor.sh"));
            }
        }
    }

    #[test]
    fn deleting_git_fails_closed_without_falling_back_to_an_ancestor_repository() {
        let root = TempDir::new().expect("temp dir");
        let init = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .output()
            .expect("initialize parent repository");
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        let config = config(&root);
        let work = work_dir(&config);
        let backend = ci_backend(&config, [CiState::Green]);
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &DeleteGitCoder, &reviewer);

        let reason = match outcome {
            RunOutcome::Failed { reason } => reason,
            other => panic!("expected missing .git to fail closed, got {other:?}"),
        };
        assert!(reason.contains(".git"), "{reason}");
        assert!(!work.join(".git").exists());
        assert!(host_git_command(&work, &["status"]).is_err());

        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(root.path())
            .output()
            .expect("inspect parent repository");
        assert!(status.status.success());
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(
            status.lines().all(|line| line.starts_with("?? ")),
            "host Git staged files in the parent repository: {status}"
        );
    }

    #[test]
    fn a_coder_failure_ends_failed_before_any_push() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::new(crate::coder::FakeCoderBehavior::Fail);
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        let reason = match outcome {
            RunOutcome::Failed { reason } => reason,
            other => panic!("expected failed, got {other:?}"),
        };
        assert!(reason.contains("fake coder failed"), "{reason}");
        assert_eq!(backend.calls().len(), 3, "only repository setup ran");
        assert!(
            !github_events(&config)
                .iter()
                .any(|(action, ..)| action == "push_task_branch" || action == "open_pr")
        );
        let events = audit_log(&config).read_all().expect("read audit log");
        assert!(
            events.iter().any(|event| matches!(
                &event.kind,
                AuditKind::Command { argv, exit_code: Some(1), .. }
                    if argv.iter().any(|arg| arg.contains("fake coder failed"))
            )),
            "the coder failure reason is recorded in its command audit event"
        );
        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(report.terminal.as_deref(), Some("failed"));
    }

    #[test]
    fn setup_rejects_a_fixture_without_a_root_workflow_before_creating_a_repo() {
        let root = TempDir::new().expect("temp dir");
        let fixture = root.path().join("fixture");
        fs::create_dir_all(fixture.join("api/.github/workflows")).expect("nested workflow dir");
        fs::write(fixture.join("SPEC.md"), "# Toy\n").expect("fixture spec");
        fs::write(
            fixture.join("api/.github/workflows/ci.yml"),
            "name: Nested CI\n",
        )
        .expect("nested workflow");

        let mut config = config(&root);
        config.fixture_dir = fixture;
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        let reason = match outcome {
            RunOutcome::Failed { reason } => reason,
            other => panic!("expected failed, got {other:?}"),
        };
        assert!(reason.contains(".github/workflows/*.yml"), "{reason}");
        assert!(
            backend.calls().is_empty(),
            "the workflow check runs before repository creation"
        );
    }

    #[test]
    fn a_coder_timeout_ends_budget_exhausted() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::new(crate::coder::FakeCoderBehavior::Timeout);
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::BudgetExhausted);
        assert_eq!(backend.calls().len(), 3);
        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(report.terminal.as_deref(), Some("budget_exhausted"));
    }

    #[test]
    fn a_coder_budget_exhaustion_ends_budget_exhausted() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::new(crate::coder::FakeCoderBehavior::BudgetExhausted);
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::BudgetExhausted);
        assert_eq!(backend.calls().len(), 3);
    }

    #[test]
    fn a_denied_open_pr_ends_failed_with_the_reason_in_the_log() {
        let root = TempDir::new().expect("temp dir");
        let mut config = config(&root);
        config.max_prs_per_day = 0;
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        let reason = match outcome {
            RunOutcome::Failed { reason } => reason,
            other => panic!("expected failed, got {other:?}"),
        };
        assert!(reason.contains("cap of 0 pull requests"), "{reason}");
        assert_eq!(
            backend.calls().len(),
            4,
            "the denied pull request never ran"
        );

        let denied = github_events(&config)
            .into_iter()
            .find(|(_, _, allowed, _)| !allowed)
            .expect("the refusal is audited");
        assert_eq!(denied.0, "open_pr");
        assert!(denied.3.contains("cap of 0 pull requests"), "{}", denied.3);
    }

    #[test]
    fn a_denied_setup_push_ends_failed_with_the_reason_in_the_log() {
        let root = TempDir::new().expect("temp dir");
        let mut config = config(&root);
        config.max_pushes_per_day = 1;
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        let reason = match outcome {
            RunOutcome::Failed { reason } => reason,
            other => panic!("expected failed, got {other:?}"),
        };
        assert!(reason.contains("daily cap of 1 pushes reached"), "{reason}");
        assert_eq!(backend.calls().len(), 2, "the denied push never ran");

        let denied = github_events(&config)
            .into_iter()
            .find(|(_, _, allowed, _)| !allowed)
            .expect("the refusal is audited");
        assert_eq!(denied.0, "push_setup_branch");
        assert!(
            denied.3.contains("daily cap of 1 pushes reached"),
            "{}",
            denied.3
        );
    }

    #[test]
    fn a_missing_token_fails_without_reaching_the_backend() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Green]);
        let store = MemorySecretStore::default();
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_with(&config, &backend, &store, &coder, &reviewer, &NoKillSwitch);

        match outcome {
            RunOutcome::Failed { reason } => {
                assert!(reason.contains("GITHUB_TOKEN"), "{reason}");
            }
            other => panic!("expected failed, got {other:?}"),
        }
        assert!(backend.calls().is_empty());

        let denied = github_events(&config)
            .into_iter()
            .find(|(_, _, allowed, _)| !allowed)
            .expect("the missing token is audited");
        assert!(denied.3.contains("github token is not set"), "{}", denied.3);
    }

    #[test]
    fn green_ci_and_an_approving_review_merge_on_the_first_try() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::Delivered);
        assert_eq!(coder.requests().len(), 1, "no fix cycle is needed");
        assert_eq!(reviewer.requests().len(), 1);
        assert!(matches!(
            backend.calls().last().map(|call| call.op.clone()),
            Some(BackendOp::MergePr { merge_target, .. }) if merge_target == "develop"
        ));
        assert!(!flow_steps(&config).iter().any(|step| step == "ci:failed"));
    }

    #[test]
    fn no_ci_run_after_the_poll_limit_fails_with_a_root_workflow_hint() {
        let root = TempDir::new().expect("temp dir");
        let mut config = config(&root);
        config.ci_no_run_poll_limit = 2;
        config.ci_poll_interval = Duration::ZERO;
        let backend = ci_backend_with_runs(&config, [None, None]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        let reason = match outcome {
            RunOutcome::Failed { reason } => reason,
            other => panic!("expected failed, got {other:?}"),
        };
        let sha = head_sha_of(&work_dir(&config)).expect("read pushed head SHA");
        assert_eq!(
            reason,
            format!(
                "no CI run started for {sha}: check that the repo has .github/workflows at its root"
            )
        );
        assert_eq!(
            backend
                .calls()
                .iter()
                .filter(|call| matches!(call.op, BackendOp::ReadCiStatus { .. }))
                .count(),
            2
        );
        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(report.terminal.as_deref(), Some("failed"));
    }

    #[test]
    fn a_pending_ci_run_can_turn_green_and_deliver() {
        let root = TempDir::new().expect("temp dir");
        let mut config = config(&root);
        config.ci_poll_interval = Duration::ZERO;
        let backend = ci_backend(&config, [CiState::Pending, CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::Delivered);
        assert_eq!(
            backend
                .calls()
                .iter()
                .filter(|call| matches!(call.op, BackendOp::ReadCiStatus { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn a_pending_ci_run_past_the_wall_time_ends_budget_exhausted() {
        let root = TempDir::new().expect("temp dir");
        let mut config = config(&root);
        config.max_wall_time = Duration::from_secs(1);
        config.ci_poll_interval = Duration::from_millis(10);
        let backend = ci_backend(&config, vec![CiState::Pending; 300]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::BudgetExhausted);
        assert!(
            backend
                .calls()
                .iter()
                .any(|call| matches!(call.op, BackendOp::ReadCiStatus { .. })),
            "the existing pending run was polled before the wall-time limit"
        );
        assert!(reviewer.requests().is_empty());
    }

    #[test]
    fn a_ci_failure_is_fixed_then_ci_passes() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Failed, CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::Delivered);
        assert_eq!(coder.requests().len(), 2);
        let fix = &coder.requests()[1];
        assert!(fix.prompt.contains("failure log at"), "{}", fix.prompt);
        let steps = flow_steps(&config);
        let failed = steps
            .iter()
            .position(|step| step == "ci:failed")
            .expect("the failure is audited");
        assert!(steps[failed + 1..].contains(&"command:ci-fix:cycle".to_string()));
        assert!(steps.iter().any(|step| step == "ci:green"));
    }

    #[test]
    fn red_beyond_the_fix_cycles_ends_failed() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let failures = usize::from(MAX_FIX_CYCLES) + 1;
        let backend = ci_backend(&config, vec![CiState::Failed; failures]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        let reason = match outcome {
            RunOutcome::Failed { reason } => reason,
            other => panic!("expected failed, got {other:?}"),
        };
        assert!(reason.contains("fix cycles exhausted"), "{reason}");
        assert_eq!(
            coder.requests().len(),
            usize::from(MAX_FIX_CYCLES) + 1,
            "the initial coder plus every allowed fix cycle"
        );
        assert!(reviewer.requests().is_empty(), "review never runs");
    }

    #[test]
    fn review_changes_consumes_a_fix_cycle_then_approves() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Green, CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::new([ReviewDecision::Changes, ReviewDecision::Approved]);

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::Delivered);
        assert_eq!(reviewer.requests().len(), 2);
        assert_eq!(coder.requests().len(), 2, "one review fix round");
        let steps = flow_steps(&config);
        let changes = steps
            .iter()
            .position(|step| step == "command:review:changes")
            .expect("the review decision is audited");
        assert!(steps[changes + 1..].contains(&"command:ci-fix:cycle".to_string()));
        assert!(steps.iter().any(|step| step == "command:review:approved"));
    }

    #[test]
    fn a_denied_merge_ends_failed() {
        let root = TempDir::new().expect("temp dir");
        let mut config = config(&root);
        config.stage = ProjectStage::Production;
        config.default_branch = config.integration_branch.clone();
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        let reason = match outcome {
            RunOutcome::Failed { reason } => reason,
            other => panic!("expected failed, got {other:?}"),
        };
        assert!(reason.contains("explicit approval"), "{reason}");
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| matches!(call.op, BackendOp::MergePr { .. })),
            "a denied merge never reaches the backend"
        );
    }

    #[test]
    fn the_run_wall_clock_limit_ends_budget_exhausted() {
        let root = TempDir::new().expect("temp dir");
        let mut config = config(&root);
        config.max_wall_time = Duration::ZERO;
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::BudgetExhausted);
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| matches!(call.op, BackendOp::ReadCiStatus { .. })),
            "the deadline is checked before the first poll"
        );
        assert!(reviewer.requests().is_empty());
    }

    #[test]
    fn the_post_pull_request_steps_are_audited_in_order() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);

        assert_eq!(outcome, RunOutcome::Delivered);
        let steps = flow_steps(&config);
        let open = steps
            .iter()
            .position(|step| step == "github:open_pr")
            .expect("the pull request is audited");
        assert_eq!(
            &steps[open + 1..],
            &[
                "github:read_ci_status",
                "ci:green",
                "board:review",
                "command:bwrap:filesystem-isolation",
                "command:claude:-p",
                "command:review:approved",
                "command:merge-decision:allowed",
                "github:merge_pr",
                "board:done",
                "command:e2e-outcome:delivered",
            ]
        );

        let log = audit_log(&config);
        assert!(log.verify().expect("verify audit log").is_none());
    }

    #[test]
    fn the_planner_makes_one_task_from_the_spec() {
        let text = std::fs::read_to_string(fixture_dir().join("SPEC.md")).expect("read spec");
        let plan = plan_from_spec(&Spec::parse(&text));

        assert_eq!(plan.tasks.len(), 1);
        let task = &plan.tasks[0];
        assert!(task.title.contains("remove a note"), "{}", task.title);
        assert!(task.details.contains("DELETE /notes/{id}"));
        assert_eq!(
            task.actors,
            vec![
                FactoryRole::Coder,
                FactoryRole::Tester,
                FactoryRole::Reviewer,
                FactoryRole::GitCoder,
            ]
        );
    }

    #[test]
    fn a_stale_kill_file_is_removed_at_the_start_of_a_new_run() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        request_kill(&config.state_dir).expect("request kill");
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();
        let kill = FileKillSwitch::new(&config.state_dir);

        let outcome = run_with(
            &config,
            &backend,
            &MemorySecretStore::dry_run(),
            &coder,
            &reviewer,
            &kill,
        );

        assert_eq!(outcome, RunOutcome::Delivered);
        assert!(
            !kill_file(&config.state_dir).exists(),
            "the stale kill file is gone"
        );
        let log = audit_log(&config);
        let events = log.read_all().expect("read audit log");
        assert!(
            events.iter().any(
                |event| matches!(&event.kind, AuditKind::Command { argv, .. }
                if argv.first().map(String::as_str) == Some(STALE_KILL_MARKER))
            ),
            "the stale kill removal is audited"
        );
        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(report.terminal.as_deref(), Some("delivered"));
    }

    #[test]
    fn old_scratch_labs_are_cleaned_at_the_start_of_a_new_run() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let scratch = config.state_dir.join(SCRATCH_DIR);
        let old_run = scratch.join("e2e-00000000000000000001");
        let old_base = scratch.join(E2E_LAB);
        let unrelated = scratch.join("keep-me");
        fs::create_dir_all(&old_run).expect("old run scratch");
        fs::create_dir_all(&old_base).expect("old base scratch");
        fs::create_dir_all(&unrelated).expect("unrelated scratch directory");

        start_run(&config).expect("start new run");

        assert!(!old_run.exists());
        assert!(!old_base.exists());
        assert!(unrelated.is_dir());
    }

    #[test]
    fn a_second_run_into_the_same_state_dir_passes_verify() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);

        for _ in 0..2 {
            let backend = ci_backend(&config, [CiState::Green]);
            let coder = FakeCoderRunner::editing();
            let reviewer = FakeReviewerRunner::approving();
            assert_eq!(
                run_dry(&config, &backend, &coder, &reviewer),
                RunOutcome::Delivered
            );
        }

        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(report.terminal.as_deref(), Some("delivered"));
    }

    #[test]
    fn a_kill_mid_coder_interrupts_the_child_and_keeps_the_run() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let kill_path = kill_file(&config.state_dir);
        let work_tree = work_dir(&config);
        let started_path = work_tree.join("coder.started");
        let descendant_path = work_tree.join("coder-descendant.txt");
        let fake_bin = TempDir::new().expect("fake binary directory");
        let script = fake_bin.path().join("fake-claude");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\n(while :; do echo x >> '{}'; sleep 0.01; done) &\nwait\n",
                started_path.display(),
                descendant_path.display()
            ),
        )
        .expect("write fake coder");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("make executable");

        let backend = ci_backend(&config, [CiState::Green]);
        let coder = ClaudeCoderRunner::with_binary_and_home(&script, fake_home(root.path()));
        let reviewer = FakeReviewerRunner::approving();
        let kill = FileKillSwitch::new(&config.state_dir);

        let store = MemorySecretStore::dry_run();
        let outcome = thread::scope(|scope| {
            let run = scope.spawn(|| run_with(&config, &backend, &store, &coder, &reviewer, &kill));
            let started = Instant::now();
            while !started_path.exists() && started.elapsed() < Duration::from_secs(5) {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(started_path.exists(), "the coder child started");
            fs::write(&kill_path, "kill").expect("request kill from the host");
            run.join().expect("join e2e run")
        });

        assert_eq!(outcome, RunOutcome::Killed);
        assert!(work_dir(&config).is_dir(), "the work tree is kept");
        assert!(board_dir(&config).is_dir(), "the Board is kept");
        let log = audit_log(&config);
        let events = log.read_all().expect("read audit log");
        assert_eq!(outcome_step(&events).as_deref(), Some("coder"));
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| matches!(call.op, BackendOp::OpenPr { .. })),
            "the run stops before the pull request"
        );

        let child_output = fs::read(&descendant_path).expect("descendant wrote output");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fs::read(&descendant_path)
                .expect("descendant output remains")
                .len(),
            child_output.len(),
            "the coder child process group was killed"
        );
        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(report.terminal.as_deref(), Some("killed"));
    }

    #[test]
    fn a_kill_during_the_ci_wait_ends_killed() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = KillingBackend {
            inner: ci_backend(&config, [CiState::Pending]),
            kill_path: kill_file(&config.state_dir),
        };
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();
        let kill = FileKillSwitch::new(&config.state_dir);

        let outcome = run_with(
            &config,
            &backend,
            &MemorySecretStore::dry_run(),
            &coder,
            &reviewer,
            &kill,
        );

        assert_eq!(outcome, RunOutcome::Killed);
        assert!(reviewer.requests().is_empty(), "review never starts");
        let log = audit_log(&config);
        let events = log.read_all().expect("read audit log");
        assert_eq!(outcome_step(&events).as_deref(), Some("ci"));
        assert_eq!(coder.requests().len(), 1, "only the initial coder ran");
        assert!(work_dir(&config).is_dir(), "the work tree is kept");
        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(report.terminal.as_deref(), Some("killed"));
    }

    #[test]
    fn verify_passes_on_a_delivered_run() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Failed, CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();

        let outcome = run_dry(&config, &backend, &coder, &reviewer);
        assert_eq!(outcome, RunOutcome::Delivered);

        let report = verify_run(&config.state_dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(report.terminal.as_deref(), Some("delivered"));
        assert!(report.events > 0);
    }

    #[test]
    fn verify_fails_on_a_tampered_log() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();
        assert_eq!(
            run_dry(&config, &backend, &coder, &reviewer),
            RunOutcome::Delivered
        );

        let log = audit_log(&config);
        let text = fs::read_to_string(log.path()).expect("read audit log file");
        let tampered = text.replacen("\"exit_code\":0", "\"exit_code\":1", 1);
        assert_ne!(text, tampered, "the log has an exit code to change");
        fs::write(log.path(), tampered).expect("write tampered log");

        let report = verify_run(&config.state_dir);
        assert!(!report.ok());
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("hash chain")),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn verify_fails_on_a_token_looking_string() {
        let root = TempDir::new().expect("temp dir");
        let config = config(&root);
        let backend = ci_backend(&config, [CiState::Green]);
        let coder = FakeCoderRunner::editing();
        let reviewer = FakeReviewerRunner::approving();
        assert_eq!(
            run_dry(&config, &backend, &coder, &reviewer),
            RunOutcome::Delivered
        );

        let log = audit_log(&config);
        log.append(
            Some(E2E_PROJECT),
            Some(E2E_WORKER),
            AuditKind::command(
                vec!["echo".to_string(), "ghp_notARealToken".to_string()],
                Some(0),
                None,
                false,
                0,
            ),
        )
        .expect("append a token-looking event");

        let report = verify_run(&config.state_dir);
        assert!(!report.ok());
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("token")),
            "{:?}",
            report.problems
        );
    }
}
