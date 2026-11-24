//! Usage meter, throttle policy and the resume queue of the Dark Factory lab.
//!
//! The meter holds the latest usage window of each provider: what it spent, its limit and when
//! the window resets. The policy turns a window into a [`Throttle`] decision: normal work,
//! reduced concurrency (soft) or no new work until the window resets (hard). Everything is pure
//! state and decisions: no process is spawned, no network is used, no clock is read here. Every
//! decision takes `now` in unix seconds from its caller.
//!
//! A worker stopped by the usage limit is not a failure. Its piece is parked in a
//! [`ResumeQueue`] with the resume brief of the work that was interrupted, and about two
//! minutes after the reset the lab resumes it in park order.
//!
//! The meter is persisted in `<dir>/usage.json` and the resume queue in `<dir>/resume.json`.
//! Each file has a schema version, uses a temporary file and a rename for every save, and is
//! guarded by an in-process mutex and a cross-process `flock`, so two instances on one directory
//! cannot lose each other's writes. A corrupt file or an unknown schema version is an error
//! rather than an empty state.

use std::{
    collections::BTreeSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    os::fd::AsRawFd,
    path::{Component, Path, PathBuf},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use jgf_core::{ModelError, ProjectId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use thiserror::Error;

/// Used percentage from which concurrency is reduced (soft threshold).
pub const SOFT_PERCENT: u64 = 80;

/// Used percentage from which no new worker starts (hard threshold).
pub const HARD_PERCENT: u64 = 95;

/// Seconds after the reset at which a parked piece resumes.
pub const RESUME_DELAY_SECS: u64 = 120;

/// Age in seconds after which a reading with the window still open is stale.
pub const STALE_SECS: u64 = 900;

/// Maximum length of a provider name.
const MAX_PROVIDER_LEN: usize = 40;

/// Maximum length of a resume task reference.
const MAX_TASK_LEN: usize = 256;

/// File name of the persisted meter inside its directory.
const USAGE_FILE: &str = "usage.json";

/// File name of the meter's cross-process lock.
const USAGE_LOCK_FILE: &str = "usage.lock";

/// File name of the persisted resume queue inside its directory.
const RESUME_FILE: &str = "resume.json";

/// File name of the resume queue's cross-process lock.
const RESUME_LOCK_FILE: &str = "resume.lock";

/// Schema version written with every usage and resume file.
const SCHEMA_VERSION: u32 = 1;

/// Seconds of one day, used to format a resume time as UTC `HH:MM`.
const SECONDS_PER_DAY: u64 = 86_400;

/// Name of a model provider whose usage window is metered.
///
/// A provider has 1 to 40 characters and only lowercase ASCII letters, digits, `-` and `_`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Provider(String);

impl Provider {
    /// Parses and validates a provider name.
    pub fn parse(raw: &str) -> Result<Self, UsageError> {
        if !provider_is_valid(raw) {
            return Err(UsageError::InvalidProvider {
                provider: raw.to_string(),
            });
        }
        Ok(Self(raw.to_string()))
    }

    /// The Claude provider, whose usage window the lab meters by default.
    pub fn claude() -> Self {
        Self("claude".to_string())
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Provider {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Provider {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

fn provider_is_valid(raw: &str) -> bool {
    (1..=MAX_PROVIDER_LEN).contains(&raw.len())
        && raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// One usage window of a provider: what it spent against its limit, and when it resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// Amount already used in this window.
    pub used: u64,
    /// Window limit; it is always greater than zero.
    pub limit: u64,
    /// Unix seconds at which the window resets.
    pub resets_at: u64,
}

impl Window {
    /// Builds a usage window, refusing a zero limit.
    pub fn new(used: u64, limit: u64, resets_at: u64) -> Result<Self, UsageError> {
        if limit == 0 {
            return Err(UsageError::InvalidWindow { used });
        }
        Ok(Self {
            used,
            limit,
            resets_at,
        })
    }

    /// Used percentage as an integer, from 0 upwards.
    ///
    /// It is `used * 100 / limit` with a saturating multiplication, so it never uses floats and
    /// may exceed 100 when a window is over its limit.
    pub fn percent(&self) -> u64 {
        self.used.saturating_mul(100) / self.limit
    }
}

/// How a recorded reading relates to the current window at a given time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingState {
    /// The window is open and the reading is fresh.
    Fresh,
    /// The window reset before `now`; the reading no longer applies.
    Expired,
    /// The window is still open but the reading is older than [`STALE_SECS`].
    Stale,
}

/// The latest window recorded for a provider, and when it was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    /// Recorded usage window.
    pub window: Window,
    /// Unix seconds at which the window was recorded.
    pub updated_at: u64,
}

impl Reading {
    /// Classifies the reading against `now`.
    ///
    /// An expired window wins over a stale reading: a reading taken before the reset says
    /// nothing about the current window.
    pub fn freshness(&self, now: u64) -> ReadingState {
        if self.window.resets_at <= now {
            ReadingState::Expired
        } else if now.saturating_sub(self.updated_at) > STALE_SECS {
            ReadingState::Stale
        } else {
            ReadingState::Fresh
        }
    }

    /// Used percentage of the current window at `now`.
    ///
    /// An expired window is treated as empty, so it reads 0 until a new reading arrives.
    pub fn percent(&self, now: u64) -> u64 {
        match self.freshness(now) {
            ReadingState::Expired => 0,
            ReadingState::Fresh | ReadingState::Stale => self.window.percent(),
        }
    }
}

/// How much a provider's usage limits concurrency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// No usage throttling.
    Normal,
    /// Concurrency is reduced and no new reviewer starts.
    Soft,
    /// No new worker or reviewer starts until the window resets.
    Hard,
}

/// One usage decision for a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throttle {
    /// Usage level this decision applies.
    pub level: Level,
    /// Maximum coders that may run.
    pub max_coders: usize,
    /// Whether a new worker may start.
    pub allow_new_worker: bool,
    /// Whether a new reviewer may start.
    pub allow_new_reviewer: bool,
    /// Unix seconds after which parked work resumes, on [`Level::Hard`].
    pub resume_at: Option<u64>,
}

impl Throttle {
    fn normal(max_coders: usize) -> Self {
        Self {
            level: Level::Normal,
            max_coders,
            allow_new_worker: true,
            allow_new_reviewer: true,
            resume_at: None,
        }
    }

    fn soft(max_coders: usize) -> Self {
        Self {
            level: Level::Soft,
            max_coders,
            allow_new_worker: true,
            allow_new_reviewer: false,
            resume_at: None,
        }
    }

    fn hard(resets_at: u64) -> Self {
        Self {
            level: Level::Hard,
            max_coders: 0,
            allow_new_worker: false,
            allow_new_reviewer: false,
            resume_at: Some(resets_at.saturating_add(RESUME_DELAY_SECS)),
        }
    }
}

/// The usage policy: turns the latest reading of a provider into a [`Throttle`].
pub struct Policy;

impl Policy {
    /// Decides how `reading` limits concurrency at `now`, for a current coder cap.
    ///
    /// No reading, a fresh reading below [`SOFT_PERCENT`] and an expired window are normal. At
    /// [`SOFT_PERCENT`] or above the cap is halved (rounded down, at least 1) and reviewers
    /// stop. At [`HARD_PERCENT`] or above, and for a stale reading whose window is still open,
    /// no new worker or reviewer starts and work resumes [`RESUME_DELAY_SECS`] after the reset.
    pub fn throttle(reading: Option<&Reading>, current_cap: usize, now: u64) -> Throttle {
        let Some(reading) = reading else {
            return Throttle::normal(current_cap);
        };
        match reading.freshness(now) {
            ReadingState::Expired => Throttle::normal(current_cap),
            ReadingState::Stale => Throttle::hard(reading.window.resets_at),
            ReadingState::Fresh => {
                let percent = reading.window.percent();
                if percent >= HARD_PERCENT {
                    Throttle::hard(reading.window.resets_at)
                } else if percent >= SOFT_PERCENT {
                    Throttle::soft((current_cap / 2).max(1))
                } else {
                    Throttle::normal(current_cap)
                }
            }
        }
    }
}

/// What the lab as a whole is doing under a throttle decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabState {
    /// Work is running normally.
    Running,
    /// Concurrency is reduced but work continues.
    Throttled,
    /// No new work starts until the given unix time.
    PausedUntil(u64),
    /// Nothing runs and nothing waits.
    Idle,
}

/// Summarizes the lab under `throttle` for the card.
///
/// A hard level pauses the lab until it resumes; a soft level keeps it throttled. With a normal
/// level it is idle only when no worker runs and no project waits.
pub fn lab_state(
    throttle: &Throttle,
    running_workers: usize,
    queued_projects: usize,
    now: u64,
) -> LabState {
    match throttle.level {
        Level::Hard => LabState::PausedUntil(throttle.resume_at.unwrap_or(now)),
        Level::Soft => LabState::Throttled,
        Level::Normal if running_workers == 0 && queued_projects == 0 => LabState::Idle,
        Level::Normal => LabState::Running,
    }
}

impl fmt::Display for LabState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("Running"),
            Self::Throttled => f.write_str("Throttled"),
            Self::PausedUntil(at) => write!(f, "Paused until {}", utc_clock(*at)),
            Self::Idle => f.write_str("Idle"),
        }
    }
}

/// Formats unix seconds as `HH:MM` in UTC.
fn utc_clock(at: u64) -> String {
    let seconds = at % SECONDS_PER_DAY;
    format!("{:02}:{:02}", seconds / 3_600, (seconds % 3_600) / 60)
}

/// Errors raised by the usage meter, its policy and the resume queue.
#[derive(Debug, Error)]
pub enum UsageError {
    /// Reading or writing meter state failed.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// A provider name does not follow the provider name rules.
    #[error(
        "invalid provider name {provider:?}; expected 1 to {MAX_PROVIDER_LEN} characters of a-z, 0-9, '-' or '_'"
    )]
    InvalidProvider {
        /// Rejected provider name.
        provider: String,
    },

    /// A usage window has a zero limit.
    #[error("invalid usage window for {used} used units; the limit must be greater than zero")]
    InvalidWindow {
        /// Used amount of the rejected window.
        used: u64,
    },

    /// A resume task reference is empty or too long.
    #[error("invalid resume task reference {task:?}")]
    InvalidTask {
        /// Rejected task reference.
        task: String,
    },

    /// A resume brief path is relative, empty or contains `..`.
    #[error("invalid resume brief path {}; it must be absolute and contain no `..`", path.display())]
    InvalidBriefPath {
        /// Rejected brief path.
        path: PathBuf,
    },

    /// A persisted usage or resume file is not valid JSON for its state.
    #[error("invalid usage state at {}: {source}", path.display())]
    InvalidState {
        /// Path of the rejected file.
        path: PathBuf,
        /// JSON parser error.
        #[source]
        source: serde_json::Error,
    },

    /// A persisted usage or resume file uses a schema version this build does not know.
    #[error("unsupported usage schema version {version} at {}", path.display())]
    UnsupportedVersion {
        /// Path of the rejected file.
        path: PathBuf,
        /// Version found in the file.
        version: u32,
    },

    /// A persisted usage or resume file is valid JSON but violates its invariants.
    #[error("corrupt usage state at {}: {detail}", path.display())]
    CorruptState {
        /// Path of the rejected file.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },
}

impl From<serde_json::Error> for UsageError {
    fn from(source: serde_json::Error) -> Self {
        Self::Model(ModelError::Json(source))
    }
}

/// One persisted reading, keyed by provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReadingRecord {
    provider: Provider,
    window: Window,
    updated_at: u64,
}

/// In-memory state persisted in `<dir>/usage.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UsageState {
    readings: Vec<ReadingRecord>,
}

impl UsageState {
    /// Refuses a persisted state that cannot be produced by this meter.
    fn validate(&self, path: &Path) -> Result<(), UsageError> {
        let corrupt = |detail: String| UsageError::CorruptState {
            path: path.to_path_buf(),
            detail,
        };
        let mut providers = BTreeSet::new();
        for record in &self.readings {
            if !providers.insert(record.provider.as_str()) {
                return Err(corrupt(format!(
                    "usage reading for provider {} is stored twice",
                    record.provider
                )));
            }
            if record.window.limit == 0 {
                return Err(corrupt(format!(
                    "usage reading for provider {} has a zero limit",
                    record.provider
                )));
            }
        }
        Ok(())
    }
}

/// Borrowed on-disk form of the meter, including its schema version.
#[derive(Debug, Serialize)]
struct UsageFileRef<'a> {
    version: u32,
    #[serde(flatten)]
    state: &'a UsageState,
}

/// First field read from a usage or resume file, before the rest of the state is parsed.
#[derive(Debug, Deserialize)]
struct VersionHeader {
    version: u32,
}

/// The usage meter, persisted as `<dir>/usage.json`.
///
/// A meter only records windows and decides; the lab runtime reads a window and asks the policy.
#[derive(Debug)]
pub struct UsageMeter {
    dir: PathBuf,
    path: PathBuf,
    lock_path: PathBuf,
    state: Mutex<()>,
}

impl UsageMeter {
    /// Opens the meter under `dir`, creating an empty one when the file is absent.
    ///
    /// A file that exists but is corrupt, or whose schema version is unknown, is an error.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, UsageError> {
        let dir = dir.into();
        let path = dir.join(USAGE_FILE);
        let lock_path = dir.join(USAGE_LOCK_FILE);
        read_usage(&path)?;
        Ok(Self {
            dir,
            path,
            lock_path,
            state: Mutex::new(()),
        })
    }

    /// Directory that holds `usage.json`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the persisted meter file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records `window` as the latest reading of `provider`, taken at `now`.
    pub fn record(&self, provider: &Provider, window: Window, now: u64) -> Result<(), UsageError> {
        self.mutate(|state| {
            if let Some(record) = state
                .readings
                .iter_mut()
                .find(|record| &record.provider == provider)
            {
                record.window = window;
                record.updated_at = now;
            } else {
                state.readings.push(ReadingRecord {
                    provider: provider.clone(),
                    window,
                    updated_at: now,
                });
            }
            Ok(())
        })
    }

    /// Latest reading recorded for `provider`, when one exists.
    pub fn get(&self, provider: &Provider) -> Result<Option<Reading>, UsageError> {
        Ok(self
            .read()?
            .readings
            .iter()
            .find(|record| &record.provider == provider)
            .map(|record| Reading {
                window: record.window,
                updated_at: record.updated_at,
            }))
    }

    /// Decides how `provider` limits concurrency at `now`, for a current coder cap.
    pub fn throttle(
        &self,
        provider: &Provider,
        current_cap: usize,
        now: u64,
    ) -> Result<Throttle, UsageError> {
        let reading = self.get(provider)?;
        Ok(Policy::throttle(reading.as_ref(), current_cap, now))
    }

    /// Re-reads the state under the exclusive lock, applies `change`, then saves the result.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut UsageState) -> Result<T, UsageError>,
    ) -> Result<T, UsageError> {
        let _state = self.lock();
        let lock_file = open_lock_file(&self.dir, &self.lock_path)?;
        let _lock = FileLock::exclusive(&lock_file, &self.lock_path)?;
        let mut state = read_usage(&self.path)?;
        let value = change(&mut state)?;
        self.persist(&state)?;
        Ok(value)
    }

    /// Reads the latest persisted state under a shared lock.
    fn read(&self) -> Result<UsageState, UsageError> {
        let _state = self.lock();
        let lock_file = open_lock_file(&self.dir, &self.lock_path)?;
        let _lock = FileLock::shared(&lock_file, &self.lock_path)?;
        read_usage(&self.path)
    }

    /// Saves `state` with an atomic replacement of `<dir>/usage.json`.
    fn persist(&self, state: &UsageState) -> Result<(), UsageError> {
        let bytes = serde_json::to_vec_pretty(&UsageFileRef {
            version: SCHEMA_VERSION,
            state,
        })?;
        write_atomic(&self.path, &bytes).map_err(UsageError::from)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One piece of work stopped by the usage limit and waiting for its resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parked {
    /// Project the interrupted piece belongs to.
    pub project: ProjectId,
    /// Task reference or key of the interrupted piece.
    pub task: String,
    /// Absolute path of the saved resume brief.
    pub brief_path: PathBuf,
    /// Unix seconds at which the usage window resets.
    pub resets_at: u64,
}

impl Parked {
    /// Unix seconds from which this piece may resume.
    pub fn due_at(&self) -> u64 {
        self.resets_at.saturating_add(RESUME_DELAY_SECS)
    }
}

/// In-memory state persisted in `<dir>/resume.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ResumeState {
    entries: Vec<Parked>,
}

impl ResumeState {
    /// Refuses a persisted state that cannot be produced by this queue.
    fn validate(&self, path: &Path) -> Result<(), UsageError> {
        let corrupt = |detail: String| UsageError::CorruptState {
            path: path.to_path_buf(),
            detail,
        };
        let mut keys = BTreeSet::new();
        for entry in &self.entries {
            if !task_is_valid(&entry.task) {
                return Err(corrupt(format!(
                    "invalid resume task reference {:?}",
                    entry.task
                )));
            }
            if !brief_path_is_valid(&entry.brief_path) {
                return Err(corrupt(format!(
                    "invalid resume brief path {}",
                    entry.brief_path.display()
                )));
            }
            if !keys.insert((entry.project.as_str(), entry.task.as_str())) {
                return Err(corrupt(format!(
                    "resume entry for task {:?} in project {} is stored twice",
                    entry.task, entry.project
                )));
            }
        }
        Ok(())
    }
}

/// Borrowed on-disk form of the resume queue, including its schema version.
#[derive(Debug, Serialize)]
struct ResumeFileRef<'a> {
    version: u32,
    #[serde(flatten)]
    state: &'a ResumeState,
}

/// The queue of pieces stopped by the usage limit, persisted as `<dir>/resume.json`.
#[derive(Debug)]
pub struct ResumeQueue {
    dir: PathBuf,
    path: PathBuf,
    lock_path: PathBuf,
    state: Mutex<()>,
}

impl ResumeQueue {
    /// Opens the queue under `dir`, creating an empty one when the file is absent.
    ///
    /// A file that exists but is corrupt, or whose schema version is unknown, is an error.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, UsageError> {
        let dir = dir.into();
        let path = dir.join(RESUME_FILE);
        let lock_path = dir.join(RESUME_LOCK_FILE);
        read_resume(&path)?;
        Ok(Self {
            dir,
            path,
            lock_path,
            state: Mutex::new(()),
        })
    }

    /// Directory that holds `resume.json`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the persisted resume file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parks a piece stopped by the limit, with the brief the lab resumes from.
    ///
    /// Parking the same project and task again replaces the entry in place, so it keeps its
    /// original position in the queue.
    pub fn park(
        &self,
        project: ProjectId,
        task: String,
        brief_path: PathBuf,
        resets_at: u64,
    ) -> Result<Parked, UsageError> {
        let parked = Parked {
            project,
            task,
            brief_path,
            resets_at,
        };
        validate_parked(&parked)?;
        self.mutate(move |state| {
            if let Some(entry) = state
                .entries
                .iter_mut()
                .find(|entry| entry.project == parked.project && entry.task == parked.task)
            {
                *entry = parked.clone();
            } else {
                state.entries.push(parked.clone());
            }
            Ok(parked)
        })
    }

    /// Entries whose resume delay has elapsed at `now`, in park order.
    pub fn due(&self, now: u64) -> Result<Vec<Parked>, UsageError> {
        Ok(self
            .read()?
            .entries
            .into_iter()
            .filter(|entry| entry.due_at() <= now)
            .collect())
    }

    /// Removes and returns one entry after it resumed.
    pub fn take(&self, project: &ProjectId, task: &str) -> Result<Option<Parked>, UsageError> {
        self.mutate(|state| {
            let Some(index) = state
                .entries
                .iter()
                .position(|entry| &entry.project == project && entry.task == task)
            else {
                return Ok(None);
            };
            Ok(Some(state.entries.remove(index)))
        })
    }

    /// Every parked entry, in park order.
    pub fn pending(&self) -> Result<Vec<Parked>, UsageError> {
        Ok(self.read()?.entries)
    }

    /// Re-reads the state under the exclusive lock, applies `change`, then saves the result.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut ResumeState) -> Result<T, UsageError>,
    ) -> Result<T, UsageError> {
        let _state = self.lock();
        let lock_file = open_lock_file(&self.dir, &self.lock_path)?;
        let _lock = FileLock::exclusive(&lock_file, &self.lock_path)?;
        let mut state = read_resume(&self.path)?;
        let value = change(&mut state)?;
        self.persist(&state)?;
        Ok(value)
    }

    /// Reads the latest persisted state under a shared lock.
    fn read(&self) -> Result<ResumeState, UsageError> {
        let _state = self.lock();
        let lock_file = open_lock_file(&self.dir, &self.lock_path)?;
        let _lock = FileLock::shared(&lock_file, &self.lock_path)?;
        read_resume(&self.path)
    }

    /// Saves `state` with an atomic replacement of `<dir>/resume.json`.
    fn persist(&self, state: &ResumeState) -> Result<(), UsageError> {
        let bytes = serde_json::to_vec_pretty(&ResumeFileRef {
            version: SCHEMA_VERSION,
            state,
        })?;
        write_atomic(&self.path, &bytes).map_err(UsageError::from)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn validate_parked(entry: &Parked) -> Result<(), UsageError> {
    if !task_is_valid(&entry.task) {
        return Err(UsageError::InvalidTask {
            task: entry.task.clone(),
        });
    }
    if !brief_path_is_valid(&entry.brief_path) {
        return Err(UsageError::InvalidBriefPath {
            path: entry.brief_path.clone(),
        });
    }
    Ok(())
}

fn task_is_valid(task: &str) -> bool {
    !task.trim().is_empty() && task.len() <= MAX_TASK_LEN && !task.chars().any(char::is_control)
}

fn brief_path_is_valid(path: &Path) -> bool {
    path.is_absolute()
        && !path.as_os_str().is_empty()
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
}

/// Reads and validates the persisted meter at `path`, or a fresh one when it is absent.
fn read_usage(path: &Path) -> Result<UsageState, UsageError> {
    let Some(state) = read_versioned::<UsageState>(path)? else {
        return Ok(UsageState::default());
    };
    state.validate(path)?;
    Ok(state)
}

/// Reads and validates the persisted resume queue at `path`, or a fresh one when it is absent.
fn read_resume(path: &Path) -> Result<ResumeState, UsageError> {
    let Some(state) = read_versioned::<ResumeState>(path)? else {
        return Ok(ResumeState::default());
    };
    state.validate(path)?;
    Ok(state)
}

/// Reads a versioned persisted state, rejecting an unknown schema version.
fn read_versioned<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, UsageError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(path, source).into()),
    };
    let header = serde_json::from_slice::<VersionHeader>(&bytes).map_err(|source| {
        UsageError::InvalidState {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if header.version != SCHEMA_VERSION {
        return Err(UsageError::UnsupportedVersion {
            path: path.to_path_buf(),
            version: header.version,
        });
    }
    let value = serde_json::from_slice::<T>(&bytes).map_err(|source| UsageError::InvalidState {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(value))
}

/// Opens (creating when needed) the file used for a cross-process lock.
fn open_lock_file(dir: &Path, lock_path: &Path) -> Result<File, ModelError> {
    fs::create_dir_all(dir).map_err(|source| io_error(dir, source))?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|source| io_error(lock_path, source))
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
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        thread,
    };

    use super::*;

    /// Temporary meter directory removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("jgf-hub-usage-{}-{suffix}", std::process::id()));
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

    fn open_meter(root: &TempRoot) -> UsageMeter {
        UsageMeter::open(root.path()).expect("open meter")
    }

    fn open_queue(root: &TempRoot) -> ResumeQueue {
        ResumeQueue::open(root.path()).expect("open queue")
    }

    fn project(raw: &str) -> ProjectId {
        ProjectId::parse(raw).expect("valid project id")
    }

    fn provider(raw: &str) -> Provider {
        Provider::parse(raw).expect("valid provider")
    }

    fn brief(root: &TempRoot, name: &str) -> PathBuf {
        root.path().join(name)
    }

    fn reading(used: u64, limit: u64, resets_at: u64, updated_at: u64) -> Reading {
        Reading {
            window: Window::new(used, limit, resets_at).expect("valid window"),
            updated_at,
        }
    }

    #[test]
    fn percent_boundaries_are_integers_and_may_exceed_one_hundred() {
        let cases = [
            (0, 0),
            (79, 79),
            (80, 80),
            (94, 94),
            (95, 95),
            (100, 100),
            (101, 101),
            (250, 250),
        ];
        for (used, expected) in cases {
            let window = Window::new(used, 100, 1_000).expect("window");
            assert_eq!(window.percent(), expected, "used {used} of 100");
        }

        let cases = [
            (1, 3, 33),
            (799, 1_000, 79),
            (800, 1_000, 80),
            (950, 1_000, 95),
        ];
        for (used, limit, expected) in cases {
            let window = Window::new(used, limit, 1_000).expect("window");
            assert_eq!(window.percent(), expected, "used {used} of {limit}");
        }
    }

    #[test]
    fn zero_limit_is_rejected() {
        for used in [0, 1] {
            assert!(matches!(
                Window::new(used, 0, 1_000),
                Err(UsageError::InvalidWindow { used: rejected }) if rejected == used
            ));
        }
    }

    #[test]
    fn policy_thresholds_are_exact() {
        let resets_at = 10_000;
        let cases = [
            (0, Level::Normal),
            (79, Level::Normal),
            (80, Level::Soft),
            (94, Level::Soft),
            (95, Level::Hard),
            (100, Level::Hard),
            (250, Level::Hard),
        ];
        for (used, expected) in cases {
            let reading = reading(used, 100, resets_at, 0);
            let decision = Policy::throttle(Some(&reading), 4, 0);
            assert_eq!(decision.level, expected, "used {used} of 100");
        }
    }

    #[test]
    fn soft_level_halves_the_cap_and_stops_reviewers() {
        let reading = reading(80, 100, 10_000, 0);
        let cases = [(4, 2), (3, 1), (2, 1), (1, 1), (0, 1)];
        for (cap, expected) in cases {
            let decision = Policy::throttle(Some(&reading), cap, 0);
            assert_eq!(decision.level, Level::Soft);
            assert_eq!(decision.max_coders, expected, "cap {cap}");
            assert!(decision.allow_new_worker);
            assert!(!decision.allow_new_reviewer);
            assert_eq!(decision.resume_at, None);
        }
    }

    #[test]
    fn hard_level_stops_new_work_and_schedules_the_resume() {
        let resets_at = 10_000;
        let reading = reading(95, 100, resets_at, 0);
        let decision = Policy::throttle(Some(&reading), 4, 0);
        assert_eq!(decision.level, Level::Hard);
        assert_eq!(decision.max_coders, 0);
        assert!(!decision.allow_new_worker);
        assert!(!decision.allow_new_reviewer);
        assert_eq!(decision.resume_at, Some(resets_at + RESUME_DELAY_SECS));
    }

    #[test]
    fn no_reading_is_normal() {
        let decision = Policy::throttle(None, 4, 0);
        assert_eq!(decision, Throttle::normal(4));
    }

    #[test]
    fn expired_window_reads_as_zero_and_is_not_throttled() {
        let root = TempRoot::new();
        let meter = open_meter(&root);
        let claude = Provider::claude();
        meter
            .record(&claude, Window::new(100, 100, 1_000).expect("window"), 900)
            .expect("record");

        let reading = meter.get(&claude).expect("get").expect("recorded");
        assert_eq!(reading.percent(999), 100);
        assert_eq!(reading.freshness(999), ReadingState::Fresh);
        assert_eq!(reading.percent(1_000), 0);
        assert_eq!(reading.freshness(1_000), ReadingState::Expired);

        let decision = meter.throttle(&claude, 4, 1_000).expect("throttle");
        assert_eq!(decision, Throttle::normal(4));
    }

    #[test]
    fn stale_reading_fails_closed_with_the_hard_threshold() {
        let root = TempRoot::new();
        let meter = open_meter(&root);
        let claude = Provider::claude();
        let recorded_at = 1_000;
        let resets_at = 100_000;
        meter
            .record(
                &claude,
                Window::new(10, 100, resets_at).expect("window"),
                recorded_at,
            )
            .expect("record");

        let fresh = recorded_at + STALE_SECS;
        let stale = fresh + 1;
        let reading = meter.get(&claude).expect("get").expect("recorded");
        assert_eq!(reading.freshness(fresh), ReadingState::Fresh);
        assert_eq!(reading.freshness(stale), ReadingState::Stale);
        assert_eq!(
            meter.throttle(&claude, 4, fresh).expect("fresh"),
            Throttle::normal(4)
        );

        let decision = meter.throttle(&claude, 4, stale).expect("stale");
        assert_eq!(decision.level, Level::Hard);
        assert_eq!(decision.resume_at, Some(resets_at + RESUME_DELAY_SECS));
    }

    #[test]
    fn non_claude_provider_ignores_the_claude_window() {
        let root = TempRoot::new();
        let meter = open_meter(&root);
        let claude = Provider::claude();
        let openai = provider("openai");

        meter
            .record(&claude, Window::new(100, 100, 10_000).expect("window"), 0)
            .expect("record claude");
        assert_eq!(
            meter.throttle(&openai, 4, 0).expect("openai"),
            Throttle::normal(4)
        );

        meter
            .record(&openai, Window::new(80, 100, 10_000).expect("window"), 0)
            .expect("record openai");
        assert_eq!(
            meter.throttle(&openai, 4, 0).expect("openai").level,
            Level::Soft
        );
        assert_eq!(
            meter.throttle(&claude, 4, 0).expect("claude").level,
            Level::Hard
        );
    }

    #[test]
    fn provider_names_are_validated() {
        let longest = "a".repeat(MAX_PROVIDER_LEN);
        for raw in [
            "claude",
            "openai",
            "a",
            "a-1_2",
            "9-provider",
            longest.as_str(),
        ] {
            let parsed = Provider::parse(raw).expect("valid provider");
            assert_eq!(parsed.as_str(), raw);
        }
        assert_eq!(
            Provider::claude(),
            Provider::parse("claude").expect("claude")
        );

        let too_long = "a".repeat(MAX_PROVIDER_LEN + 1);
        let hostile = [
            String::new(),
            "Claude".to_string(),
            "claude!".to_string(),
            "a b".to_string(),
            "../x".to_string(),
            "a/b".to_string(),
            too_long,
        ];
        for raw in hostile {
            assert!(
                matches!(
                    Provider::parse(&raw),
                    Err(UsageError::InvalidProvider { .. })
                ),
                "accepted invalid provider {raw:?}"
            );
        }
    }

    #[test]
    fn lab_state_matrix_and_display() {
        let resets_at = 3_600 * 3 + 60 * 5;
        let hard = Policy::throttle(Some(&reading(95, 100, resets_at, 0)), 4, 0);
        let resume_at = resets_at + RESUME_DELAY_SECS;

        let cases = [
            (lab_state(&hard, 0, 0, 0), LabState::PausedUntil(resume_at)),
            (lab_state(&hard, 4, 2, 0), LabState::PausedUntil(resume_at)),
        ];
        for (state, expected) in cases {
            assert_eq!(state, expected);
        }
        assert_eq!(lab_state(&hard, 0, 0, 0).to_string(), "Paused until 03:07");

        let soft = Policy::throttle(Some(&reading(80, 100, 10_000, 0)), 4, 0);
        assert_eq!(lab_state(&soft, 0, 0, 0), LabState::Throttled);
        assert_eq!(lab_state(&soft, 3, 2, 0), LabState::Throttled);

        let normal = Policy::throttle(None, 4, 0);
        assert_eq!(lab_state(&normal, 0, 0, 0), LabState::Idle);
        assert_eq!(lab_state(&normal, 1, 0, 0), LabState::Running);
        assert_eq!(lab_state(&normal, 0, 1, 0), LabState::Running);

        assert_eq!(LabState::Running.to_string(), "Running");
        assert_eq!(LabState::Throttled.to_string(), "Throttled");
        assert_eq!(LabState::Idle.to_string(), "Idle");
        assert_eq!(LabState::PausedUntil(0).to_string(), "Paused until 00:00");
        assert_eq!(
            LabState::PausedUntil(SECONDS_PER_DAY - 60).to_string(),
            "Paused until 23:59"
        );
    }

    #[test]
    fn resume_queue_is_fifo_and_waits_for_the_delay() {
        let root = TempRoot::new();
        let queue = open_queue(&root);
        for (name, task, resets_at) in [
            ("alpha", "ALPHA-1", 1_000),
            ("beta", "BETA-1", 1_100),
            ("gamma", "GAMMA-1", 1_200),
        ] {
            queue
                .park(
                    project(name),
                    task.to_string(),
                    brief(&root, task),
                    resets_at,
                )
                .expect("park");
        }

        assert!(
            queue.due(1_119).expect("due").is_empty(),
            "nothing is due before the resume delay"
        );
        assert_eq!(
            tasks(queue.due(1_120).expect("due")),
            ["ALPHA-1"],
            "the first entry is due exactly at reset plus the delay"
        );
        assert_eq!(tasks(queue.due(1_220).expect("due")), ["ALPHA-1", "BETA-1"]);
        assert_eq!(
            tasks(queue.due(1_320).expect("due")),
            ["ALPHA-1", "BETA-1", "GAMMA-1"]
        );

        let taken = queue
            .take(&project("alpha"), "ALPHA-1")
            .expect("take")
            .expect("parked");
        assert_eq!(taken.task, "ALPHA-1");
        assert!(
            queue
                .take(&project("alpha"), "ALPHA-1")
                .expect("take")
                .is_none()
        );
        assert_eq!(
            tasks(queue.pending().expect("pending")),
            ["BETA-1", "GAMMA-1"]
        );
        assert_eq!(tasks(queue.due(1_320).expect("due")), ["BETA-1", "GAMMA-1"]);
    }

    fn tasks(entries: Vec<Parked>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.task).collect()
    }

    #[test]
    fn reparking_keeps_the_original_position_and_replaces_the_entry() {
        let root = TempRoot::new();
        let queue = open_queue(&root);
        for (name, task) in [
            ("alpha", "ALPHA-1"),
            ("beta", "BETA-1"),
            ("gamma", "GAMMA-1"),
        ] {
            queue
                .park(project(name), task.to_string(), brief(&root, task), 1_000)
                .expect("park");
        }

        let replaced = queue
            .park(
                project("alpha"),
                "ALPHA-1".to_string(),
                brief(&root, "alpha-new"),
                5_000,
            )
            .expect("re-park");
        assert_eq!(replaced.resets_at, 5_000);

        let pending = queue.pending().expect("pending");
        assert_eq!(tasks(pending.clone()), ["ALPHA-1", "BETA-1", "GAMMA-1"]);
        assert_eq!(pending[0].resets_at, 5_000);
        assert_eq!(pending[0].brief_path, brief(&root, "alpha-new"));
        assert_eq!(
            tasks(queue.due(5_120).expect("due")),
            ["ALPHA-1", "BETA-1", "GAMMA-1"]
        );
    }

    #[test]
    fn bad_resume_entries_are_rejected() {
        let root = TempRoot::new();
        let queue = open_queue(&root);
        let too_long = "a".repeat(MAX_TASK_LEN + 1);
        let cases = [
            (String::new(), PathBuf::from("/briefs/a.json")),
            (too_long, PathBuf::from("/briefs/a.json")),
            ("TASK-1".to_string(), PathBuf::from("relative/brief.json")),
            ("TASK-1".to_string(), PathBuf::from("/briefs/../brief.json")),
            ("TASK-1".to_string(), PathBuf::new()),
        ];
        for (task, path) in cases {
            let result = queue.park(project("alpha"), task.clone(), path, 1_000);
            assert!(
                matches!(
                    result,
                    Err(UsageError::InvalidTask { .. } | UsageError::InvalidBriefPath { .. })
                ),
                "accepted a bad resume entry {task:?}"
            );
        }
        assert!(queue.pending().expect("pending").is_empty());
    }

    #[test]
    fn meter_and_queue_survive_a_reopen() {
        let root = TempRoot::new();
        {
            let meter = open_meter(&root);
            meter
                .record(
                    &Provider::claude(),
                    Window::new(80, 100, 10_000).expect("window"),
                    500,
                )
                .expect("record");
            let queue = open_queue(&root);
            queue
                .park(
                    project("alpha"),
                    "ALPHA-1".to_string(),
                    brief(&root, "alpha.json"),
                    10_000,
                )
                .expect("park");
            queue
                .park(
                    project("beta"),
                    "BETA-1".to_string(),
                    brief(&root, "beta.json"),
                    10_100,
                )
                .expect("park");
        }

        let meter = open_meter(&root);
        let reading = meter
            .get(&Provider::claude())
            .expect("get")
            .expect("recorded");
        assert_eq!(
            reading.window,
            Window::new(80, 100, 10_000).expect("window")
        );
        assert_eq!(reading.updated_at, 500);
        assert_eq!(
            meter
                .throttle(&Provider::claude(), 4, 600)
                .expect("throttle"),
            Throttle::soft(2)
        );

        let queue = open_queue(&root);
        assert_eq!(
            tasks(queue.pending().expect("pending")),
            ["ALPHA-1", "BETA-1"]
        );
        assert!(queue.due(10_119).expect("due").is_empty());
        assert_eq!(tasks(queue.due(10_120).expect("due")), ["ALPHA-1"]);
    }

    #[test]
    fn two_instances_serialize_concurrent_writes() {
        const THREADS: usize = 8;

        let root = TempRoot::new();
        let first_meter = Arc::new(open_meter(&root));
        let second_meter = Arc::new(open_meter(&root));
        let recorders: Vec<_> = (0..THREADS)
            .map(|index| {
                let meter = if index % 2 == 0 {
                    Arc::clone(&first_meter)
                } else {
                    Arc::clone(&second_meter)
                };
                thread::spawn(move || {
                    let provider = Provider::parse(&format!("provider-{index}")).expect("provider");
                    let window = Window::new(index as u64, 100, 10_000).expect("window");
                    meter.record(&provider, window, 100).expect("record");
                })
            })
            .collect();
        for recorder in recorders {
            recorder.join().expect("join recorder");
        }
        for index in 0..THREADS {
            let provider = Provider::parse(&format!("provider-{index}")).expect("provider");
            assert!(
                first_meter.get(&provider).expect("get").is_some(),
                "provider {index} was lost"
            );
        }

        let first_queue = Arc::new(open_queue(&root));
        let second_queue = Arc::new(open_queue(&root));
        let parkers: Vec<_> = (0..THREADS)
            .map(|index| {
                let queue = if index % 2 == 0 {
                    Arc::clone(&first_queue)
                } else {
                    Arc::clone(&second_queue)
                };
                thread::spawn(move || {
                    let project = ProjectId::parse(&format!("team-{index}")).expect("project");
                    queue
                        .park(
                            project,
                            format!("TASK-{index}"),
                            PathBuf::from(format!("/briefs/{index}.json")),
                            1_000,
                        )
                        .expect("park");
                })
            })
            .collect();
        for parker in parkers {
            parker.join().expect("join parker");
        }
        let pending = first_queue.pending().expect("pending");
        assert_eq!(pending.len(), THREADS, "every parked piece is kept once");
        assert_eq!(second_queue.pending().expect("pending"), pending);
    }

    #[test]
    fn corrupt_files_and_unknown_versions_are_errors() {
        let root = TempRoot::new();
        fs::write(root.path().join(USAGE_FILE), b"{not json").expect("write corrupt usage");
        assert!(matches!(
            UsageMeter::open(root.path()),
            Err(UsageError::InvalidState { .. })
        ));

        let root = TempRoot::new();
        let bad_version = serde_json::json!({ "version": SCHEMA_VERSION + 1 });
        fs::write(
            root.path().join(USAGE_FILE),
            serde_json::to_vec(&bad_version).expect("json"),
        )
        .expect("write bad version");
        assert!(matches!(
            UsageMeter::open(root.path()),
            Err(UsageError::UnsupportedVersion { version, .. }) if version == SCHEMA_VERSION + 1
        ));

        let root = TempRoot::new();
        let zero_limit = serde_json::json!({
            "version": SCHEMA_VERSION,
            "readings": [{
                "provider": "claude",
                "window": { "used": 1, "limit": 0, "resets_at": 5 },
                "updated_at": 0
            }]
        });
        fs::write(
            root.path().join(USAGE_FILE),
            serde_json::to_vec(&zero_limit).expect("json"),
        )
        .expect("write corrupt invariant");
        assert!(matches!(
            UsageMeter::open(root.path()),
            Err(UsageError::CorruptState { .. })
        ));

        let root = TempRoot::new();
        fs::write(root.path().join(RESUME_FILE), b"{not json").expect("write corrupt resume");
        assert!(matches!(
            ResumeQueue::open(root.path()),
            Err(UsageError::InvalidState { .. })
        ));

        let root = TempRoot::new();
        fs::write(
            root.path().join(RESUME_FILE),
            serde_json::to_vec(&bad_version).expect("json"),
        )
        .expect("write bad version");
        assert!(matches!(
            ResumeQueue::open(root.path()),
            Err(UsageError::UnsupportedVersion { version, .. }) if version == SCHEMA_VERSION + 1
        ));
    }
}
