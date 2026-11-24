//! Host-side watchdog of the Dark Factory lab: decides whether the Lab Coordinator is listening.
//!
//! The host timer wakes every [`WATCHDOG_INTERVAL_SECS`] and asks [`Watchdog::check`] what to do.
//! The Lab Coordinator beats while it listens; a heartbeat no older than [`HEARTBEAT_TTL_SECS`]
//! means the lab is healthy. Without a fresh heartbeat the watchdog starts the wake-up worker,
//! at most [`MAX_WAKE_ATTEMPTS`] times in a row and no more often than every
//! [`WAKE_COOLDOWN_SECS`], and then escalates until a heartbeat arrives.
//!
//! A usage-limit pause ([`LabState::PausedUntil`]) is not a fall. The watchdog waits for its
//! resume time instead of waking, but a pause whose resume time already passed is stuck and is
//! covered like any other fall. The lab stops only when no spec is left: then the decision is
//! [`Action::Done`] and nothing wakes.
//!
//! This module is pure state and decisions: no process is spawned, no network is used and no
//! clock is read. Every decision takes `now` in unix seconds from its caller. The host turns an
//! [`Action::Wake`] into a [`WakeRequest`], the plain data the wake-up worker runs on.
//!
//! State is persisted in `<dir>/watchdog.json`. Every save uses a temporary file and a rename,
//! and an in-process mutex plus a cross-process `flock` of `<dir>/watchdog.lock` serialize the
//! read-modify-write of the wake counter, so two instances on one directory cannot wake more
//! often than the limit together. A corrupt file or an unknown schema version is an error
//! rather than a fresh state.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use jgf_core::ModelError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::usage::LabState;

/// Seconds between two runs of the host-side watchdog.
pub const WATCHDOG_INTERVAL_SECS: u64 = 7_200;

/// Maximum age of a heartbeat from a listening Lab Coordinator.
pub const HEARTBEAT_TTL_SECS: u64 = 900;

/// Consecutive wake-up attempts allowed before the watchdog escalates.
pub const MAX_WAKE_ATTEMPTS: u8 = 3;

/// Minimum seconds between two wake-up attempts.
pub const WAKE_COOLDOWN_SECS: u64 = 600;

/// File name of the persisted watchdog inside its directory.
const STATE_FILE: &str = "watchdog.json";

/// File name of the watchdog's cross-process lock.
const LOCK_FILE: &str = "watchdog.lock";

/// Schema version written with every watchdog file.
const SCHEMA_VERSION: u32 = 1;

/// What the host does after a watchdog check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The Lab Coordinator beat recently; nothing to do.
    Healthy,
    /// No spec is left and nothing runs; the lab stops and never wakes.
    Done,
    /// Start the wake-up worker for the given consecutive attempt.
    Wake {
        /// Wake attempt number, from 1 to [`MAX_WAKE_ATTEMPTS`].
        attempt: u8,
    },
    /// Wait and check again at the given unix time.
    Wait {
        /// Unix seconds at which the next check may wake the lab.
        until: u64,
    },
    /// Every wake attempt failed; a human takes over until a heartbeat arrives.
    Escalate,
}

/// Why the watchdog wants to wake the Lab Coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeReason {
    /// The Lab Coordinator stopped beating.
    HeartbeatStale,
    /// The lab is still paused for the usage limit after its resume time.
    StuckPause,
}

impl WakeReason {
    /// Stable name used in a [`WakeRequest`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HeartbeatStale => "heartbeat_stale",
            Self::StuckPause => "stuck_pause",
        }
    }
}

/// The job handed to the wake-up worker so it restarts the Lab Coordinator.
///
/// It is plain data the host serializes for the worker: a low-effort `wake-up` role and a
/// [`WakeReason`] name, never free text from outside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeRequest {
    /// Role of the worker, always `wake-up`.
    pub role: String,
    /// Reasoning effort of the worker, always `low`.
    pub effort: String,
    /// Stable name of the wake reason.
    pub reason: String,
    /// Consecutive wake attempt, from 1 to [`MAX_WAKE_ATTEMPTS`].
    pub attempt: u8,
}

/// Builds the wake-up job for `attempt` and `reason`.
pub fn wake_request(attempt: u8, reason: WakeReason) -> WakeRequest {
    WakeRequest {
        role: "wake-up".to_string(),
        effort: "low".to_string(),
        reason: reason.as_str().to_string(),
        attempt,
    }
}

/// Everything a watchdog check needs to know about the lab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckInput {
    /// Unix seconds at which the check runs.
    pub now: u64,
    /// What the lab as a whole is doing.
    pub lab: LabState,
    /// Whether any spec is still left to work on.
    pub specs_left: bool,
}

/// Errors raised by the watchdog.
#[derive(Debug, Error)]
pub enum WatchdogError {
    /// Reading or writing watchdog state failed.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// A persisted watchdog file is not valid JSON for a watchdog.
    #[error("invalid watchdog state at {}: {source}", path.display())]
    InvalidState {
        /// Path of the rejected file.
        path: PathBuf,
        /// JSON parser error.
        #[source]
        source: serde_json::Error,
    },

    /// A persisted watchdog file uses a schema version this build does not know.
    #[error("unsupported watchdog schema version {version} at {}", path.display())]
    UnsupportedVersion {
        /// Path of the rejected file.
        path: PathBuf,
        /// Version found in the file.
        version: u32,
    },

    /// A persisted watchdog file is valid JSON but violates its invariants.
    #[error("corrupt watchdog state at {}: {detail}", path.display())]
    CorruptState {
        /// Path of the rejected file.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },
}

impl From<serde_json::Error> for WatchdogError {
    fn from(source: serde_json::Error) -> Self {
        Self::Model(ModelError::Json(source))
    }
}

/// In-memory state persisted in `<dir>/watchdog.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    /// Unix seconds of the latest heartbeat, absent when the coordinator never beat.
    last_beat: Option<u64>,
    /// Consecutive wake attempts since the last heartbeat.
    wake_attempts: u8,
    /// Unix seconds of the latest wake attempt.
    last_wake: Option<u64>,
}

impl State {
    /// Refuses a persisted state that cannot be produced by this watchdog.
    fn validate(&self, path: &Path) -> Result<(), WatchdogError> {
        if self.wake_attempts > MAX_WAKE_ATTEMPTS {
            return Err(WatchdogError::CorruptState {
                path: path.to_path_buf(),
                detail: format!(
                    "wake attempts {} is above the limit of {MAX_WAKE_ATTEMPTS}",
                    self.wake_attempts
                ),
            });
        }
        Ok(())
    }
}

/// Borrowed on-disk form of the watchdog, including its schema version.
#[derive(Debug, Serialize)]
struct StateFileRef<'a> {
    version: u32,
    #[serde(flatten)]
    state: &'a State,
}

/// First field read from a watchdog file, before the rest of the state is parsed.
#[derive(Debug, Deserialize)]
struct VersionHeader {
    version: u32,
}

/// The watchdog, persisted as `<dir>/watchdog.json`.
///
/// A watchdog only records heartbeats and decides. It never starts the wake-up worker or the
/// Lab Coordinator; the host does that after consuming its [`Action`].
#[derive(Debug)]
pub struct Watchdog {
    dir: PathBuf,
    path: PathBuf,
    lock_path: PathBuf,
    state: Mutex<()>,
}

impl Watchdog {
    /// Opens the watchdog under `dir`, creating an empty one when the file is absent.
    ///
    /// A file that exists but is corrupt, or whose schema version is unknown, is an error.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, WatchdogError> {
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

    /// Directory that holds `watchdog.json`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the persisted watchdog file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records a Lab Coordinator heartbeat at `now` and restarts the wake counter.
    ///
    /// A heartbeat older than the stored one is ignored entirely, so a clock going backwards
    /// neither rewinds the heartbeat nor clears the wake counter or an escalation.
    pub fn beat(&self, now: u64) -> Result<(), WatchdogError> {
        self.mutate(|state| {
            if state.last_beat.is_some_and(|stored| now < stored) {
                return Ok(());
            }
            state.last_beat = Some(now);
            state.wake_attempts = 0;
            state.last_wake = None;
            Ok(())
        })
    }

    /// Unix seconds of the latest heartbeat, when the coordinator ever beat.
    pub fn last_beat(&self) -> Result<Option<u64>, WatchdogError> {
        Ok(self.read()?.last_beat)
    }

    /// Consecutive wake attempts since the last heartbeat.
    pub fn wake_attempts(&self) -> Result<u8, WatchdogError> {
        Ok(self.read()?.wake_attempts)
    }

    /// Decides what the host does with the lab at `input.now`.
    ///
    /// No spec left and an idle lab is [`Action::Done`], even without a heartbeat. A heartbeat
    /// no older than [`HEARTBEAT_TTL_SECS`] is [`Action::Healthy`]. A stale or never-beaten lab
    /// waits for a usage-limit pause that is still running, and otherwise takes the wake path: a
    /// wake inside [`WAKE_COOLDOWN_SECS`] of the previous one is a wait, a counter below
    /// [`MAX_WAKE_ATTEMPTS`] is a [`Action::Wake`], and a counter at the limit is
    /// [`Action::Escalate`] on every later check until [`Watchdog::beat`] clears it.
    pub fn check(&self, input: CheckInput) -> Result<Action, WatchdogError> {
        if !input.specs_left && input.lab == LabState::Idle {
            return Ok(Action::Done);
        }

        let state = self.read()?;
        if heartbeat_is_fresh(&state, input.now) {
            return Ok(Action::Healthy);
        }

        // The wake counter moves under the exclusive lock, so the cooldown, the counter and the
        // state read cannot race another instance. Heartbeat and pause are re-checked there too.
        self.mutate(|state| {
            if heartbeat_is_fresh(state, input.now) {
                return Ok(Action::Healthy);
            }
            if let Some(until) = pause_resume(input.lab, input.now) {
                return Ok(Action::Wait { until });
            }
            if state.wake_attempts >= MAX_WAKE_ATTEMPTS {
                return Ok(Action::Escalate);
            }
            if let Some(last_wake) = state.last_wake {
                let ready = last_wake.saturating_add(WAKE_COOLDOWN_SECS);
                if input.now < ready {
                    return Ok(Action::Wait { until: ready });
                }
            }
            state.wake_attempts = state.wake_attempts.saturating_add(1);
            state.last_wake = Some(input.now);
            Ok(Action::Wake {
                attempt: state.wake_attempts,
            })
        })
    }

    /// Re-reads the state under the exclusive lock, applies `change`, then saves the result.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut State) -> Result<T, WatchdogError>,
    ) -> Result<T, WatchdogError> {
        let _state = self.lock();
        let lock_file = self.open_lock_file()?;
        let _lock = FileLock::exclusive(&lock_file, &self.lock_path)?;
        let mut state = read_state(&self.path)?;
        let value = change(&mut state)?;
        self.persist(&state)?;
        Ok(value)
    }

    /// Reads the latest persisted state under a shared lock.
    fn read(&self) -> Result<State, WatchdogError> {
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

    /// Saves `state` with an atomic replacement of `<dir>/watchdog.json`.
    fn persist(&self, state: &State) -> Result<(), WatchdogError> {
        let bytes = serde_json::to_vec_pretty(&StateFileRef {
            version: SCHEMA_VERSION,
            state,
        })?;
        write_atomic(&self.path, &bytes).map_err(WatchdogError::from)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether the Lab Coordinator beat within [`HEARTBEAT_TTL_SECS`] before `now`.
///
/// A beat stored in the future (the clock went backwards) counts as fresh rather than stale.
fn heartbeat_is_fresh(state: &State, now: u64) -> bool {
    state
        .last_beat
        .is_some_and(|last_beat| now.saturating_sub(last_beat) <= HEARTBEAT_TTL_SECS)
}

/// Resume time of a usage-limit pause that is still running at `now`, if any.
fn pause_resume(lab: LabState, now: u64) -> Option<u64> {
    match lab {
        LabState::PausedUntil(until) if now < until => Some(until),
        _ => None,
    }
}

/// Reads and validates the persisted watchdog at `path`, or a fresh one when it is absent.
fn read_state(path: &Path) -> Result<State, WatchdogError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(State::default()),
        Err(source) => return Err(io_error(path, source).into()),
    };
    let header = serde_json::from_slice::<VersionHeader>(&bytes).map_err(|source| {
        WatchdogError::InvalidState {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if header.version != SCHEMA_VERSION {
        return Err(WatchdogError::UnsupportedVersion {
            path: path.to_path_buf(),
            version: header.version,
        });
    }
    let state =
        serde_json::from_slice::<State>(&bytes).map_err(|source| WatchdogError::InvalidState {
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
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        thread,
    };

    use super::*;

    /// Temporary watchdog directory removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jgf-hub-watchdog-{}-{suffix}", std::process::id()));
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

    fn open(root: &TempRoot) -> Watchdog {
        Watchdog::open(root.path()).expect("open watchdog")
    }

    fn input(now: u64, lab: LabState, specs_left: bool) -> CheckInput {
        CheckInput {
            now,
            lab,
            specs_left,
        }
    }

    fn check(watchdog: &Watchdog, now: u64, lab: LabState, specs_left: bool) -> Action {
        watchdog
            .check(input(now, lab, specs_left))
            .expect("check watchdog")
    }

    #[test]
    fn heartbeat_freshness_is_table_driven() {
        let cases = [
            (None, 1_000, Action::Wake { attempt: 1 }),
            (Some(1_000), 1_000, Action::Healthy),
            (Some(1_000), 1_900, Action::Healthy),
            (Some(1_000), 1_901, Action::Wake { attempt: 1 }),
            (Some(1_000), 900, Action::Healthy),
            (Some(1_000), 0, Action::Healthy),
        ];
        for (beat_at, now, expected) in cases {
            let root = TempRoot::new();
            let watchdog = open(&root);
            if let Some(beat_at) = beat_at {
                watchdog.beat(beat_at).expect("beat");
            }
            assert_eq!(
                check(&watchdog, now, LabState::Running, true),
                expected,
                "beat {beat_at:?} checked at {now}"
            );
        }
    }

    #[test]
    fn done_stops_the_lab_and_never_wakes() {
        let cases = [
            (false, LabState::Idle, None, Action::Done),
            (false, LabState::Idle, Some(1_000), Action::Done),
            (true, LabState::Idle, Some(1_000), Action::Healthy),
            (true, LabState::Idle, None, Action::Wake { attempt: 1 }),
            (false, LabState::Running, None, Action::Wake { attempt: 1 }),
            (
                false,
                LabState::Throttled,
                None,
                Action::Wake { attempt: 1 },
            ),
            (
                false,
                LabState::PausedUntil(5_000),
                None,
                Action::Wait { until: 5_000 },
            ),
        ];
        for (specs_left, lab, beat_at, expected) in cases {
            let root = TempRoot::new();
            let watchdog = open(&root);
            if let Some(beat_at) = beat_at {
                watchdog.beat(beat_at).expect("beat");
            }
            assert_eq!(
                check(&watchdog, 1_000, lab, specs_left),
                expected,
                "specs_left {specs_left} lab {lab} beat {beat_at:?}"
            );
        }
    }

    #[test]
    fn a_running_pause_waits_but_a_stuck_pause_wakes() {
        let cases = [
            (
                None,
                1_000,
                LabState::PausedUntil(5_000),
                Action::Wait { until: 5_000 },
            ),
            (
                None,
                4_999,
                LabState::PausedUntil(5_000),
                Action::Wait { until: 5_000 },
            ),
            (
                None,
                5_000,
                LabState::PausedUntil(5_000),
                Action::Wake { attempt: 1 },
            ),
            (
                None,
                5_001,
                LabState::PausedUntil(5_000),
                Action::Wake { attempt: 1 },
            ),
            (
                Some(4_500),
                5_000,
                LabState::PausedUntil(5_000),
                Action::Healthy,
            ),
        ];
        for (beat_at, now, lab, expected) in cases {
            let root = TempRoot::new();
            let watchdog = open(&root);
            if let Some(beat_at) = beat_at {
                watchdog.beat(beat_at).expect("beat");
            }
            assert_eq!(
                check(&watchdog, now, lab, true),
                expected,
                "beat {beat_at:?} at {now} with lab {lab}"
            );
        }
    }

    #[test]
    fn cooldown_delays_the_next_wake_and_three_wakes_escalate_for_good() {
        let root = TempRoot::new();
        let watchdog = open(&root);
        let start = 10_000;

        assert_eq!(
            check(&watchdog, start, LabState::Running, true),
            Action::Wake { attempt: 1 }
        );
        assert_eq!(
            check(&watchdog, start + 1, LabState::Running, true),
            Action::Wait {
                until: start + WAKE_COOLDOWN_SECS
            }
        );
        assert_eq!(
            check(
                &watchdog,
                start + WAKE_COOLDOWN_SECS - 1,
                LabState::Running,
                true
            ),
            Action::Wait {
                until: start + WAKE_COOLDOWN_SECS
            }
        );
        assert_eq!(
            check(
                &watchdog,
                start + WAKE_COOLDOWN_SECS,
                LabState::Running,
                true
            ),
            Action::Wake { attempt: 2 }
        );
        assert_eq!(
            check(
                &watchdog,
                start + 2 * WAKE_COOLDOWN_SECS,
                LabState::Running,
                true
            ),
            Action::Wake { attempt: 3 }
        );
        assert_eq!(
            watchdog.wake_attempts().expect("attempts"),
            MAX_WAKE_ATTEMPTS
        );

        for now in [
            start + 2 * WAKE_COOLDOWN_SECS + 1,
            start + 3 * WAKE_COOLDOWN_SECS,
            start + 1_000_000,
        ] {
            assert_eq!(
                check(&watchdog, now, LabState::Running, true),
                Action::Escalate,
                "check at {now}"
            );
            assert_eq!(
                watchdog.wake_attempts().expect("attempts"),
                MAX_WAKE_ATTEMPTS,
                "the counter never exceeds the limit"
            );
        }
    }

    #[test]
    fn a_beat_clears_the_escalation_and_restarts_the_counter() {
        let root = TempRoot::new();
        let watchdog = open(&root);
        for attempt in 1..=MAX_WAKE_ATTEMPTS {
            let now = u64::from(attempt - 1) * WAKE_COOLDOWN_SECS;
            assert_eq!(
                check(&watchdog, now, LabState::Running, true),
                Action::Wake { attempt }
            );
        }
        assert_eq!(
            check(&watchdog, 3 * WAKE_COOLDOWN_SECS, LabState::Running, true),
            Action::Escalate
        );

        let beat_at = 10_000;
        watchdog.beat(beat_at).expect("beat");
        assert_eq!(watchdog.wake_attempts().expect("attempts"), 0);
        assert_eq!(
            check(&watchdog, beat_at, LabState::Running, true),
            Action::Healthy
        );
        assert_eq!(
            check(
                &watchdog,
                beat_at + HEARTBEAT_TTL_SECS,
                LabState::Running,
                true
            ),
            Action::Healthy
        );
        assert_eq!(
            check(
                &watchdog,
                beat_at + HEARTBEAT_TTL_SECS + 1,
                LabState::Running,
                true
            ),
            Action::Wake { attempt: 1 }
        );
    }

    #[test]
    fn an_older_beat_is_ignored() {
        let root = TempRoot::new();
        let watchdog = open(&root);

        watchdog.beat(1_000).expect("beat");
        watchdog.beat(500).expect("older beat");
        assert_eq!(watchdog.last_beat().expect("last beat"), Some(1_000));

        for attempt in 1..=MAX_WAKE_ATTEMPTS {
            let now = 1_000 + HEARTBEAT_TTL_SECS + 1 + u64::from(attempt - 1) * WAKE_COOLDOWN_SECS;
            assert_eq!(
                check(&watchdog, now, LabState::Running, true),
                Action::Wake { attempt }
            );
        }
        watchdog.beat(999).expect("older beat");
        assert_eq!(
            watchdog.wake_attempts().expect("attempts"),
            MAX_WAKE_ATTEMPTS,
            "an older beat does not clear the escalation"
        );
        assert_eq!(watchdog.last_beat().expect("last beat"), Some(1_000));

        watchdog.beat(1_000).expect("same beat");
        assert_eq!(
            watchdog.wake_attempts().expect("attempts"),
            0,
            "a beat at the stored time is accepted"
        );
    }

    #[test]
    fn overflow_inputs_are_saturating() {
        let root = TempRoot::new();
        let watchdog = open(&root);
        watchdog
            .beat(u64::MAX - HEARTBEAT_TTL_SECS)
            .expect("beat near the maximum");
        assert_eq!(
            check(&watchdog, u64::MAX, LabState::Running, true),
            Action::Healthy,
            "a beat exactly the TTL before the maximum is fresh"
        );

        let root = TempRoot::new();
        let watchdog = open(&root);
        assert_eq!(
            check(
                &watchdog,
                u64::MAX - 1,
                LabState::PausedUntil(u64::MAX),
                true
            ),
            Action::Wait { until: u64::MAX },
            "a pause ending one second later is still running"
        );
        assert_eq!(
            check(&watchdog, u64::MAX, LabState::PausedUntil(u64::MAX), true),
            Action::Wake { attempt: 1 },
            "a pause ending now is stuck"
        );

        let root = TempRoot::new();
        let watchdog = open(&root);
        assert_eq!(
            check(&watchdog, u64::MAX - 10, LabState::Running, true),
            Action::Wake { attempt: 1 }
        );
        assert_eq!(
            check(&watchdog, u64::MAX - 5, LabState::Running, true),
            Action::Wait { until: u64::MAX },
            "the cooldown addition saturates"
        );
        assert_eq!(
            check(&watchdog, u64::MAX, LabState::Running, true),
            Action::Wake { attempt: 2 }
        );
    }

    #[test]
    fn state_survives_a_reopen() {
        let root = TempRoot::new();
        {
            let watchdog = open(&root);
            watchdog.beat(5_000).expect("beat");
            assert_eq!(
                check(&watchdog, 6_000, LabState::Running, true),
                Action::Wake { attempt: 1 }
            );
        }

        let watchdog = open(&root);
        assert_eq!(watchdog.last_beat().expect("last beat"), Some(5_000));
        assert_eq!(watchdog.wake_attempts().expect("attempts"), 1);
        assert_eq!(
            check(&watchdog, 6_600, LabState::Running, true),
            Action::Wake { attempt: 2 }
        );
    }

    #[test]
    fn two_instances_never_exceed_the_wake_limit() {
        const THREADS: usize = 10;

        let root = TempRoot::new();
        let first = Arc::new(open(&root));
        let second = Arc::new(open(&root));
        let now = 10_000;
        let checks: Vec<_> = (0..THREADS)
            .map(|index| {
                let watchdog = if index % 2 == 0 {
                    Arc::clone(&first)
                } else {
                    Arc::clone(&second)
                };
                thread::spawn(move || check(&watchdog, now, LabState::Running, true))
            })
            .collect();

        let actions: Vec<Action> = checks
            .into_iter()
            .map(|handle| handle.join().expect("join checker"))
            .collect();
        let wakes = actions
            .iter()
            .filter(|action| matches!(action, Action::Wake { .. }))
            .count();
        assert_eq!(
            wakes, 1,
            "the cooldown admits exactly one wake, got {actions:?}"
        );
        assert_eq!(
            first.wake_attempts().expect("attempts"),
            1,
            "the counter is shared through the file"
        );
        assert_eq!(second.wake_attempts().expect("attempts"), 1);

        let mut wakes = 1;
        while wakes < MAX_WAKE_ATTEMPTS {
            let now = now + u64::from(wakes) * WAKE_COOLDOWN_SECS;
            assert_eq!(
                check(&first, now, LabState::Running, true),
                Action::Wake { attempt: wakes + 1 }
            );
            wakes += 1;
        }
        assert_eq!(
            check(&second, now + WAKE_COOLDOWN_SECS, LabState::Running, true),
            Action::Escalate
        );
    }

    #[test]
    fn wake_requests_are_fixed_data() {
        let cases = [
            (WakeReason::HeartbeatStale, "heartbeat_stale"),
            (WakeReason::StuckPause, "stuck_pause"),
        ];
        for (reason, name) in cases {
            let request = wake_request(MAX_WAKE_ATTEMPTS, reason);
            assert_eq!(request.role, "wake-up");
            assert_eq!(request.effort, "low");
            assert_eq!(request.reason, name);
            assert_eq!(request.attempt, MAX_WAKE_ATTEMPTS);

            let json = serde_json::to_value(&request).expect("serialize request");
            assert_eq!(json["role"], "wake-up");
            assert_eq!(json["reason"], name);
            let decoded: WakeRequest = serde_json::from_value(json).expect("deserialize request");
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn corrupt_state_and_unknown_version_are_errors() {
        let root = TempRoot::new();
        fs::write(root.path().join(STATE_FILE), b"{not json").expect("write corrupt state");
        assert!(matches!(
            Watchdog::open(root.path()),
            Err(WatchdogError::InvalidState { .. })
        ));

        let root = TempRoot::new();
        let bad_version = serde_json::json!({ "version": SCHEMA_VERSION + 1 });
        fs::write(
            root.path().join(STATE_FILE),
            serde_json::to_vec(&bad_version).expect("json"),
        )
        .expect("write bad version");
        assert!(matches!(
            Watchdog::open(root.path()),
            Err(WatchdogError::UnsupportedVersion { version, .. })
                if version == SCHEMA_VERSION + 1
        ));

        let root = TempRoot::new();
        let over_limit = serde_json::json!({
            "version": SCHEMA_VERSION,
            "last_beat": null,
            "wake_attempts": MAX_WAKE_ATTEMPTS + 1,
            "last_wake": null,
        });
        fs::write(
            root.path().join(STATE_FILE),
            serde_json::to_vec(&over_limit).expect("json"),
        )
        .expect("write over-limit state");
        assert!(matches!(
            Watchdog::open(root.path()),
            Err(WatchdogError::CorruptState { .. })
        ));
    }
}
