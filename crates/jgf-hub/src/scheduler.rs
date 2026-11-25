//! Scheduler of the Dark Factory lab: project queue, run slots and the review queue.
//!
//! The scheduler is pure state and decisions. It never launches, stops or deletes a worker,
//! worktree or audit record: the lab runtime reads these decisions later and starts or stops
//! the actual processes. For the same reason this module has no audit hook yet; the lab
//! runtime will write scheduler events when it arrives.
//!
//! Two modes share one lab-wide state, never at the same time:
//!
//! - [`Mode::A`] runs up to [`MAX_TEAMS`] projects, each with one coder;
//! - [`Mode::B`] runs one project with up to [`MAX_CODERS`] coders, one per repository.
//!
//! The Lab Coordinator chooses the mode per phase. The choice is fixed while any coder slot is
//! occupied, so [`Scheduler::set_mode`] refuses to switch until every slot is released.
//!
//! Projects wait in a FIFO queue. A [`Switch`] pauses, resumes or kills the lab: paused and
//! killed labs grant no new coder or review, and [`Scheduler::kill`] returns the slots that were
//! running so the runtime can stop them. Reviews are serialized through one lab-wide FIFO
//! queue, at most one reviewer running, and a review waits while its project has a coder.
//!
//! Everything is persisted in `<dir>/scheduler.json`. An in-process mutex and a cross-process
//! `flock` of `<dir>/scheduler.lock` serialize read-modify-write cycles, so two schedulers on
//! one directory cannot exceed the limits together. Saves use a temporary file and a rename,
//! and a corrupt file or an unknown schema version is an error rather than an empty scheduler.

use std::{
    collections::BTreeSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use jgf_core::{ModelError, ProjectId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum number of projects running at once in [`Mode::A`].
pub const MAX_TEAMS: usize = 4;

/// Maximum number of coders running at once in [`Mode::B`].
pub const MAX_CODERS: usize = 4;

/// Maximum length of a repository name, matching [`ProjectId`].
const MAX_REPO_LEN: usize = 40;

/// Maximum length of a review task reference.
const MAX_TASK_LEN: usize = 256;

/// File name of the persisted scheduler inside its directory.
const STATE_FILE: &str = "scheduler.json";

/// File name of the cross-process lock inside the scheduler directory.
const LOCK_FILE: &str = "scheduler.lock";

/// Schema version written with every scheduler file.
const SCHEMA_VERSION: u32 = 1;

/// How many projects may run together, and how many coders each mode may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// Up to [`MAX_TEAMS`] projects, each with one coder.
    #[serde(rename = "a")]
    A,
    /// One project with up to [`MAX_CODERS`] coders, one per repository.
    #[serde(rename = "b")]
    B,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::A => f.write_str("A"),
            Self::B => f.write_str("B"),
        }
    }
}

/// Whether the lab is running, paused or killed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Switch {
    /// New coders and reviews may start.
    #[serde(rename = "running")]
    Running,
    /// Running work finishes, but nothing new starts.
    #[serde(rename = "paused")]
    Paused,
    /// Running work is stopped by the runtime; nothing starts until a reset.
    #[serde(rename = "killed")]
    Killed,
}

impl fmt::Display for Switch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Paused => f.write_str("paused"),
            Self::Killed => f.write_str("killed"),
        }
    }
}

/// Identifier of one acquired coder slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SlotId(u64);

impl SlotId {
    /// Builds a slot id from its raw number.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw number of this slot id.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One acquired coder slot: who runs and in which repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoderSlot {
    /// Unique id used by [`Scheduler::release`].
    pub id: SlotId,
    /// Project the coder works for.
    pub project: ProjectId,
    /// Repository the coder works in.
    pub repo: String,
}

/// Why [`Scheduler::acquire_coder`] did not grant a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// [`Mode::A`] already runs [`MAX_TEAMS`] projects.
    TeamLimit,
    /// The mode already runs [`MAX_CODERS`] coders.
    CoderLimit,
    /// [`Mode::A`] already runs a coder for this project.
    ProjectAlreadyRunning,
    /// [`Mode::B`] already runs coders for another project.
    OtherProjectRunning,
    /// [`Mode::B`] already runs a coder in this repository.
    RepositoryBusy,
}

/// The outcome of asking for a coder slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoderGrant {
    /// A slot was granted.
    Granted(CoderSlot),
    /// The slot cannot be granted now; the caller may wait and retry.
    Wait(WaitReason),
}

/// One review request in the lab-wide review queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewTicket {
    /// Project the reviewed task belongs to.
    pub project: ProjectId,
    /// Task reference or key that is being reviewed.
    pub task: String,
}

/// Current concurrency limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum coders running at once.
    pub max_coders: usize,
    /// Maximum projects running at once in [`Mode::A`].
    pub max_teams: usize,
}

/// Errors raised by the lab scheduler.
#[derive(Debug, Error)]
pub enum SchedulerError {
    /// Reading or writing scheduler state failed.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// A persisted scheduler file is not valid JSON for a scheduler.
    #[error("invalid scheduler file at {}: {source}", path.display())]
    InvalidState {
        /// Path of the rejected file.
        path: PathBuf,
        /// JSON parser error.
        #[source]
        source: serde_json::Error,
    },

    /// A persisted scheduler file uses a schema version this build does not know.
    #[error("unsupported scheduler schema version {version} at {}", path.display())]
    UnsupportedVersion {
        /// Path of the rejected file.
        path: PathBuf,
        /// Version found in the file.
        version: u32,
    },

    /// A persisted scheduler file is valid JSON but violates scheduler invariants.
    #[error("corrupt scheduler state at {}: {detail}", path.display())]
    CorruptState {
        /// Path of the rejected file.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },

    /// The mode cannot change while coder slots are occupied.
    #[error("cannot switch mode while {running} coder slot(s) are occupied")]
    ModeLocked {
        /// Number of slots still recorded.
        running: usize,
    },

    /// A repository name does not follow the repository name rules.
    #[error("invalid repository name {repo:?}")]
    InvalidRepo {
        /// Rejected repository name.
        repo: String,
    },

    /// A review task reference is empty or too long.
    #[error("invalid review task reference {task:?}")]
    InvalidTask {
        /// Rejected task reference.
        task: String,
    },

    /// A limit would be zero or above its maximum.
    #[error("invalid limit {requested}; it must be between 1 and {max}")]
    InvalidLimit {
        /// Requested limit.
        requested: usize,
        /// Largest accepted limit.
        max: usize,
    },

    /// No coder slot has the requested id.
    #[error("unknown coder slot {0}")]
    UnknownSlot(SlotId),

    /// The same project and task were already queued or are being reviewed.
    #[error("review of task {task:?} in project {project} is already queued or running")]
    DuplicateReview {
        /// Project of the duplicated request.
        project: ProjectId,
        /// Task of the duplicated request.
        task: String,
    },

    /// The scheduler is paused and grants nothing new.
    #[error("the scheduler is paused")]
    Paused,

    /// The scheduler is killed and grants nothing until a reset.
    #[error("the scheduler is killed; reset it first")]
    Killed,

    /// A switch transition is not allowed from the current state.
    #[error("cannot switch from {from} to {to}")]
    InvalidSwitch {
        /// Current switch state.
        from: Switch,
        /// Requested switch state.
        to: Switch,
    },
}

impl From<serde_json::Error> for SchedulerError {
    fn from(source: serde_json::Error) -> Self {
        Self::Model(ModelError::Json(source))
    }
}

/// In-memory state persisted in `<dir>/scheduler.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    mode: Mode,
    switch: Switch,
    max_coders: usize,
    max_teams: usize,
    projects: Vec<ProjectId>,
    next_slot: u64,
    slots: Vec<CoderSlot>,
    reviews: Vec<ReviewTicket>,
    running_review: Option<ReviewTicket>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            mode: Mode::A,
            switch: Switch::Running,
            max_coders: MAX_CODERS,
            max_teams: MAX_TEAMS,
            projects: Vec::new(),
            next_slot: 0,
            slots: Vec::new(),
            reviews: Vec::new(),
            running_review: None,
        }
    }
}

impl State {
    /// Refuses a persisted state that cannot be produced by this scheduler.
    fn validate(&self, path: &Path) -> Result<(), SchedulerError> {
        let corrupt = |detail: String| SchedulerError::CorruptState {
            path: path.to_path_buf(),
            detail,
        };
        if !(1..=MAX_CODERS).contains(&self.max_coders) {
            return Err(corrupt(format!(
                "max coders {} is outside 1..={MAX_CODERS}",
                self.max_coders
            )));
        }
        if !(1..=MAX_TEAMS).contains(&self.max_teams) {
            return Err(corrupt(format!(
                "max teams {} is outside 1..={MAX_TEAMS}",
                self.max_teams
            )));
        }

        let mut queued = BTreeSet::new();
        for project in &self.projects {
            if !queued.insert(project.as_str()) {
                return Err(corrupt(format!("project {project} is queued twice")));
            }
        }

        let mut slot_ids = BTreeSet::new();
        for slot in &self.slots {
            if !slot_ids.insert(slot.id) {
                return Err(corrupt(format!("coder slot {} is stored twice", slot.id)));
            }
            if !repo_is_valid(&slot.repo) {
                return Err(corrupt(format!(
                    "invalid repository name {:?} in coder slot {}",
                    slot.repo, slot.id
                )));
            }
            if slot.id.get() >= self.next_slot {
                return Err(corrupt(format!(
                    "coder slot {} is not below the next slot id {}",
                    slot.id, self.next_slot
                )));
            }
        }

        match self.mode {
            Mode::A => {
                let mut projects = BTreeSet::new();
                for slot in &self.slots {
                    if !projects.insert(slot.project.as_str()) {
                        return Err(corrupt(format!(
                            "mode A has two coders on project {}",
                            slot.project
                        )));
                    }
                }
            }
            Mode::B => {
                if let Some(first) = self.slots.first() {
                    let mut repos = BTreeSet::new();
                    for slot in &self.slots {
                        if slot.project != first.project {
                            return Err(corrupt(format!(
                                "mode B runs projects {} and {}",
                                first.project, slot.project
                            )));
                        }
                        if !repos.insert(slot.repo.as_str()) {
                            return Err(corrupt(format!(
                                "mode B has two coders in repository {:?}",
                                slot.repo
                            )));
                        }
                    }
                }
            }
        }

        let mut reviews = BTreeSet::new();
        for ticket in self.reviews.iter().chain(self.running_review.iter()) {
            if !task_is_valid(&ticket.task) {
                return Err(corrupt(format!(
                    "invalid review task reference {:?}",
                    ticket.task
                )));
            }
            if !reviews.insert((ticket.project.as_str(), ticket.task.as_str())) {
                return Err(corrupt(format!(
                    "review of task {:?} in project {} is stored twice",
                    ticket.task, ticket.project
                )));
            }
        }

        Ok(())
    }
}

/// Borrowed on-disk form of the scheduler, including its schema version.
#[derive(Debug, Serialize)]
struct StateFileRef<'a> {
    version: u32,
    #[serde(flatten)]
    state: &'a State,
}

/// First field read from a scheduler file, before the rest of the state is parsed.
#[derive(Debug, Deserialize)]
struct VersionHeader {
    version: u32,
}

/// The lab scheduler, persisted as `<dir>/scheduler.json`.
///
/// A scheduler only decides. It never starts, stops or deletes a worker, worktree or audit
/// record; the lab runtime consumes its decisions and does that later.
#[derive(Debug)]
pub struct Scheduler {
    dir: PathBuf,
    path: PathBuf,
    lock_path: PathBuf,
    state: Mutex<()>,
}

impl Scheduler {
    /// Opens the scheduler under `dir`, creating an empty scheduler when the file is absent.
    ///
    /// A file that exists but is corrupt, or whose schema version is unknown, is an error.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, SchedulerError> {
        let dir = dir.into();
        let path = dir.join(STATE_FILE);
        let lock_path = dir.join(LOCK_FILE);
        read_state(&path)?;
        Ok(Self {
            dir,
            path,
            lock_path,
            state: Mutex::new(()),
        })
    }

    /// Directory that holds `scheduler.json`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the persisted scheduler file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current mode of the lab.
    pub fn mode(&self) -> Result<Mode, SchedulerError> {
        Ok(self.read()?.mode)
    }

    /// Current switch state of the lab.
    pub fn switch(&self) -> Result<Switch, SchedulerError> {
        Ok(self.read()?.switch)
    }

    /// Current coder and team limits.
    pub fn limits(&self) -> Result<Limits, SchedulerError> {
        let state = self.read()?;
        Ok(Limits {
            max_coders: state.max_coders,
            max_teams: state.max_teams,
        })
    }

    /// Queued projects, in FIFO order.
    pub fn queued_projects(&self) -> Result<Vec<ProjectId>, SchedulerError> {
        Ok(self.read()?.projects)
    }

    /// Coder slots currently recorded, in acquisition order.
    pub fn coder_slots(&self) -> Result<Vec<CoderSlot>, SchedulerError> {
        Ok(self.read()?.slots)
    }

    /// Review requests waiting in the lab-wide queue, in FIFO order.
    pub fn review_queue(&self) -> Result<Vec<ReviewTicket>, SchedulerError> {
        Ok(self.read()?.reviews)
    }

    /// Review currently being performed, when one is running.
    pub fn running_review(&self) -> Result<Option<ReviewTicket>, SchedulerError> {
        Ok(self.read()?.running_review)
    }

    /// Chooses the mode for this phase.
    ///
    /// The choice is refused with [`SchedulerError::ModeLocked`] while any coder slot is
    /// occupied. Setting the current mode again is always accepted.
    pub fn set_mode(&self, mode: Mode) -> Result<(), SchedulerError> {
        self.mutate(|state| {
            if state.mode == mode {
                return Ok(());
            }
            if !state.slots.is_empty() {
                return Err(SchedulerError::ModeLocked {
                    running: state.slots.len(),
                });
            }
            state.mode = mode;
            Ok(())
        })
    }

    /// Appends `project` to the FIFO queue and returns whether it was added.
    ///
    /// A project already in the queue is left where it is, so the queue never holds duplicates.
    pub fn enqueue(&self, project: ProjectId) -> Result<bool, SchedulerError> {
        self.mutate(|state| {
            if state.projects.contains(&project) {
                return Ok(false);
            }
            state.projects.push(project);
            Ok(true)
        })
    }

    /// Removes `project` from the queue wherever it is, keeping the rest in order.
    ///
    /// Returns whether the project was queued.
    pub fn remove(&self, project: &ProjectId) -> Result<bool, SchedulerError> {
        self.mutate(|state| {
            let Some(index) = state.projects.iter().position(|queued| queued == project) else {
                return Ok(false);
            };
            state.projects.remove(index);
            Ok(true)
        })
    }

    /// Removes and returns the queue head.
    pub fn dequeue(&self) -> Result<Option<ProjectId>, SchedulerError> {
        self.mutate(|state| {
            if state.projects.is_empty() {
                return Ok(None);
            }
            Ok(Some(state.projects.remove(0)))
        })
    }

    /// Returns the earliest queued project that may start now.
    ///
    /// [`Mode::A`] skips projects that already run a coder and projects past the team limit.
    /// [`Mode::B`] only selects a project when every running coder belongs to it, so it never
    /// mixes two projects. A paused or killed lab has no next project.
    pub fn next_to_start(&self) -> Result<Option<ProjectId>, SchedulerError> {
        Ok(next_project(&self.read()?))
    }

    /// Asks for a coder slot in `repo` for `project`.
    ///
    /// Returns [`CoderGrant::Granted`] with a fresh slot id, or [`CoderGrant::Wait`] with the
    /// reason the limits or the mode refuse it now. A paused or killed lab returns a typed
    /// error instead. Running slots are never evicted by a later limit change.
    pub fn acquire_coder(
        &self,
        project: ProjectId,
        repo: impl Into<String>,
    ) -> Result<CoderGrant, SchedulerError> {
        let repo = repo.into();
        validate_repo(&repo)?;
        self.mutate(|state| {
            ensure_running(state)?;
            if let Some(reason) = coder_wait_reason(state, &project, &repo) {
                return Ok(CoderGrant::Wait(reason));
            }
            let id = SlotId::new(state.next_slot);
            state.next_slot =
                state
                    .next_slot
                    .checked_add(1)
                    .ok_or_else(|| SchedulerError::CorruptState {
                        path: self.path.clone(),
                        detail: "slot id counter is exhausted".to_string(),
                    })?;
            let slot = CoderSlot { id, project, repo };
            state.slots.push(slot.clone());
            Ok(CoderGrant::Granted(slot))
        })
    }

    /// Releases a coder slot and returns it.
    ///
    /// The slot is removed whatever the switch state is, so the runtime can always finish or
    /// stop work it started before a pause or a kill.
    pub fn release(&self, slot: SlotId) -> Result<CoderSlot, SchedulerError> {
        self.mutate(|state| {
            let index = state
                .slots
                .iter()
                .position(|running| running.id == slot)
                .ok_or(SchedulerError::UnknownSlot(slot))?;
            Ok(state.slots.remove(index))
        })
    }

    /// Queues a review of `task` in `project`.
    ///
    /// The request carries no duplicate of the same project and task, whether that pair is
    /// still queued or already running. A paused or killed lab refuses new requests.
    pub fn request_review(
        &self,
        project: ProjectId,
        task: impl Into<String>,
    ) -> Result<ReviewTicket, SchedulerError> {
        let task = task.into();
        validate_task(&task)?;
        self.mutate(|state| {
            ensure_running(state)?;
            let ticket = ReviewTicket { project, task };
            let already_requested = state
                .reviews
                .iter()
                .chain(state.running_review.iter())
                .any(|queued| queued == &ticket);
            if already_requested {
                return Err(SchedulerError::DuplicateReview {
                    project: ticket.project,
                    task: ticket.task,
                });
            }
            state.reviews.push(ticket.clone());
            Ok(ticket)
        })
    }

    /// Starts the first review that may run now.
    ///
    /// A review never overlaps another: while one runs, this returns `None`. Otherwise the
    /// queue is scanned in FIFO order, entries whose project still has a coder are skipped
    /// without being reordered, and the first eligible entry is removed and returned. A
    /// paused or killed lab starts nothing.
    pub fn start_review(&self) -> Result<Option<ReviewTicket>, SchedulerError> {
        self.mutate(|state| {
            if state.switch != Switch::Running || state.running_review.is_some() {
                return Ok(None);
            }
            let Some(index) = state
                .reviews
                .iter()
                .position(|ticket| !project_has_coder(state, &ticket.project))
            else {
                return Ok(None);
            };
            let ticket = state.reviews.remove(index);
            state.running_review = Some(ticket.clone());
            Ok(Some(ticket))
        })
    }

    /// Starts the next eligible review only if it matches `ticket`.
    ///
    /// Unlike [`Scheduler::start_review`], this leaves the queue unchanged when a concurrent
    /// scheduler update makes another review eligible first.
    pub fn start_review_if_next(&self, ticket: &ReviewTicket) -> Result<bool, SchedulerError> {
        self.mutate(|state| {
            if state.switch != Switch::Running || state.running_review.is_some() {
                return Ok(false);
            }
            let Some(index) = state
                .reviews
                .iter()
                .position(|queued| !project_has_coder(state, &queued.project))
            else {
                return Ok(false);
            };
            if state.reviews[index] != *ticket {
                return Ok(false);
            }
            let ticket = state.reviews.remove(index);
            state.running_review = Some(ticket);
            Ok(true)
        })
    }

    /// Finishes the running review and returns it, when one was running.
    pub fn finish_review(&self) -> Result<Option<ReviewTicket>, SchedulerError> {
        self.mutate(|state| Ok(state.running_review.take()))
    }

    /// Pauses the lab: running slots stay recorded, nothing new starts.
    pub fn pause(&self) -> Result<(), SchedulerError> {
        self.change_switch(Switch::Running, Switch::Paused)
    }

    /// Resumes a paused lab. Only [`Switch::Paused`] may resume.
    pub fn resume(&self) -> Result<(), SchedulerError> {
        self.change_switch(Switch::Paused, Switch::Running)
    }

    /// Kills the lab and returns the coder slots that were running.
    ///
    /// The slots stay recorded until the runtime releases them, and nothing is deleted: the
    /// scheduler leaves worktrees and audit records to the runtime. A killed lab stays killed
    /// until [`Scheduler::reset_after_kill`].
    pub fn kill(&self) -> Result<Vec<CoderSlot>, SchedulerError> {
        self.mutate(|state| {
            let from = state.switch;
            if !matches!(from, Switch::Running | Switch::Paused) {
                return Err(SchedulerError::InvalidSwitch {
                    from,
                    to: Switch::Killed,
                });
            }
            state.switch = Switch::Killed;
            Ok(state.slots.clone())
        })
    }

    /// Returns a killed lab to running. Only [`Switch::Killed`] may reset.
    pub fn reset_after_kill(&self) -> Result<(), SchedulerError> {
        self.change_switch(Switch::Killed, Switch::Running)
    }

    /// Lowers or raises the coder limit, up to [`MAX_CODERS`].
    ///
    /// The limit is never zero, never above the maximum, and never evicts a running slot.
    pub fn set_max_coders(&self, max: usize) -> Result<(), SchedulerError> {
        ensure_limit(max, MAX_CODERS)?;
        self.mutate(|state| {
            state.max_coders = max;
            Ok(())
        })
    }

    /// Lowers or raises the [`Mode::A`] team limit, up to [`MAX_TEAMS`].
    ///
    /// The limit is never zero, never above the maximum, and never evicts a running slot.
    pub fn set_max_teams(&self, max: usize) -> Result<(), SchedulerError> {
        ensure_limit(max, MAX_TEAMS)?;
        self.mutate(|state| {
            state.max_teams = max;
            Ok(())
        })
    }

    /// Moves the switch from `from` to `to`, refusing every other transition.
    fn change_switch(&self, from_expected: Switch, to: Switch) -> Result<(), SchedulerError> {
        self.mutate(|state| {
            let from = state.switch;
            if from != from_expected {
                return Err(SchedulerError::InvalidSwitch { from, to });
            }
            state.switch = to;
            Ok(())
        })
    }

    /// Re-reads the state under the exclusive lock, applies `change`, then saves the result.
    ///
    /// The in-process mutex and `<dir>/scheduler.lock` are held while the file is read and
    /// replaced, so the limit check and the grant it allows happen under the same lock.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut State) -> Result<T, SchedulerError>,
    ) -> Result<T, SchedulerError> {
        let _state = self.lock();
        let lock_file = self.open_lock_file()?;
        let _lock = FileLock::exclusive(&lock_file, &self.lock_path)?;
        let mut state = read_state(&self.path)?;
        let value = change(&mut state)?;
        self.persist(&state)?;
        Ok(value)
    }

    /// Reads the latest persisted state under a shared lock.
    fn read(&self) -> Result<State, SchedulerError> {
        let _state = self.lock();
        let lock_file = self.open_lock_file()?;
        let _lock = FileLock::shared(&lock_file, &self.lock_path)?;
        read_state(&self.path)
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

    /// Saves `state` with an atomic replacement of `<dir>/scheduler.json`.
    fn persist(&self, state: &State) -> Result<(), SchedulerError> {
        let bytes = serde_json::to_vec_pretty(&StateFileRef {
            version: SCHEMA_VERSION,
            state,
        })?;
        write_atomic(&self.path, &bytes).map_err(SchedulerError::from)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Why [`Scheduler::acquire_coder`] would not grant a slot, or `None` when it would.
fn coder_wait_reason(state: &State, project: &ProjectId, repo: &str) -> Option<WaitReason> {
    match state.mode {
        Mode::A => {
            if state.slots.iter().any(|slot| &slot.project == project) {
                Some(WaitReason::ProjectAlreadyRunning)
            } else if state.slots.len() >= state.max_teams {
                Some(WaitReason::TeamLimit)
            } else if state.slots.len() >= state.max_coders {
                Some(WaitReason::CoderLimit)
            } else {
                None
            }
        }
        Mode::B => {
            if state.slots.iter().any(|slot| &slot.project != project) {
                Some(WaitReason::OtherProjectRunning)
            } else if state
                .slots
                .iter()
                .any(|slot| &slot.project == project && slot.repo == repo)
            {
                Some(WaitReason::RepositoryBusy)
            } else if state.slots.len() >= state.max_coders {
                Some(WaitReason::CoderLimit)
            } else {
                None
            }
        }
    }
}

/// The earliest queued project that may start under the current mode and limits.
fn next_project(state: &State) -> Option<ProjectId> {
    if state.switch != Switch::Running {
        return None;
    }
    match state.mode {
        Mode::A => {
            let team_limit = state.max_teams.min(state.max_coders);
            if state.slots.len() >= team_limit {
                return None;
            }
            state
                .projects
                .iter()
                .find(|project| !state.slots.iter().any(|slot| &slot.project == *project))
                .cloned()
        }
        Mode::B => {
            if state.slots.len() >= state.max_coders {
                return None;
            }
            state
                .projects
                .iter()
                .find(|project| state.slots.iter().all(|slot| &slot.project == *project))
                .cloned()
        }
    }
}

/// Whether `project` currently has a coder, which makes its review wait.
fn project_has_coder(state: &State, project: &ProjectId) -> bool {
    state.slots.iter().any(|slot| &slot.project == project)
}

fn ensure_running(state: &State) -> Result<(), SchedulerError> {
    match state.switch {
        Switch::Running => Ok(()),
        Switch::Paused => Err(SchedulerError::Paused),
        Switch::Killed => Err(SchedulerError::Killed),
    }
}

fn ensure_limit(requested: usize, max: usize) -> Result<(), SchedulerError> {
    if (1..=max).contains(&requested) {
        Ok(())
    } else {
        Err(SchedulerError::InvalidLimit { requested, max })
    }
}

fn validate_repo(repo: &str) -> Result<(), SchedulerError> {
    if repo_is_valid(repo) {
        Ok(())
    } else {
        Err(SchedulerError::InvalidRepo {
            repo: repo.to_string(),
        })
    }
}

fn repo_is_valid(repo: &str) -> bool {
    !repo.is_empty()
        && repo.len() <= MAX_REPO_LEN
        && repo.starts_with(|c: char| c.is_ascii_lowercase())
        && repo
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn validate_task(task: &str) -> Result<(), SchedulerError> {
    if task_is_valid(task) {
        Ok(())
    } else {
        Err(SchedulerError::InvalidTask {
            task: task.to_string(),
        })
    }
}

fn task_is_valid(task: &str) -> bool {
    !task.trim().is_empty() && task.len() <= MAX_TASK_LEN && !task.chars().any(char::is_control)
}

/// Reads and validates the persisted scheduler at `path`, or a fresh one when it is absent.
fn read_state(path: &Path) -> Result<State, SchedulerError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(State::default()),
        Err(source) => return Err(io_error(path, source).into()),
    };
    let header = serde_json::from_slice::<VersionHeader>(&bytes).map_err(|source| {
        SchedulerError::InvalidState {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if header.version != SCHEMA_VERSION {
        return Err(SchedulerError::UnsupportedVersion {
            path: path.to_path_buf(),
            version: header.version,
        });
    }
    let state =
        serde_json::from_slice::<State>(&bytes).map_err(|source| SchedulerError::InvalidState {
            path: path.to_path_buf(),
            source,
        })?;
    state.validate(path)?;
    Ok(state)
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

    use super::*;

    /// Temporary scheduler directory removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jgf-hub-scheduler-{}-{suffix}", std::process::id()));
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

    fn open(root: &TempRoot) -> Scheduler {
        Scheduler::open(root.path()).expect("open scheduler")
    }

    fn granted(outcome: CoderGrant) -> CoderSlot {
        match outcome {
            CoderGrant::Granted(slot) => slot,
            CoderGrant::Wait(reason) => panic!("expected a granted slot, got {reason:?}"),
        }
    }

    fn wait_reason(outcome: CoderGrant) -> WaitReason {
        match outcome {
            CoderGrant::Granted(slot) => panic!("expected a wait, got {slot:?}"),
            CoderGrant::Wait(reason) => reason,
        }
    }

    #[test]
    fn mode_a_grants_four_teams_and_waits_for_the_fifth() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        assert_eq!(scheduler.mode().expect("mode"), Mode::A);

        let alpha = granted(
            scheduler
                .acquire_coder(project("alpha"), "backend")
                .expect("alpha"),
        );
        for name in ["beta", "gamma", "delta"] {
            granted(
                scheduler
                    .acquire_coder(project(name), "backend")
                    .expect("team"),
            );
        }
        assert_eq!(scheduler.coder_slots().expect("slots").len(), MAX_TEAMS);

        assert_eq!(
            wait_reason(
                scheduler
                    .acquire_coder(project("epsilon"), "backend")
                    .expect("fifth")
            ),
            WaitReason::TeamLimit
        );
        assert_eq!(
            wait_reason(
                scheduler
                    .acquire_coder(project("alpha"), "other")
                    .expect("second alpha")
            ),
            WaitReason::ProjectAlreadyRunning
        );
        assert_eq!(scheduler.coder_slots().expect("slots").len(), MAX_TEAMS);
        assert_eq!(alpha.project, project("alpha"));
    }

    #[test]
    fn mode_b_grants_four_repositories_of_one_project() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        scheduler.set_mode(Mode::B).expect("switch to B");

        for repo in ["backend", "frontend", "infra", "mobile"] {
            granted(
                scheduler
                    .acquire_coder(project("alpha"), repo)
                    .expect("repo"),
            );
        }
        assert_eq!(scheduler.coder_slots().expect("slots").len(), MAX_CODERS);

        assert_eq!(
            wait_reason(
                scheduler
                    .acquire_coder(project("alpha"), "e2e")
                    .expect("fifth")
            ),
            WaitReason::CoderLimit
        );
        assert_eq!(
            wait_reason(
                scheduler
                    .acquire_coder(project("alpha"), "backend")
                    .expect("same repo")
            ),
            WaitReason::RepositoryBusy
        );
        assert_eq!(
            wait_reason(
                scheduler
                    .acquire_coder(project("beta"), "backend")
                    .expect("other project")
            ),
            WaitReason::OtherProjectRunning
        );
    }

    #[test]
    fn mode_is_locked_while_slots_run() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        scheduler.set_mode(Mode::B).expect("switch to B");
        let slot = granted(
            scheduler
                .acquire_coder(project("alpha"), "backend")
                .expect("slot"),
        );

        scheduler
            .set_mode(Mode::B)
            .expect("setting the current mode is allowed");
        assert!(matches!(
            scheduler.set_mode(Mode::A),
            Err(SchedulerError::ModeLocked { running: 1 })
        ));
        assert_eq!(scheduler.mode().expect("mode"), Mode::B);

        scheduler.release(slot.id).expect("release");
        scheduler.set_mode(Mode::A).expect("switch to A");
        assert_eq!(scheduler.mode().expect("mode"), Mode::A);
    }

    #[test]
    fn next_to_start_honours_the_queue_mode_and_limits() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        for name in ["alpha", "beta", "gamma"] {
            scheduler.enqueue(project(name)).expect("enqueue");
        }
        assert_eq!(
            scheduler.next_to_start().expect("next"),
            Some(project("alpha"))
        );

        granted(
            scheduler
                .acquire_coder(project("alpha"), "backend")
                .expect("alpha"),
        );
        assert_eq!(
            scheduler.next_to_start().expect("next"),
            Some(project("beta")),
            "mode A skips a project that already runs its coder"
        );
        granted(
            scheduler
                .acquire_coder(project("beta"), "backend")
                .expect("beta"),
        );
        assert_eq!(
            scheduler.next_to_start().expect("next"),
            Some(project("gamma"))
        );
        granted(
            scheduler
                .acquire_coder(project("gamma"), "backend")
                .expect("gamma"),
        );
        granted(
            scheduler
                .acquire_coder(project("delta"), "backend")
                .expect("delta"),
        );
        assert!(scheduler.next_to_start().expect("next").is_none());

        let root = TempRoot::new();
        let scheduler = open(&root);
        scheduler.set_mode(Mode::B).expect("switch to B");
        scheduler.enqueue(project("beta")).expect("enqueue beta");
        let alpha = granted(
            scheduler
                .acquire_coder(project("alpha"), "backend")
                .expect("alpha"),
        );
        assert!(
            scheduler.next_to_start().expect("next").is_none(),
            "mode B cannot start the queued beta while alpha runs"
        );
        scheduler.enqueue(project("alpha")).expect("enqueue alpha");
        assert_eq!(
            scheduler.next_to_start().expect("next"),
            Some(project("alpha")),
            "alpha may add another coder, beta still may not start"
        );
        scheduler.release(alpha.id).expect("release");
        assert_eq!(
            scheduler.next_to_start().expect("next"),
            Some(project("beta")),
            "with no coder running the queue head may start"
        );
    }

    #[test]
    fn mode_limits_and_switch_survive_a_reopen() {
        let root = TempRoot::new();
        {
            let scheduler = open(&root);
            scheduler.set_mode(Mode::B).expect("switch to B");
            scheduler.set_max_coders(2).expect("lower coders");
            scheduler.set_max_teams(3).expect("lower teams");
            scheduler.pause().expect("pause");
            scheduler.enqueue(project("alpha")).expect("enqueue");
        }

        let scheduler = open(&root);
        assert_eq!(scheduler.mode().expect("mode"), Mode::B);
        assert_eq!(scheduler.switch().expect("switch"), Switch::Paused);
        assert_eq!(
            scheduler.limits().expect("limits"),
            Limits {
                max_coders: 2,
                max_teams: 3,
            }
        );
        assert_eq!(
            scheduler.queued_projects().expect("queue"),
            [project("alpha")]
        );
    }

    #[test]
    fn review_queue_is_fifo_and_runs_one_at_a_time() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        for (name, task) in [
            ("alpha", "ALPHA-1"),
            ("beta", "BETA-1"),
            ("gamma", "GAMMA-1"),
        ] {
            scheduler
                .request_review(project(name), task)
                .expect("request review");
        }

        let first = scheduler.start_review().expect("start").expect("granted");
        assert_eq!(first.project, project("alpha"));
        assert_eq!(first.task, "ALPHA-1");
        assert!(scheduler.start_review().expect("start again").is_none());
        assert_eq!(
            scheduler.finish_review().expect("finish"),
            Some(first.clone())
        );

        let second = scheduler.start_review().expect("start").expect("granted");
        assert_eq!(second.project, project("beta"));
        scheduler.finish_review().expect("finish");

        let third = scheduler.start_review().expect("start").expect("granted");
        assert_eq!(third.project, project("gamma"));
        scheduler.finish_review().expect("finish");

        assert!(scheduler.start_review().expect("empty").is_none());
        assert!(scheduler.review_queue().expect("queue").is_empty());
    }

    #[test]
    fn review_skips_a_project_whose_coder_runs_without_reordering_the_rest() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        for (name, task) in [
            ("alpha", "ALPHA-1"),
            ("beta", "BETA-1"),
            ("gamma", "GAMMA-1"),
        ] {
            scheduler
                .request_review(project(name), task)
                .expect("request review");
        }
        let alpha_coder = granted(
            scheduler
                .acquire_coder(project("alpha"), "backend")
                .expect("alpha coder"),
        );

        let beta = scheduler.start_review().expect("start").expect("granted");
        assert_eq!(beta.project, project("beta"));
        assert_eq!(
            scheduler
                .review_queue()
                .expect("queue")
                .into_iter()
                .map(|ticket| ticket.project)
                .collect::<Vec<_>>(),
            [project("alpha"), project("gamma")]
        );
        scheduler.finish_review().expect("finish");

        let gamma = scheduler.start_review().expect("start").expect("granted");
        assert_eq!(gamma.project, project("gamma"));
        assert_eq!(
            scheduler
                .review_queue()
                .expect("queue")
                .into_iter()
                .map(|ticket| ticket.project)
                .collect::<Vec<_>>(),
            [project("alpha")]
        );
        scheduler.finish_review().expect("finish");

        scheduler.release(alpha_coder.id).expect("release");
        let alpha = scheduler.start_review().expect("start").expect("granted");
        assert_eq!(alpha.project, project("alpha"));
    }

    #[test]
    fn start_review_if_next_does_not_start_a_different_eligible_ticket() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        let alpha = scheduler
            .request_review(project("alpha"), "ALPHA-1")
            .expect("queue alpha");
        let beta = scheduler
            .request_review(project("beta"), "BETA-1")
            .expect("queue beta");
        let alpha_coder = granted(
            scheduler
                .acquire_coder(project("alpha"), "backend")
                .expect("acquire alpha coder"),
        );

        assert!(
            !scheduler
                .start_review_if_next(&alpha)
                .expect("alpha is no longer eligible")
        );
        assert!(
            scheduler
                .running_review()
                .expect("running review")
                .is_none()
        );
        assert_eq!(
            scheduler.review_queue().expect("queue"),
            [alpha.clone(), beta.clone()]
        );

        assert!(
            scheduler
                .start_review_if_next(&beta)
                .expect("beta is next eligible")
        );
        assert_eq!(
            scheduler.running_review().expect("running review"),
            Some(beta.clone())
        );
        assert_eq!(
            scheduler.review_queue().expect("queue").as_slice(),
            std::slice::from_ref(&alpha)
        );
        assert_eq!(scheduler.finish_review().expect("finish beta"), Some(beta));

        scheduler
            .release(alpha_coder.id)
            .expect("release alpha coder");
        assert!(scheduler.start_review_if_next(&alpha).expect("start alpha"));
        assert_eq!(
            scheduler.finish_review().expect("finish alpha"),
            Some(alpha)
        );
    }

    #[test]
    fn duplicate_review_requests_are_refused() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        let ticket = scheduler
            .request_review(project("alpha"), "ALPHA-1")
            .expect("request");

        assert!(matches!(
            scheduler.request_review(project("alpha"), "ALPHA-1"),
            Err(SchedulerError::DuplicateReview { .. })
        ));
        assert_eq!(
            scheduler.start_review().expect("start"),
            Some(ticket.clone())
        );
        assert!(matches!(
            scheduler.request_review(project("alpha"), "ALPHA-1"),
            Err(SchedulerError::DuplicateReview { .. })
        ));

        scheduler.finish_review().expect("finish");
        scheduler
            .request_review(project("alpha"), "ALPHA-1")
            .expect("request again after the review finished");
        assert!(matches!(
            scheduler.request_review(project("alpha"), ""),
            Err(SchedulerError::InvalidTask { .. })
        ));
    }

    #[test]
    fn pause_resume_and_kill_follow_the_switch_rules() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        scheduler.enqueue(project("alpha")).expect("enqueue");
        let alpha = granted(
            scheduler
                .acquire_coder(project("alpha"), "backend")
                .expect("alpha"),
        );

        scheduler.pause().expect("pause");
        assert_eq!(scheduler.switch().expect("switch"), Switch::Paused);
        assert!(matches!(
            scheduler.acquire_coder(project("beta"), "backend"),
            Err(SchedulerError::Paused)
        ));
        assert!(matches!(
            scheduler.request_review(project("alpha"), "ALPHA-1"),
            Err(SchedulerError::Paused)
        ));
        assert!(scheduler.next_to_start().expect("next").is_none());
        assert!(scheduler.start_review().expect("start").is_none());

        scheduler.resume().expect("resume");
        assert_eq!(scheduler.switch().expect("switch"), Switch::Running);
        assert!(matches!(
            scheduler.resume(),
            Err(SchedulerError::InvalidSwitch {
                from: Switch::Running,
                to: Switch::Running
            })
        ));

        let beta = granted(
            scheduler
                .acquire_coder(project("beta"), "backend")
                .expect("beta"),
        );
        scheduler.pause().expect("pause again");
        let killed = scheduler.kill().expect("kill");
        assert_eq!(
            killed.iter().map(|slot| slot.id).collect::<Vec<_>>(),
            [alpha.id, beta.id]
        );
        assert_eq!(scheduler.switch().expect("switch"), Switch::Killed);
        assert_eq!(
            scheduler.coder_slots().expect("slots").len(),
            2,
            "kill returns the running slots but does not delete them"
        );

        assert!(matches!(
            scheduler.acquire_coder(project("gamma"), "backend"),
            Err(SchedulerError::Killed)
        ));
        assert!(matches!(
            scheduler.request_review(project("alpha"), "ALPHA-1"),
            Err(SchedulerError::Killed)
        ));
        assert!(scheduler.next_to_start().expect("next").is_none());
        assert!(scheduler.start_review().expect("start").is_none());
        assert!(matches!(
            scheduler.resume(),
            Err(SchedulerError::InvalidSwitch {
                from: Switch::Killed,
                to: Switch::Running
            })
        ));
        assert!(matches!(
            scheduler.kill(),
            Err(SchedulerError::InvalidSwitch {
                from: Switch::Killed,
                to: Switch::Killed
            })
        ));

        scheduler.reset_after_kill().expect("reset");
        assert_eq!(scheduler.switch().expect("switch"), Switch::Running);
        assert_eq!(
            scheduler.coder_slots().expect("slots").len(),
            2,
            "reset keeps the slots until they are released"
        );
        scheduler.release(alpha.id).expect("release alpha");
        scheduler.release(beta.id).expect("release beta");
        granted(
            scheduler
                .acquire_coder(project("gamma"), "backend")
                .expect("after reset"),
        );
    }

    #[test]
    fn lowered_limits_wait_new_coders_without_evicting_running_ones() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        scheduler.set_max_teams(2).expect("lower teams");
        for name in ["alpha", "beta"] {
            granted(
                scheduler
                    .acquire_coder(project(name), "backend")
                    .expect("team"),
            );
        }
        assert_eq!(
            wait_reason(
                scheduler
                    .acquire_coder(project("gamma"), "backend")
                    .expect("third")
            ),
            WaitReason::TeamLimit
        );
        scheduler
            .set_max_teams(1)
            .expect("lower teams below running");
        assert_eq!(
            scheduler.coder_slots().expect("slots").len(),
            2,
            "a lower limit never evicts running slots"
        );

        let root = TempRoot::new();
        let scheduler = open(&root);
        scheduler.set_max_coders(2).expect("lower coders");
        for name in ["alpha", "beta"] {
            granted(
                scheduler
                    .acquire_coder(project(name), "backend")
                    .expect("team"),
            );
        }
        assert_eq!(
            wait_reason(
                scheduler
                    .acquire_coder(project("gamma"), "backend")
                    .expect("third")
            ),
            WaitReason::CoderLimit
        );
        scheduler
            .set_max_coders(1)
            .expect("lower coders below running");
        assert_eq!(scheduler.coder_slots().expect("slots").len(), 2);

        for invalid in [0, MAX_TEAMS + 1] {
            assert!(matches!(
                scheduler.set_max_teams(invalid),
                Err(SchedulerError::InvalidLimit { requested, .. }) if requested == invalid
            ));
            assert!(matches!(
                scheduler.set_max_coders(invalid),
                Err(SchedulerError::InvalidLimit { requested, .. }) if requested == invalid
            ));
        }
    }

    #[test]
    fn queue_order_survives_a_reopen_and_has_no_duplicates() {
        let root = TempRoot::new();
        {
            let scheduler = open(&root);
            assert!(scheduler.enqueue(project("alpha")).expect("enqueue"));
            assert!(!scheduler.enqueue(project("alpha")).expect("duplicate"));
            scheduler.enqueue(project("beta")).expect("enqueue");
            scheduler.enqueue(project("gamma")).expect("enqueue");
            assert!(scheduler.remove(&project("beta")).expect("remove"));
            assert!(!scheduler.remove(&project("beta")).expect("remove again"));
        }

        let scheduler = open(&root);
        assert_eq!(
            scheduler.queued_projects().expect("queue"),
            [project("alpha"), project("gamma")]
        );
        assert_eq!(
            scheduler.dequeue().expect("dequeue"),
            Some(project("alpha"))
        );
        assert_eq!(
            scheduler.queued_projects().expect("queue"),
            [project("gamma")]
        );
    }

    #[test]
    fn concurrent_schedulers_never_exceed_the_mode_a_limit() {
        const THREADS: usize = 8;

        let root = TempRoot::new();
        let first = Arc::new(open(&root));
        let second = Arc::new(open(&root));
        let acquirers: Vec<_> = (0..THREADS)
            .map(|index| {
                let scheduler = if index % 2 == 0 {
                    Arc::clone(&first)
                } else {
                    Arc::clone(&second)
                };
                thread::spawn(move || {
                    let project = project(&format!("team-{index}"));
                    scheduler
                        .acquire_coder(project, "backend")
                        .expect("acquire")
                })
            })
            .collect();

        let outcomes: Vec<CoderGrant> = acquirers
            .into_iter()
            .map(|handle| handle.join().expect("join acquirer"))
            .collect();
        let slots: Vec<CoderSlot> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                CoderGrant::Granted(slot) => Some(slot.clone()),
                CoderGrant::Wait(_) => None,
            })
            .collect();
        assert_eq!(slots.len(), MAX_TEAMS);
        assert!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, CoderGrant::Wait(_)))
                .all(|outcome| matches!(outcome, CoderGrant::Wait(WaitReason::TeamLimit)))
        );
        let ids: BTreeSet<SlotId> = slots.iter().map(|slot| slot.id).collect();
        assert_eq!(ids.len(), MAX_TEAMS, "every grant has a unique slot id");

        assert_eq!(first.coder_slots().expect("first slots").len(), MAX_TEAMS);
        assert_eq!(second.coder_slots().expect("second slots").len(), MAX_TEAMS);
        assert_eq!(
            wait_reason(
                first
                    .acquire_coder(project("late"), "backend")
                    .expect("late")
            ),
            WaitReason::TeamLimit
        );
    }

    #[test]
    fn corrupt_state_and_unknown_version_are_errors() {
        let root = TempRoot::new();
        fs::write(root.path().join(STATE_FILE), b"{not json").expect("write corrupt state");
        assert!(matches!(
            Scheduler::open(root.path()),
            Err(SchedulerError::InvalidState { .. })
        ));

        let bad_version = serde_json::json!({ "version": SCHEMA_VERSION + 1 });
        fs::write(
            root.path().join(STATE_FILE),
            serde_json::to_vec(&bad_version).expect("json"),
        )
        .expect("write bad version");
        assert!(matches!(
            Scheduler::open(root.path()),
            Err(SchedulerError::UnsupportedVersion { version, .. }) if version == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn hostile_repository_names_are_rejected() {
        let root = TempRoot::new();
        let scheduler = open(&root);
        let mut hostile = vec![
            String::new(),
            "Backend".to_string(),
            "../etc".to_string(),
            "a b".to_string(),
            "a/b".to_string(),
            "-x".to_string(),
            "a".repeat(MAX_REPO_LEN + 1),
        ];
        hostile.push("a\0b".to_string());

        for repo in hostile {
            assert!(
                matches!(
                    scheduler.acquire_coder(project("alpha"), repo),
                    Err(SchedulerError::InvalidRepo { .. })
                ),
                "hostile repository name must be refused"
            );
        }
        assert!(scheduler.coder_slots().expect("slots").is_empty());
    }
}
