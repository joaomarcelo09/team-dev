//! Board of the Dark Factory lab: task cards, columns, permissions and persistence.
//!
//! The lab Board has no gates. The Lab Coordinator, the user and each project's
//! Co-coordinator may move a card between any two columns at any time. Instead of blocking
//! a move, the Board appends a [`AuditKind::BoardMove`] event with who moved the card, why,
//! and which checks were or were not green.
//!
//! The only refusals are project isolation (a Co-coordinator may act only on cards of its
//! own project) and unknown cards. Every card carries the [`ProjectId`] it belongs to.

use std::{
    collections::BTreeMap,
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

use jgf_core::{AuditKind, AuditLog, CheckOutcome, CheckStatus, ModelError, ProjectId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// File name of the persisted Board inside the Board directory.
const BOARD_FILE: &str = "board.json";

/// File name of the cross-process lock inside the Board directory.
const LOCK_FILE: &str = "board.lock";

/// Errors raised by the lab Board.
#[derive(Debug, Error)]
pub enum BoardError {
    /// Reading, writing or auditing failed.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// A persisted Board file is not valid JSON for a Board.
    #[error("invalid board file at {}: {source}", path.display())]
    InvalidBoard {
        /// Path of the rejected file.
        path: PathBuf,
        /// JSON parser error.
        #[source]
        source: serde_json::Error,
    },

    /// No card has the requested key.
    #[error("unknown task {0:?}")]
    UnknownTask(String),

    /// A Co-coordinator tried to act on another project's card.
    #[error("co-coordinator of project {coordinator} cannot act on cards of project {card}")]
    WrongProject {
        /// Project the Co-coordinator owns.
        coordinator: ProjectId,
        /// Project of the card it tried to act on.
        card: ProjectId,
    },

    /// A column name does not belong to the Board.
    #[error("unknown board column {0:?}")]
    UnknownColumn(String),

    /// A project's task counter cannot grow any further.
    #[error("task counter for project {0} is exhausted")]
    CounterExhausted(ProjectId),
}

impl From<serde_json::Error> for BoardError {
    fn from(source: serde_json::Error) -> Self {
        Self::Model(ModelError::Json(source))
    }
}

/// A column of the lab Board, from intake to delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    /// Not queued yet.
    Backlog,
    /// Queued for a worker.
    OnQueue,
    /// A worker is on it.
    Working,
    /// Tests are running.
    Testing,
    /// Under review.
    Review,
    /// Ready to commit.
    ToCommit,
    /// Ready to push.
    ToPush,
    /// Ready to open a pull request.
    ToOpenPr,
    /// Ready to merge.
    ToMerge,
    /// Finished.
    Done,
}

impl Column {
    /// Lowercase name used in JSON, the audit log and the CLI.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::OnQueue => "on_queue",
            Self::Working => "working",
            Self::Testing => "testing",
            Self::Review => "review",
            Self::ToCommit => "to_commit",
            Self::ToPush => "to_push",
            Self::ToOpenPr => "to_open_pr",
            Self::ToMerge => "to_merge",
            Self::Done => "done",
        }
    }

    /// Parses a column name written by [`Column::as_str`].
    pub fn parse(raw: &str) -> Result<Self, BoardError> {
        match raw {
            "backlog" => Ok(Self::Backlog),
            "on_queue" => Ok(Self::OnQueue),
            "working" => Ok(Self::Working),
            "testing" => Ok(Self::Testing),
            "review" => Ok(Self::Review),
            "to_commit" => Ok(Self::ToCommit),
            "to_push" => Ok(Self::ToPush),
            "to_open_pr" => Ok(Self::ToOpenPr),
            "to_merge" => Ok(Self::ToMerge),
            "done" => Ok(Self::Done),
            other => Err(BoardError::UnknownColumn(other.to_string())),
        }
    }
}

/// Whether a recorded test or review result passed or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    /// The test or review passed.
    Pass,
    /// The test or review failed.
    Fail,
}

/// A recorded test or review result, with a short human summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// Pass or fail.
    pub status: OutcomeStatus,
    /// Short summary of what happened.
    pub summary: String,
}

impl Outcome {
    /// A passing result with `summary`.
    pub fn pass(summary: impl Into<String>) -> Self {
        Self {
            status: OutcomeStatus::Pass,
            summary: summary.into(),
        }
    }

    /// A failing result with `summary`.
    pub fn fail(summary: impl Into<String>) -> Self {
        Self {
            status: OutcomeStatus::Fail,
            summary: summary.into(),
        }
    }

    /// The outcome as it appears in the checks of an audit event.
    pub fn check_outcome(&self) -> CheckOutcome {
        match self.status {
            OutcomeStatus::Pass => CheckOutcome::Pass,
            OutcomeStatus::Fail => CheckOutcome::Fail,
        }
    }
}

/// Person or agent that acts on the Board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// The lab-wide coordinator; may act on every project.
    LabCoordinator,
    /// The coordinator of one project; may act only on that project's cards.
    CoCoordinator(ProjectId),
    /// The user; may act on every project.
    User,
}

impl Actor {
    /// Name recorded in the audit log.
    pub fn who(&self) -> String {
        match self {
            Self::LabCoordinator => "lab-coordinator".to_string(),
            Self::CoCoordinator(project) => format!("co-coordinator:{project}"),
            Self::User => "user".to_string(),
        }
    }
}

/// One card of the lab Board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Unique board key, `<PROJECT-ID-UPPER>-<n>`.
    pub key: String,
    /// Project the card belongs to.
    pub project: ProjectId,
    /// Short title.
    pub title: String,
    /// What the task must do.
    pub description: String,
    /// Optional implementation details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// Column the card is in.
    pub status: Column,
    /// Last recorded test result, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_result: Option<Outcome>,
    /// Last recorded review result, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_result: Option<Outcome>,
    /// Creation time, in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// Last change time, in milliseconds since the Unix epoch.
    pub updated_ms: u64,
}

/// Selects cards for [`Board::list`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardFilter {
    /// Only cards of this project, when set.
    pub project: Option<ProjectId>,
    /// Only cards in this column, when set.
    pub column: Option<Column>,
}

impl BoardFilter {
    /// A filter that matches every card.
    pub fn all() -> Self {
        Self::default()
    }

    fn matches(&self, task: &Task) -> bool {
        self.project
            .as_ref()
            .is_none_or(|project| *project == task.project)
            && self.column.is_none_or(|column| column == task.status)
    }
}

/// In-memory state guarded by the Board mutex.
#[derive(Debug)]
struct State {
    /// Cards in creation order.
    tasks: Vec<Task>,
    /// Highest task number created per project id.
    counters: BTreeMap<String, u64>,
}

/// On-disk form of the Board.
#[derive(Debug, Default, Deserialize)]
struct BoardFile {
    /// Every card, in creation order.
    #[serde(default)]
    tasks: Vec<Task>,
}

/// Borrowed on-disk form used while writing the Board.
#[derive(Debug, Serialize)]
struct BoardFileRef<'a> {
    /// Every card, in creation order.
    tasks: &'a [Task],
}

/// The lab Board, persisted as `<dir>/board.json`.
///
/// An in-process mutex and a cross-process `flock` of `<dir>/board.lock` serialize every
/// change, and each change re-reads `board.json` before it applies, so Boards in several
/// processes or instances never overwrite each other's cards. Saves go to a uniquely named
/// temporary file that is synced and renamed over the Board file, so a crash never leaves a
/// partially written Board.
#[derive(Debug)]
pub struct Board {
    dir: PathBuf,
    path: PathBuf,
    lock_path: PathBuf,
    audit: AuditLog,
    state: Mutex<State>,
}

impl Board {
    /// Opens the Board under `dir`, creating an empty Board when `board.json` is absent.
    ///
    /// A file that exists but is not valid Board JSON is an error, not an empty Board.
    pub fn open(dir: impl Into<PathBuf>, audit: AuditLog) -> Result<Self, BoardError> {
        let dir = dir.into();
        let path = dir.join(BOARD_FILE);
        let lock_path = dir.join(LOCK_FILE);
        let tasks = read_tasks(&path)?;
        let counters = counters_from(&tasks);
        Ok(Self {
            dir,
            path,
            lock_path,
            audit,
            state: Mutex::new(State { tasks, counters }),
        })
    }

    /// Directory that holds `board.json`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the persisted Board file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Audit log that records every card move.
    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// Creates a card in Backlog and returns it.
    ///
    /// A Co-coordinator may create cards only for its own project. The key uses a counter
    /// per project, so the first card of `alpha` is `ALPHA-1`.
    pub fn create(
        &self,
        actor: &Actor,
        project: ProjectId,
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Task, BoardError> {
        ensure_can_act(actor, &project)?;
        self.mutate(|state| {
            let next = state
                .counters
                .get(project.as_str())
                .copied()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| BoardError::CounterExhausted(project.clone()))?;
            let key = format!("{}-{next}", project.as_str().to_ascii_uppercase());
            let now = now_ms();
            let task = Task {
                key,
                project: project.clone(),
                title: title.into(),
                description: description.into(),
                details: None,
                status: Column::Backlog,
                test_result: None,
                review_result: None,
                created_ms: now,
                updated_ms: now,
            };

            let mut tasks = state.tasks.clone();
            tasks.push(task.clone());
            Ok((tasks, task))
        })
    }

    /// Moves a card to OnQueue. Shorthand for [`Board::move_task`].
    pub fn queue(
        &self,
        actor: &Actor,
        key: &str,
        reason: impl Into<String>,
    ) -> Result<Task, BoardError> {
        self.move_task(actor, key, Column::OnQueue, reason, Vec::new())
    }

    /// Moves a card to `to`, recording the move in the audit log.
    ///
    /// Any column may follow any other column and the same column is accepted too. The
    /// recorded checks are `checks` followed by `test_result` and `review_result`, taken
    /// from the card (or `Missing` when the card has no result). The audit event is written
    /// before the Board is saved; if the audit append fails, the move is not applied.
    pub fn move_task(
        &self,
        actor: &Actor,
        key: &str,
        to: Column,
        reason: impl Into<String>,
        checks: Vec<CheckStatus>,
    ) -> Result<Task, BoardError> {
        self.mutate(|state| {
            let index = state
                .tasks
                .iter()
                .position(|task| task.key == key)
                .ok_or_else(|| BoardError::UnknownTask(key.to_string()))?;
            let current = state.tasks[index].clone();
            ensure_can_act(actor, &current.project)?;

            let from = current.status;
            let who = actor.who();
            let checks = checks_with_results(&current, checks);
            let kind = AuditKind::board_move(
                current.key.clone(),
                from.as_str(),
                to.as_str(),
                who.clone(),
                reason,
                checks,
            );
            self.audit
                .append(Some(current.project.as_str()), Some(who.as_str()), kind)?;

            let mut tasks = state.tasks.clone();
            tasks[index].status = to;
            tasks[index].updated_ms = now_ms();
            let updated = tasks[index].clone();
            Ok((tasks, updated))
        })
    }

    /// Replaces the optional details of a card.
    pub fn update_details(
        &self,
        actor: &Actor,
        key: &str,
        details: Option<String>,
    ) -> Result<Task, BoardError> {
        self.update_task(actor, key, |task| task.details = details)
    }

    /// Records or clears the test result of a card.
    pub fn record_test_result(
        &self,
        actor: &Actor,
        key: &str,
        result: Option<Outcome>,
    ) -> Result<Task, BoardError> {
        self.update_task(actor, key, |task| task.test_result = result)
    }

    /// Records or clears the review result of a card.
    pub fn record_review_result(
        &self,
        actor: &Actor,
        key: &str,
        result: Option<Outcome>,
    ) -> Result<Task, BoardError> {
        self.update_task(actor, key, |task| task.review_result = result)
    }

    /// Returns a copy of the card with `key`, when it exists.
    ///
    /// The persisted Board is re-read under a shared lock first, so the answer includes
    /// changes made by another Board instance or process.
    pub fn get(&self, key: &str) -> Option<Task> {
        let mut state = self.lock();
        self.refresh(&mut state);
        state.tasks.iter().find(|task| task.key == key).cloned()
    }

    /// Copies of the cards selected by `filter`, in creation order.
    ///
    /// The persisted Board is re-read under a shared lock first, so the answer includes
    /// changes made by another Board instance or process.
    pub fn list(&self, filter: &BoardFilter) -> Vec<Task> {
        let mut state = self.lock();
        self.refresh(&mut state);
        state
            .tasks
            .iter()
            .filter(|task| filter.matches(task))
            .cloned()
            .collect()
    }

    /// Applies `change` to a card, saves the Board and returns the updated card.
    fn update_task(
        &self,
        actor: &Actor,
        key: &str,
        change: impl FnOnce(&mut Task),
    ) -> Result<Task, BoardError> {
        self.mutate(|state| {
            let index = state
                .tasks
                .iter()
                .position(|task| task.key == key)
                .ok_or_else(|| BoardError::UnknownTask(key.to_string()))?;
            ensure_can_act(actor, &state.tasks[index].project)?;

            let mut tasks = state.tasks.clone();
            let now = now_ms();
            let task = &mut tasks[index];
            change(task);
            task.updated_ms = now;
            let updated = task.clone();
            Ok((tasks, updated))
        })
    }

    /// Re-reads the Board under the exclusive lock, applies `change`, then saves the result.
    ///
    /// `change` returns the new card list and its own result. The Board mutex and
    /// `<dir>/board.lock` are held while the file is read and replaced, so concurrent
    /// Boards in this or another process cannot lose each other's changes. Nothing is
    /// saved when `change` fails, and the in-memory cache keeps reflecting the disk.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut State) -> Result<(Vec<Task>, T), BoardError>,
    ) -> Result<T, BoardError> {
        let mut state = self.lock();
        let lock_file = self.open_lock_file()?;
        let _lock = FileLock::exclusive(&lock_file, &self.lock_path)?;
        state.tasks = read_tasks(&self.path)?;
        state.counters = counters_from(&state.tasks);

        let (tasks, value) = change(&mut state)?;
        self.persist_tasks(&tasks)?;
        state.counters = counters_from(&tasks);
        state.tasks = tasks;
        Ok(value)
    }

    /// Refreshes the cache from disk under a shared lock, when the file can be read.
    ///
    /// Reads have no error return, so an unreadable lock or Board file leaves the previous
    /// snapshot in place instead of failing the caller.
    fn refresh(&self, state: &mut State) {
        let Some(tasks) = self.latest_tasks() else {
            return;
        };
        state.tasks = tasks;
        state.counters = counters_from(&state.tasks);
    }

    fn latest_tasks(&self) -> Option<Vec<Task>> {
        if !self.dir.exists() {
            return None;
        }
        let lock_file = self.open_lock_file().ok()?;
        let _lock = FileLock::shared(&lock_file, &self.lock_path).ok()?;
        read_tasks(&self.path).ok()
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

    /// Saves `tasks` with an atomic replacement of `<dir>/board.json`.
    fn persist_tasks(&self, tasks: &[Task]) -> Result<(), BoardError> {
        let bytes = serde_json::to_vec_pretty(&BoardFileRef { tasks })?;
        write_atomic(&self.path, &bytes).map_err(BoardError::from)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Refuses a Co-coordinator that acts on another project's card.
fn ensure_can_act(actor: &Actor, project: &ProjectId) -> Result<(), BoardError> {
    match actor {
        Actor::LabCoordinator | Actor::User => Ok(()),
        Actor::CoCoordinator(coordinator) if coordinator == project => Ok(()),
        Actor::CoCoordinator(coordinator) => Err(BoardError::WrongProject {
            coordinator: coordinator.clone(),
            card: project.clone(),
        }),
    }
}

/// Adds the card's test and review outcomes to the caller's checks.
fn checks_with_results(task: &Task, mut checks: Vec<CheckStatus>) -> Vec<CheckStatus> {
    checks.push(CheckStatus::new(
        "test_result",
        task.test_result
            .as_ref()
            .map_or(CheckOutcome::Missing, Outcome::check_outcome),
    ));
    checks.push(CheckStatus::new(
        "review_result",
        task.review_result
            .as_ref()
            .map_or(CheckOutcome::Missing, Outcome::check_outcome),
    ));
    checks
}

/// Recomputes the task counter of every project from the stored keys.
fn counters_from(tasks: &[Task]) -> BTreeMap<String, u64> {
    let mut counters: BTreeMap<String, u64> = BTreeMap::new();
    for task in tasks {
        let Some((_, suffix)) = task.key.rsplit_once('-') else {
            continue;
        };
        let Ok(number) = suffix.parse::<u64>() else {
            continue;
        };
        counters
            .entry(task.project.as_str().to_string())
            .and_modify(|current| *current = (*current).max(number))
            .or_insert(number);
    }
    counters
}

/// Reads and validates the persisted Board at `path`, or an empty Board when it is absent.
fn read_tasks(path: &Path) -> Result<Vec<Task>, BoardError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(path, source).into()),
    };
    serde_json::from_slice::<BoardFile>(&bytes)
        .map(|file| file.tasks)
        .map_err(|source| BoardError::InvalidBoard {
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

    fn shared(file: &'a File, path: &Path) -> Result<Self, ModelError> {
        Self::acquire(file, path, libc::LOCK_SH)
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
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

    /// Temporary Board directory removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("jgf-hub-board-{}-{suffix}", std::process::id()));
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

    const COLUMNS: [Column; 10] = [
        Column::Backlog,
        Column::OnQueue,
        Column::Working,
        Column::Testing,
        Column::Review,
        Column::ToCommit,
        Column::ToPush,
        Column::ToOpenPr,
        Column::ToMerge,
        Column::Done,
    ];

    fn project(raw: &str) -> ProjectId {
        ProjectId::parse(raw).expect("valid project id")
    }

    fn open(root: &TempRoot) -> Board {
        let audit = AuditLog::open(root.path(), "lab").expect("open audit log");
        Board::open(root.path(), audit).expect("open board")
    }

    /// Two Boards of the same directory and lab, each with its own audit handle.
    fn open_pair(root: &TempRoot) -> (Board, Board) {
        let first_audit = AuditLog::open(root.path(), "lab").expect("open audit log");
        let second_audit = AuditLog::open(root.path(), "lab").expect("open audit log");
        (
            Board::open(root.path(), first_audit).expect("open first board"),
            Board::open(root.path(), second_audit).expect("open second board"),
        )
    }

    fn card_keys(board: &Board) -> Vec<String> {
        board
            .list(&BoardFilter::all())
            .into_iter()
            .map(|task| task.key)
            .collect()
    }

    fn events(board: &Board) -> Vec<AuditEvent> {
        board.audit().read_all().expect("read audit log")
    }

    fn audit_move(event: &AuditEvent) -> (&str, &str, &str, &str, &str, &[CheckStatus]) {
        let AuditKind::BoardMove {
            task,
            from,
            to,
            who,
            reason,
            checks,
        } = &event.kind
        else {
            panic!("expected a board move, got {:?}", event.kind);
        };
        (task, from, to, who, reason, checks)
    }

    #[test]
    fn every_move_between_columns_is_accepted_and_audited() {
        let root = TempRoot::new();
        let board = open(&root);
        let task = board
            .create(&Actor::LabCoordinator, project("alpha"), "First", "Do it")
            .expect("create");
        assert_eq!(task.key, "ALPHA-1");
        assert_eq!(task.status, Column::Backlog);

        let mut current = Column::Backlog;
        let mut expected: Vec<(Column, Column, String)> = Vec::new();
        for from in COLUMNS {
            for to in COLUMNS {
                if current != from {
                    let reason = format!("reset {current:?} to {from:?}");
                    board
                        .move_task(
                            &Actor::LabCoordinator,
                            &task.key,
                            from,
                            reason.clone(),
                            Vec::new(),
                        )
                        .expect("reset move is accepted");
                    expected.push((current, from, reason));
                }
                let reason = format!("move {from:?} to {to:?}");
                let moved = board
                    .move_task(
                        &Actor::LabCoordinator,
                        &task.key,
                        to,
                        reason.clone(),
                        Vec::new(),
                    )
                    .expect("every move is accepted");
                assert_eq!(moved.status, to);
                expected.push((from, to, reason));
                current = to;
            }
        }

        let events = events(&board);
        assert_eq!(events.len(), expected.len());
        for (event, (from, to, reason)) in events.iter().zip(&expected) {
            let (task_key, event_from, event_to, who, event_reason, checks) = audit_move(event);
            assert_eq!(task_key, task.key);
            assert_eq!(event_from, from.as_str());
            assert_eq!(event_to, to.as_str());
            assert_eq!(who, "lab-coordinator");
            assert_eq!(event_reason, reason);
            assert_eq!(event.project.as_deref(), Some("alpha"));
            assert_eq!(event.worker.as_deref(), Some("lab-coordinator"));
            assert_eq!(
                checks,
                [
                    CheckStatus::new("test_result", CheckOutcome::Missing),
                    CheckStatus::new("review_result", CheckOutcome::Missing),
                ]
            );
        }
        assert_eq!(board.audit().verify().expect("verify chain"), None);
    }

    #[test]
    fn a_same_column_move_is_accepted_and_audited() {
        let root = TempRoot::new();
        let board = open(&root);
        let task = board
            .create(&Actor::LabCoordinator, project("alpha"), "First", "Do it")
            .expect("create");

        board
            .move_task(
                &Actor::LabCoordinator,
                &task.key,
                Column::Backlog,
                "still here",
                Vec::new(),
            )
            .expect("same column move");

        let events = events(&board);
        assert_eq!(events.len(), 1);
        let (_, from, to, _, reason, _) = audit_move(&events[0]);
        assert_eq!(from, "backlog");
        assert_eq!(to, "backlog");
        assert_eq!(reason, "still here");
    }

    #[test]
    fn audit_records_caller_checks_and_card_outcomes() {
        let root = TempRoot::new();
        let board = open(&root);
        let actor = Actor::CoCoordinator(project("alpha"));
        let task = board
            .create(&actor, project("alpha"), "First", "Do it")
            .expect("create");
        board
            .record_test_result(&actor, &task.key, Some(Outcome::pass("tests are green")))
            .expect("record test");
        board
            .record_review_result(&actor, &task.key, Some(Outcome::fail("needs work")))
            .expect("record review");

        board
            .move_task(
                &actor,
                &task.key,
                Column::Done,
                "ship it",
                vec![CheckStatus::new("build", CheckOutcome::Pass)],
            )
            .expect("move");

        let events = events(&board);
        assert_eq!(events.len(), 1);
        let (_, from, to, who, reason, checks) = audit_move(&events[0]);
        assert_eq!(from, "backlog");
        assert_eq!(to, "done");
        assert_eq!(who, "co-coordinator:alpha");
        assert_eq!(reason, "ship it");
        assert_eq!(
            checks,
            [
                CheckStatus::new("build", CheckOutcome::Pass),
                CheckStatus::new("test_result", CheckOutcome::Pass),
                CheckStatus::new("review_result", CheckOutcome::Fail),
            ]
        );
        assert_eq!(events[0].project.as_deref(), Some("alpha"));
        assert_eq!(events[0].worker.as_deref(), Some("co-coordinator:alpha"));
    }

    #[test]
    fn queue_moves_to_on_queue_and_is_audited() {
        let root = TempRoot::new();
        let board = open(&root);
        let actor = Actor::LabCoordinator;
        let task = board
            .create(&actor, project("alpha"), "First", "Do it")
            .expect("create");

        let queued = board.queue(&actor, &task.key, "start now").expect("queue");
        assert_eq!(queued.status, Column::OnQueue);

        let events = events(&board);
        assert_eq!(events.len(), 1);
        let (_, from, to, who, reason, checks) = audit_move(&events[0]);
        assert_eq!(from, "backlog");
        assert_eq!(to, "on_queue");
        assert_eq!(who, "lab-coordinator");
        assert_eq!(reason, "start now");
        assert_eq!(
            checks,
            [
                CheckStatus::new("test_result", CheckOutcome::Missing),
                CheckStatus::new("review_result", CheckOutcome::Missing),
            ]
        );
    }

    #[test]
    fn co_coordinator_can_only_act_on_its_own_project() {
        let root = TempRoot::new();
        let board = open(&root);
        let alpha = board
            .create(
                &Actor::LabCoordinator,
                project("alpha"),
                "Alpha",
                "Do alpha",
            )
            .expect("create alpha");
        let beta = board
            .create(&Actor::LabCoordinator, project("beta"), "Beta", "Do beta")
            .expect("create beta");
        let alpha_co = Actor::CoCoordinator(project("alpha"));

        board
            .queue(&alpha_co, &alpha.key, "own card")
            .expect("queue");
        board
            .create(&alpha_co, project("alpha"), "Second", "More alpha")
            .expect("create in own project");

        let refusals = [
            board.move_task(&alpha_co, &beta.key, Column::Done, "no", Vec::new()),
            board.queue(&alpha_co, &beta.key, "no"),
            board.update_details(&alpha_co, &beta.key, Some("no".to_string())),
            board.record_test_result(&alpha_co, &beta.key, Some(Outcome::pass("no"))),
            board.record_review_result(&alpha_co, &beta.key, Some(Outcome::fail("no"))),
            board.create(&alpha_co, project("beta"), "No", "No"),
        ];
        for refusal in refusals {
            assert!(
                matches!(refusal, Err(BoardError::WrongProject { .. })),
                "expected a project isolation error, got {refusal:?}"
            );
        }
        assert_eq!(
            board.get(&beta.key).expect("beta card").status,
            Column::Backlog
        );
        assert_eq!(
            board.get(&alpha.key).expect("alpha card").status,
            Column::OnQueue
        );

        board
            .move_task(
                &Actor::LabCoordinator,
                &beta.key,
                Column::Working,
                "lab may act anywhere",
                Vec::new(),
            )
            .expect("lab coordinator");
        board
            .move_task(
                &Actor::User,
                &alpha.key,
                Column::Done,
                "user may act anywhere",
                Vec::new(),
            )
            .expect("user");
        assert_eq!(
            board.get(&beta.key).expect("beta card").status,
            Column::Working
        );
        assert_eq!(
            board.get(&alpha.key).expect("alpha card").status,
            Column::Done
        );
    }

    #[test]
    fn keys_use_a_per_project_counter() {
        let root = TempRoot::new();
        let board = open(&root);
        let actor = Actor::LabCoordinator;

        let alpha_1 = board
            .create(&actor, project("alpha"), "A1", "d")
            .expect("create");
        let alpha_2 = board
            .create(&actor, project("alpha"), "A2", "d")
            .expect("create");
        let beta_1 = board
            .create(&actor, project("beta"), "B1", "d")
            .expect("create");
        let alpha_3 = board
            .create(&actor, project("alpha"), "A3", "d")
            .expect("create");

        assert_eq!(alpha_1.key, "ALPHA-1");
        assert_eq!(alpha_2.key, "ALPHA-2");
        assert_eq!(beta_1.key, "BETA-1");
        assert_eq!(alpha_3.key, "ALPHA-3");
    }

    #[test]
    fn a_board_survives_a_reopen() {
        let root = TempRoot::new();
        let key;
        {
            let board = open(&root);
            let actor = Actor::LabCoordinator;
            let task = board
                .create(&actor, project("alpha"), "First", "Do it")
                .expect("create");
            key = task.key.clone();
            board
                .update_details(&actor, &key, Some("the details".to_string()))
                .expect("details");
            board
                .record_test_result(&actor, &key, Some(Outcome::pass("green")))
                .expect("test");
            board
                .record_review_result(&actor, &key, Some(Outcome::fail("redo")))
                .expect("review");
            board
                .move_task(&actor, &key, Column::Review, "please review", Vec::new())
                .expect("move");
        }

        let board = open(&root);
        let task = board.get(&key).expect("card after reopen");
        assert_eq!(task.project, project("alpha"));
        assert_eq!(task.status, Column::Review);
        assert_eq!(task.details.as_deref(), Some("the details"));
        assert_eq!(task.test_result, Some(Outcome::pass("green")));
        assert_eq!(task.review_result, Some(Outcome::fail("redo")));
        assert_eq!(
            board
                .create(&Actor::LabCoordinator, project("alpha"), "Second", "d")
                .expect("create after reopen")
                .key,
            "ALPHA-2"
        );
    }

    #[test]
    fn list_filters_by_project_and_column() {
        let root = TempRoot::new();
        let board = open(&root);
        let actor = Actor::LabCoordinator;
        let alpha_1 = board
            .create(&actor, project("alpha"), "A1", "d")
            .expect("create");
        board
            .create(&actor, project("alpha"), "A2", "d")
            .expect("create");
        let beta_1 = board
            .create(&actor, project("beta"), "B1", "d")
            .expect("create");
        board
            .move_task(&actor, &alpha_1.key, Column::Working, "work", Vec::new())
            .expect("move alpha");
        board
            .move_task(&actor, &beta_1.key, Column::Done, "done", Vec::new())
            .expect("move beta");

        let keys = |tasks: Vec<Task>| tasks.into_iter().map(|task| task.key).collect::<Vec<_>>();
        assert_eq!(
            keys(board.list(&BoardFilter {
                project: Some(project("alpha")),
                column: None,
            })),
            ["ALPHA-1", "ALPHA-2"]
        );
        assert_eq!(
            keys(board.list(&BoardFilter {
                project: None,
                column: Some(Column::Working),
            })),
            ["ALPHA-1"]
        );
        assert_eq!(board.list(&BoardFilter::all()).len(), 3);
    }

    #[test]
    fn unknown_tasks_are_refused() {
        let root = TempRoot::new();
        let board = open(&root);
        let actor = Actor::LabCoordinator;

        assert!(matches!(
            board.move_task(&actor, "NOPE-1", Column::Done, "no", Vec::new()),
            Err(BoardError::UnknownTask(_))
        ));
        assert!(matches!(
            board.queue(&actor, "NOPE-1", "no"),
            Err(BoardError::UnknownTask(_))
        ));
        assert!(matches!(
            board.update_details(&actor, "NOPE-1", None),
            Err(BoardError::UnknownTask(_))
        ));
        assert!(matches!(
            board.record_test_result(&actor, "NOPE-1", None),
            Err(BoardError::UnknownTask(_))
        ));
        assert!(matches!(
            board.record_review_result(&actor, "NOPE-1", None),
            Err(BoardError::UnknownTask(_))
        ));
        assert!(board.get("NOPE-1").is_none());
    }

    #[test]
    fn a_corrupt_board_file_is_an_error() {
        let root = TempRoot::new();
        let audit = AuditLog::open(root.path(), "lab").expect("open audit log");
        fs::write(root.path().join(BOARD_FILE), b"{not json").expect("write corrupt board");

        let result = Board::open(root.path(), audit);
        assert!(
            matches!(result, Err(BoardError::InvalidBoard { .. })),
            "expected an invalid board error, got {result:?}"
        );
    }

    #[test]
    fn a_card_without_a_valid_project_is_rejected_on_load() {
        let root = TempRoot::new();
        let audit = AuditLog::open(root.path(), "lab").expect("open audit log");
        let contents = serde_json::json!({
            "tasks": [{
                "key": "BAD-1",
                "project": "Bad",
                "title": "t",
                "description": "d",
                "status": "backlog",
                "created_ms": 1,
                "updated_ms": 1
            }]
        });
        fs::write(
            root.path().join(BOARD_FILE),
            serde_json::to_vec(&contents).expect("json"),
        )
        .expect("write board");

        let result = Board::open(root.path(), audit);
        assert!(
            matches!(result, Err(BoardError::InvalidBoard { .. })),
            "expected an invalid board error, got {result:?}"
        );
    }

    #[test]
    fn an_audit_failure_leaves_the_board_unchanged() {
        let root = TempRoot::new();
        let board = open(&root);
        let actor = Actor::LabCoordinator;
        let task = board
            .create(&actor, project("alpha"), "First", "Do it")
            .expect("create");

        let log_path = root.path().join("labs").join("lab").join("audit.jsonl");
        let mut log = fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .expect("open audit log file");
        log.write_all(b"not an audit event\n").expect("corrupt log");
        drop(log);

        let result = board.move_task(&actor, &task.key, Column::Done, "no", Vec::new());
        assert!(
            matches!(
                result,
                Err(BoardError::Model(ModelError::AuditCorrupt { .. }))
            ),
            "expected an audit error, got {result:?}"
        );
        assert_eq!(
            board.get(&task.key).expect("card still exists").status,
            Column::Backlog
        );
    }

    #[test]
    fn two_boards_on_one_directory_do_not_lose_updates() {
        let root = TempRoot::new();
        let (first, second) = open_pair(&root);
        let actor = Actor::LabCoordinator;

        let alpha_1 = first
            .create(&actor, project("alpha"), "A1", "d")
            .expect("create");
        let alpha_2 = second
            .create(&actor, project("alpha"), "A2", "d")
            .expect("create");
        let beta_1 = first
            .create(&actor, project("beta"), "B1", "d")
            .expect("create");
        let alpha_3 = second
            .create(&actor, project("alpha"), "A3", "d")
            .expect("create");

        first.queue(&actor, &alpha_1.key, "go").expect("queue");
        second
            .move_task(&actor, &alpha_2.key, Column::Working, "work", Vec::new())
            .expect("move");
        first
            .move_task(&actor, &beta_1.key, Column::Done, "ship", Vec::new())
            .expect("move");
        second
            .move_task(&actor, &alpha_3.key, Column::Review, "review", Vec::new())
            .expect("move");

        assert_eq!(alpha_1.key, "ALPHA-1");
        assert_eq!(alpha_2.key, "ALPHA-2");
        assert_eq!(beta_1.key, "BETA-1");
        assert_eq!(alpha_3.key, "ALPHA-3");
        assert_eq!(
            card_keys(&second),
            ["ALPHA-1", "ALPHA-2", "BETA-1", "ALPHA-3"]
        );

        let status = |key: &str| first.get(key).expect("card").status;
        assert_eq!(status(&alpha_1.key), Column::OnQueue);
        assert_eq!(status(&alpha_2.key), Column::Working);
        assert_eq!(status(&beta_1.key), Column::Done);
        assert_eq!(status(&alpha_3.key), Column::Review);

        let events = events(&first);
        assert_eq!(events.len(), 4);
        for event in &events {
            assert!(matches!(event.kind, AuditKind::BoardMove { .. }));
        }
        assert_eq!(first.audit().verify().expect("verify chain"), None);
        assert_eq!(second.audit().verify().expect("verify chain"), None);

        let reopened = open(&root);
        assert_eq!(card_keys(&reopened), card_keys(&first));
        assert_eq!(
            reopened
                .create(&actor, project("alpha"), "A4", "d")
                .expect("create after reopen")
                .key,
            "ALPHA-4"
        );
    }

    #[test]
    fn concurrent_boards_keep_every_card_and_move() {
        const THREADS: usize = 8;
        const CARDS_PER_THREAD: usize = 5;

        let root = TempRoot::new();
        let (first, second) = open_pair(&root);
        let first = Arc::new(first);
        let second = Arc::new(second);

        let creators: Vec<_> = (0..THREADS)
            .map(|thread_index| {
                let first = Arc::clone(&first);
                let second = Arc::clone(&second);
                thread::spawn(move || {
                    let project = if thread_index % 2 == 0 {
                        project("alpha")
                    } else {
                        project("beta")
                    };
                    (0..CARDS_PER_THREAD)
                        .map(|card| {
                            let board = if card % 2 == 0 { &first } else { &second };
                            board
                                .create(
                                    &Actor::LabCoordinator,
                                    project.clone(),
                                    format!("T{thread_index}-{card}"),
                                    "d",
                                )
                                .expect("create")
                                .key
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let keys: Vec<String> = creators
            .into_iter()
            .flat_map(|handle| handle.join().expect("join creator"))
            .collect();

        assert_eq!(keys.len(), THREADS * CARDS_PER_THREAD);
        let unique: BTreeSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "keys are unique");

        let mut counters: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
        for key in &keys {
            let (project, number) = key.rsplit_once('-').expect("key suffix");
            counters
                .entry(project.to_string())
                .or_default()
                .insert(number.parse().expect("numeric suffix"));
        }
        let expected: BTreeSet<u64> = (1..=THREADS as u64 * CARDS_PER_THREAD as u64 / 2).collect();
        assert_eq!(counters.get("ALPHA"), Some(&expected));
        assert_eq!(counters.get("BETA"), Some(&expected));

        let movers: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let board = if index % 2 == 0 {
                    Arc::clone(&first)
                } else {
                    Arc::clone(&second)
                };
                let key = key.clone();
                thread::spawn(move || {
                    let actor = Actor::LabCoordinator;
                    board.queue(&actor, &key, "queue").expect("queue");
                    board
                        .move_task(&actor, &key, Column::Done, "done", Vec::new())
                        .expect("move");
                })
            })
            .collect();
        for handle in movers {
            handle.join().expect("join mover");
        }

        let listed = first.list(&BoardFilter::all());
        assert_eq!(listed.len(), keys.len());
        assert!(
            listed.iter().all(|task| task.status == Column::Done),
            "every move is persisted"
        );
        let events = events(&first);
        assert_eq!(events.len(), keys.len() * 2);
        assert!(
            events
                .iter()
                .all(|event| matches!(event.kind, AuditKind::BoardMove { .. })),
            "every move is audited"
        );
        assert_eq!(first.audit().verify().expect("verify chain"), None);

        let reopened = open(&root);
        assert_eq!(reopened.list(&BoardFilter::all()).len(), keys.len());
    }

    #[test]
    fn columns_round_trip_through_parse_and_json() {
        for column in COLUMNS {
            assert_eq!(Column::parse(column.as_str()).expect("parse"), column);
            let json = serde_json::to_string(&column).expect("serialize column");
            assert_eq!(json, format!("\"{}\"", column.as_str()));
        }
        assert!(matches!(
            Column::parse("nope"),
            Err(BoardError::UnknownColumn(_))
        ));
    }
}
