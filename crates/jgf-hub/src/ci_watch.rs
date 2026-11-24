//! CI watcher of the Dark Factory lab: reads CI results and decides the next step.
//!
//! The product rule is that coders and testers never run the whole test suite on every change.
//! Instead the Lab Coordinator reads the pull request checks through the broker, hands the
//! failure logs to a fresh coder, and allows at most [`MAX_FIX_CYCLES`] fix cycles per pull
//! request before escalating to the Lab Coordinator.
//!
//! The watcher is pure state and decisions. It never launches `gh`, reads logs or talks to
//! GitHub: the runtime runs the broker command, parses its output with [`parse_ci_runs`], and
//! calls [`CiWatch::decide`]. For the same reason this module has no audit hook yet; the lab
//! runtime writes CI events when it arrives.
//!
//! Everything is persisted in `<dir>/ci_watch.json`, keyed by project and pull request number.
//! An in-process mutex and a cross-process `flock` of `<dir>/ci_watch.lock` serialize
//! read-modify-write cycles, so two watchers on one directory cannot exceed the fix limit
//! together. Saves use a temporary file and a rename, and a corrupt file or an unknown schema
//! version is an error rather than an empty watcher.

use std::{
    collections::BTreeSet,
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
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum number of fix cycles granted for one pull request.
pub const MAX_FIX_CYCLES: u8 = 3;

/// File name of the persisted watcher inside its directory.
const STATE_FILE: &str = "ci_watch.json";

/// File name of the cross-process lock inside the watcher directory.
const LOCK_FILE: &str = "ci_watch.lock";

/// Schema version written with every watcher file.
const SCHEMA_VERSION: u32 = 1;

/// Outcome of the CI checks for one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiState {
    /// Checks are still queued or running.
    Pending,
    /// Every check completed successfully and the reviewer passed the pull request.
    Green,
    /// At least one check failed and needs a fix cycle.
    Failed,
}

/// What the lab does next with a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Keep watching; nothing is ready yet.
    Wait,
    /// CI is green and the reviewer passed the pull request.
    Done,
    /// Hand the failure logs to a fresh coder.
    FixCycle {
        /// Fix cycle number, from 1 to [`MAX_FIX_CYCLES`].
        cycle: u8,
        /// Absolute path of the failure log for the coder.
        log_path: PathBuf,
    },
    /// The fix cycles are exhausted; the Lab Coordinator takes over.
    Escalate,
}

/// Errors raised by the CI watcher.
#[derive(Debug, Error)]
pub enum CiError {
    /// Reading or writing watcher state failed.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// CI output does not match the expected `gh run list` JSON shape or values.
    #[error("unrecognized CI run data: {detail}")]
    Unrecognized {
        /// What is wrong.
        detail: String,
    },

    /// The expected commit SHA is not 40 hexadecimal characters.
    #[error("invalid commit SHA {sha:?}; expected 40 hexadecimal characters")]
    InvalidSha {
        /// Rejected commit SHA.
        sha: String,
    },

    /// A failed CI run was decided without a log path.
    #[error("a failed CI run needs the path of its failure log")]
    MissingLogPath,

    /// A failure log path is relative, empty or contains `..`.
    #[error("invalid CI log path {}; it must be absolute and contain no `..`", path.display())]
    InvalidLogPath {
        /// Rejected log path.
        path: PathBuf,
    },

    /// A persisted watcher file is not valid JSON for a watcher.
    #[error("invalid CI watch file at {}: {source}", path.display())]
    InvalidState {
        /// Path of the rejected file.
        path: PathBuf,
        /// JSON parser error.
        #[source]
        source: serde_json::Error,
    },

    /// A persisted watcher file uses a schema version this build does not know.
    #[error("unsupported CI watch schema version {version} at {}", path.display())]
    UnsupportedVersion {
        /// Path of the rejected file.
        path: PathBuf,
        /// Version found in the file.
        version: u32,
    },

    /// A persisted watcher file is valid JSON but violates watcher invariants.
    #[error("corrupt CI watch state at {}: {detail}", path.display())]
    CorruptState {
        /// Path of the rejected file.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },
}

impl From<serde_json::Error> for CiError {
    fn from(source: serde_json::Error) -> Self {
        Self::Model(ModelError::Json(source))
    }
}

/// One run of the `gh run list` JSON output.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    status: String,
    conclusion: serde_json::Value,
    #[serde(rename = "databaseId")]
    _database_id: u64,
    #[serde(rename = "headSha")]
    head_sha: String,
}

/// One persisted fix-cycle counter, keyed by project and pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CycleRecord {
    project: ProjectId,
    pr: u64,
    cycles: u8,
}

/// In-memory state persisted in `<dir>/ci_watch.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    records: Vec<CycleRecord>,
}

impl State {
    /// Refuses a persisted state that cannot be produced by this watcher.
    fn validate(&self, path: &Path) -> Result<(), CiError> {
        let corrupt = |detail: String| CiError::CorruptState {
            path: path.to_path_buf(),
            detail,
        };
        let mut keys = BTreeSet::new();
        for record in &self.records {
            if !keys.insert((record.project.as_str(), record.pr)) {
                return Err(corrupt(format!(
                    "CI watch record for project {} and PR {} is stored twice",
                    record.project, record.pr
                )));
            }
            if record.cycles > MAX_FIX_CYCLES {
                return Err(corrupt(format!(
                    "CI watch record for project {} and PR {} has {} cycles, above the limit of {MAX_FIX_CYCLES}",
                    record.project, record.pr, record.cycles
                )));
            }
        }
        Ok(())
    }

    /// Fix cycles already granted to `project` and `pr`.
    fn count(&self, project: &ProjectId, pr: u64) -> u8 {
        self.records
            .iter()
            .find(|record| &record.project == project && record.pr == pr)
            .map_or(0, |record| record.cycles)
    }

    /// Mutable fix-cycle counter for `project` and `pr`, created at zero when absent.
    fn count_mut(&mut self, project: &ProjectId, pr: u64) -> &mut u8 {
        let index = self
            .records
            .iter()
            .position(|record| &record.project == project && record.pr == pr)
            .unwrap_or_else(|| {
                self.records.push(CycleRecord {
                    project: project.clone(),
                    pr,
                    cycles: 0,
                });
                self.records.len() - 1
            });
        &mut self.records[index].cycles
    }
}

/// Borrowed on-disk form of the watcher, including its schema version.
#[derive(Debug, Serialize)]
struct StateFileRef<'a> {
    version: u32,
    #[serde(flatten)]
    state: &'a State,
}

/// First field read from a watcher file, before the rest of the state is parsed.
#[derive(Debug, Deserialize)]
struct VersionHeader {
    version: u32,
}

/// The CI watcher, persisted as `<dir>/ci_watch.json`.
///
/// A watcher only decides. It never runs `gh`, reads a log or talks to GitHub; the lab runtime
/// does that after consuming these decisions.
#[derive(Debug)]
pub struct CiWatch {
    dir: PathBuf,
    path: PathBuf,
    lock_path: PathBuf,
    state: Mutex<()>,
}

impl CiWatch {
    /// Opens the watcher under `dir`, creating an empty one when the file is absent.
    ///
    /// A file that exists but is corrupt, or whose schema version is unknown, is an error.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, CiError> {
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

    /// Directory that holds `ci_watch.json`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the persisted watcher file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Decides what the lab does next for `project` and `pr`.
    ///
    /// A failed result consumes one fix cycle. After [`MAX_FIX_CYCLES`] cycles, and on every
    /// later failure, the decision is [`Decision::Escalate`]. A green result does not reset the
    /// counter, so a pull request that was green and fails again continues counting.
    pub fn decide(
        &self,
        project: &ProjectId,
        pr: u64,
        state: CiState,
        review_passed: bool,
        log_path: Option<&Path>,
    ) -> Result<Decision, CiError> {
        match state {
            CiState::Pending => Ok(Decision::Wait),
            CiState::Green if review_passed => Ok(Decision::Done),
            CiState::Green => Ok(Decision::Wait),
            CiState::Failed => {
                let log_path = checked_log_path(log_path)?;
                self.mutate(|stored| {
                    let cycles = stored.count_mut(project, pr);
                    if *cycles >= MAX_FIX_CYCLES {
                        return Ok(Decision::Escalate);
                    }
                    *cycles += 1;
                    Ok(Decision::FixCycle {
                        cycle: *cycles,
                        log_path: log_path.clone(),
                    })
                })
            }
        }
    }

    /// Fix cycles already granted to `project` and `pr`.
    pub fn cycles(&self, project: &ProjectId, pr: u64) -> Result<u8, CiError> {
        Ok(self.read()?.count(project, pr))
    }

    /// Forgets `project` and `pr`, for a merged or closed pull request.
    ///
    /// Returns whether a counter was stored.
    pub fn forget(&self, project: &ProjectId, pr: u64) -> Result<bool, CiError> {
        self.mutate(|stored| {
            let before = stored.records.len();
            stored
                .records
                .retain(|record| &record.project != project || record.pr != pr);
            Ok(stored.records.len() != before)
        })
    }

    /// Re-reads the state under the exclusive lock, applies `change`, then saves the result.
    ///
    /// The in-process mutex and `<dir>/ci_watch.lock` are held while the file is read and
    /// replaced, so the cycle check and the increment happen under the same lock.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut State) -> Result<T, CiError>,
    ) -> Result<T, CiError> {
        let _state = self.lock();
        let lock_file = self.open_lock_file()?;
        let _lock = FileLock::exclusive(&lock_file, &self.lock_path)?;
        let mut state = read_state(&self.path)?;
        let value = change(&mut state)?;
        self.persist(&state)?;
        Ok(value)
    }

    /// Reads the latest persisted state under a shared lock.
    fn read(&self) -> Result<State, CiError> {
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

    /// Saves `state` with an atomic replacement of `<dir>/ci_watch.json`.
    fn persist(&self, state: &State) -> Result<(), CiError> {
        let bytes = serde_json::to_vec_pretty(&StateFileRef {
            version: SCHEMA_VERSION,
            state,
        })?;
        write_atomic(&self.path, &bytes).map_err(CiError::from)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Parses the output of `gh run list --json status,conclusion,databaseId,headSha`.
///
/// Only runs whose `headSha` equals `expected_sha` are considered. The parser fails closed:
/// malformed JSON, fields or values, and unknown statuses are [`CiError::Unrecognized`]. A run
/// that is not completed keeps the result pending while its status is one of `queued`,
/// `in_progress`, `waiting` or `pending`. Every completed run must conclude `success` for a green
/// result; `neutral`, `skipped` and the failure conclusions make the result failed.
pub fn parse_ci_runs(json: &str, expected_sha: &str) -> Result<CiState, CiError> {
    Ok(parse_ci_runs_with_presence(json, expected_sha)?.unwrap_or(CiState::Pending))
}

/// Parses CI runs and distinguishes no run for `expected_sha` from an existing pending run.
///
/// An empty list, or a list without a run for that SHA, returns `Ok(None)`. A matching run list
/// returns its state. Use [`parse_ci_runs`] when the legacy behavior of treating no run as pending
/// is desired.
pub fn parse_ci_runs_with_presence(
    json: &str,
    expected_sha: &str,
) -> Result<Option<CiState>, CiError> {
    if !sha_is_valid(expected_sha) {
        return Err(CiError::InvalidSha {
            sha: expected_sha.to_string(),
        });
    }
    let runs = serde_json::from_str::<Vec<Run>>(json)
        .map_err(|source| unrecognized(source.to_string()))?;
    let matching: Vec<&Run> = runs
        .iter()
        .filter(|run| run.head_sha.eq_ignore_ascii_case(expected_sha))
        .collect();
    if matching.is_empty() {
        return Ok(None);
    }

    let mut pending = false;
    let mut failed = false;
    for run in matching {
        let conclusion = conclusion_text(&run.conclusion)?;
        match run.status.as_str() {
            "completed" => match conclusion {
                None => return Err(unrecognized("a completed run has no conclusion")),
                Some("") => return Err(unrecognized("a completed run has an empty conclusion")),
                Some("success") => {}
                Some(_) => failed = true,
            },
            "queued" | "in_progress" | "waiting" | "pending" => pending = true,
            other => return Err(unrecognized(format!("unknown run status {other:?}"))),
        }
    }

    if pending {
        Ok(Some(CiState::Pending))
    } else if failed {
        Ok(Some(CiState::Failed))
    } else {
        Ok(Some(CiState::Green))
    }
}

fn unrecognized(detail: impl Into<String>) -> CiError {
    CiError::Unrecognized {
        detail: detail.into(),
    }
}

fn sha_is_valid(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn conclusion_text(value: &serde_json::Value) -> Result<Option<&str>, CiError> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(text) => Ok(Some(text.as_str())),
        _ => Err(unrecognized("a run conclusion is not a string")),
    }
}

fn checked_log_path(log_path: Option<&Path>) -> Result<PathBuf, CiError> {
    let path = log_path.ok_or(CiError::MissingLogPath)?;
    let relative = !path.is_absolute();
    let empty = path.as_os_str().is_empty();
    let parent = path
        .components()
        .any(|component| component == Component::ParentDir);
    if relative || empty || parent {
        return Err(CiError::InvalidLogPath {
            path: path.to_path_buf(),
        });
    }
    Ok(path.to_path_buf())
}

/// Reads and validates the persisted watcher at `path`, or a fresh one when it is absent.
fn read_state(path: &Path) -> Result<State, CiError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(State::default()),
        Err(source) => return Err(io_error(path, source).into()),
    };
    let header = serde_json::from_slice::<VersionHeader>(&bytes).map_err(|source| {
        CiError::InvalidState {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if header.version != SCHEMA_VERSION {
        return Err(CiError::UnsupportedVersion {
            path: path.to_path_buf(),
            version: header.version,
        });
    }
    let state =
        serde_json::from_slice::<State>(&bytes).map_err(|source| CiError::InvalidState {
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

    /// Temporary watcher directory removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jgf-hub-ci-watch-{}-{suffix}", std::process::id()));
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

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn project(raw: &str) -> ProjectId {
        ProjectId::parse(raw).expect("valid project id")
    }

    fn open(root: &TempRoot) -> CiWatch {
        CiWatch::open(root.path()).expect("open watcher")
    }

    fn log_path() -> PathBuf {
        PathBuf::from("/tmp/ci-logs/run.log")
    }

    fn run(sha: &str, status: &str, conclusion: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "databaseId": 42,
            "headSha": sha,
            "status": status,
            "conclusion": conclusion,
        })
    }

    fn runs(items: &[serde_json::Value]) -> String {
        serde_json::to_string(items).expect("serialize runs")
    }

    fn parse(items: &[serde_json::Value]) -> Result<CiState, CiError> {
        parse_ci_runs(&runs(items), SHA)
    }

    #[test]
    fn pending_statuses_are_pending() {
        for status in ["queued", "in_progress", "waiting", "pending"] {
            assert_eq!(
                parse(&[run(SHA, status, serde_json::Value::String(String::new()))])
                    .expect("pending run"),
                CiState::Pending,
                "status {status}"
            );
        }
        assert_eq!(
            parse(&[run(SHA, "in_progress", serde_json::Value::Null)]).expect("null conclusion"),
            CiState::Pending
        );
    }

    #[test]
    fn completed_success_is_green() {
        assert_eq!(
            parse(&[run(
                SHA,
                "completed",
                serde_json::Value::String("success".to_string())
            )])
            .expect("green run"),
            CiState::Green
        );
        assert_eq!(
            parse(&[
                run(
                    SHA,
                    "completed",
                    serde_json::Value::String("success".to_string())
                ),
                run(
                    SHA,
                    "completed",
                    serde_json::Value::String("success".to_string())
                ),
            ])
            .expect("two green runs"),
            CiState::Green
        );
    }

    #[test]
    fn failure_conclusions_are_failed() {
        for conclusion in [
            "failure",
            "timed_out",
            "cancelled",
            "action_required",
            "startup_failure",
        ] {
            assert_eq!(
                parse(&[run(
                    SHA,
                    "completed",
                    serde_json::Value::String(conclusion.to_string())
                )])
                .expect("failed run"),
                CiState::Failed,
                "conclusion {conclusion}"
            );
        }
    }

    #[test]
    fn neutral_and_skipped_are_not_success() {
        for conclusion in ["neutral", "skipped"] {
            assert_eq!(
                parse(&[run(
                    SHA,
                    "completed",
                    serde_json::Value::String(conclusion.to_string())
                )])
                .expect("neutral or skipped"),
                CiState::Failed,
                "conclusion {conclusion}"
            );
        }
    }

    #[test]
    fn one_failure_among_successes_is_failed() {
        assert_eq!(
            parse(&[
                run(
                    SHA,
                    "completed",
                    serde_json::Value::String("success".to_string())
                ),
                run(
                    SHA,
                    "completed",
                    serde_json::Value::String("failure".to_string())
                ),
            ])
            .expect("mixed runs"),
            CiState::Failed
        );
    }

    #[test]
    fn a_running_run_keeps_a_mixed_result_pending() {
        assert_eq!(
            parse(&[
                run(
                    SHA,
                    "completed",
                    serde_json::Value::String("failure".to_string())
                ),
                run(SHA, "in_progress", serde_json::Value::Null),
            ])
            .expect("mixed running runs"),
            CiState::Pending
        );
    }

    #[test]
    fn garbage_output_is_unrecognized() {
        for json in ["{not json", "null", "{}", "\"text\""] {
            assert!(
                matches!(parse_ci_runs(json, SHA), Err(CiError::Unrecognized { .. })),
                "accepted {json:?}"
            );
        }
    }

    #[test]
    fn an_empty_run_list_is_pending() {
        assert_eq!(parse(&[]).expect("empty list"), CiState::Pending);
        assert_eq!(
            parse_ci_runs_with_presence("[]", SHA).expect("empty list"),
            None
        );
    }

    #[test]
    fn a_run_list_without_the_expected_sha_has_no_matching_run() {
        let json = runs(&[run(OTHER_SHA, "in_progress", serde_json::Value::Null)]);
        assert_eq!(
            parse_ci_runs_with_presence(&json, SHA).expect("unrelated run"),
            None
        );
        assert_eq!(
            parse_ci_runs(&json, SHA).expect("legacy behavior"),
            CiState::Pending
        );
    }

    #[test]
    fn runs_for_another_sha_are_pending() {
        assert_eq!(
            parse(&[run(
                OTHER_SHA,
                "completed",
                serde_json::Value::String("failure".to_string())
            )])
            .expect("other sha"),
            CiState::Pending
        );
    }

    #[test]
    fn a_missing_or_unknown_field_is_unrecognized() {
        let missing_sha = serde_json::json!({
            "databaseId": 42,
            "status": "completed",
            "conclusion": "success",
        });
        let missing_conclusion = serde_json::json!({
            "databaseId": 42,
            "headSha": SHA,
            "status": "completed",
        });
        let extra_field = serde_json::json!({
            "databaseId": 42,
            "headSha": SHA,
            "status": "completed",
            "conclusion": "success",
            "workflowName": "ci",
        });
        for item in [missing_sha, missing_conclusion, extra_field] {
            assert!(
                matches!(
                    parse(std::slice::from_ref(&item)),
                    Err(CiError::Unrecognized { .. })
                ),
                "accepted {item:?}"
            );
        }
    }

    #[test]
    fn an_unknown_status_is_unrecognized() {
        assert!(matches!(
            parse(&[run(
                SHA,
                "mystery",
                serde_json::Value::String("success".to_string())
            )]),
            Err(CiError::Unrecognized { .. })
        ));
    }

    #[test]
    fn a_completed_run_without_a_conclusion_is_unrecognized() {
        for conclusion in [
            serde_json::Value::Null,
            serde_json::Value::String(String::new()),
        ] {
            assert!(
                matches!(
                    parse(&[run(SHA, "completed", conclusion.clone())]),
                    Err(CiError::Unrecognized { .. })
                ),
                "accepted conclusion {conclusion:?}"
            );
        }
    }

    #[test]
    fn a_bad_expected_sha_is_rejected() {
        for sha in ["", "abc", &"g".repeat(40), &"a".repeat(39), &"a".repeat(41)] {
            assert!(
                matches!(parse_ci_runs("[]", sha), Err(CiError::InvalidSha { .. })),
                "accepted SHA {sha:?}"
            );
        }
    }

    #[test]
    fn green_waits_for_the_reviewer_and_then_finishes() {
        let root = TempRoot::new();
        let watcher = open(&root);
        let alpha = project("alpha");

        assert_eq!(
            watcher
                .decide(&alpha, 7, CiState::Green, false, None)
                .expect("green without review"),
            Decision::Wait
        );
        assert_eq!(
            watcher
                .decide(&alpha, 7, CiState::Green, true, None)
                .expect("green with review"),
            Decision::Done
        );
        assert_eq!(watcher.cycles(&alpha, 7).expect("cycles"), 0);
    }

    #[test]
    fn pending_waits_regardless_of_the_reviewer() {
        let root = TempRoot::new();
        let watcher = open(&root);
        let alpha = project("alpha");

        assert_eq!(
            watcher
                .decide(&alpha, 7, CiState::Pending, true, None)
                .expect("pending"),
            Decision::Wait
        );
    }

    #[test]
    fn three_failures_grant_fix_cycles_then_escalate_and_stick() {
        let root = TempRoot::new();
        let watcher = open(&root);
        let alpha = project("alpha");

        for cycle in 1..=MAX_FIX_CYCLES {
            assert_eq!(
                watcher
                    .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
                    .expect("fix cycle"),
                Decision::FixCycle {
                    cycle,
                    log_path: log_path(),
                }
            );
            assert_eq!(watcher.cycles(&alpha, 7).expect("cycles"), cycle);
        }

        for _ in 0..2 {
            assert_eq!(
                watcher
                    .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
                    .expect("escalate"),
                Decision::Escalate
            );
            assert_eq!(
                watcher.cycles(&alpha, 7).expect("cycles"),
                MAX_FIX_CYCLES,
                "the counter never exceeds the limit"
            );
        }
    }

    #[test]
    fn a_green_result_does_not_reset_the_counter() {
        let root = TempRoot::new();
        let watcher = open(&root);
        let alpha = project("alpha");

        assert_eq!(
            watcher
                .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
                .expect("first failure"),
            Decision::FixCycle {
                cycle: 1,
                log_path: log_path(),
            }
        );
        assert_eq!(
            watcher
                .decide(&alpha, 7, CiState::Green, true, None)
                .expect("green"),
            Decision::Done
        );
        assert_eq!(watcher.cycles(&alpha, 7).expect("cycles"), 1);
        assert_eq!(
            watcher
                .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
                .expect("second failure"),
            Decision::FixCycle {
                cycle: 2,
                log_path: log_path(),
            }
        );
    }

    #[test]
    fn a_failed_run_needs_a_safe_absolute_log_path() {
        let root = TempRoot::new();
        let watcher = open(&root);
        let alpha = project("alpha");

        assert!(matches!(
            watcher.decide(&alpha, 7, CiState::Failed, false, None),
            Err(CiError::MissingLogPath)
        ));
        for path in [
            PathBuf::from("logs/run.log"),
            PathBuf::from(""),
            PathBuf::from("/tmp/../etc/passwd"),
            PathBuf::from("/tmp/ci/../run.log"),
        ] {
            assert!(
                matches!(
                    watcher.decide(&alpha, 7, CiState::Failed, false, Some(&path)),
                    Err(CiError::InvalidLogPath { .. })
                ),
                "accepted log path {path:?}"
            );
        }
        assert_eq!(
            watcher.cycles(&alpha, 7).expect("cycles"),
            0,
            "a rejected log path does not consume a cycle"
        );
    }

    #[test]
    fn counters_are_per_project_and_pull_request() {
        let root = TempRoot::new();
        let watcher = open(&root);
        let alpha = project("alpha");
        let beta = project("beta");

        watcher
            .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
            .expect("alpha seven");
        watcher
            .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
            .expect("alpha seven again");

        assert_eq!(watcher.cycles(&alpha, 7).expect("alpha seven"), 2);
        assert_eq!(watcher.cycles(&alpha, 8).expect("alpha eight"), 0);
        assert_eq!(watcher.cycles(&beta, 7).expect("beta seven"), 0);
    }

    #[test]
    fn counters_survive_a_reopen() {
        let root = TempRoot::new();
        let alpha = project("alpha");
        {
            let watcher = open(&root);
            for _ in 0..2 {
                watcher
                    .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
                    .expect("fix cycle");
            }
        }

        let watcher = open(&root);
        assert_eq!(watcher.cycles(&alpha, 7).expect("cycles"), 2);
        assert_eq!(
            watcher
                .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
                .expect("third cycle"),
            Decision::FixCycle {
                cycle: 3,
                log_path: log_path(),
            }
        );
    }

    #[test]
    fn forget_removes_a_counter() {
        let root = TempRoot::new();
        let watcher = open(&root);
        let alpha = project("alpha");

        assert!(!watcher.forget(&alpha, 7).expect("forget absent"));
        watcher
            .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
            .expect("fix cycle");
        assert!(watcher.forget(&alpha, 7).expect("forget stored"));
        assert_eq!(watcher.cycles(&alpha, 7).expect("cycles"), 0);

        let reopened = open(&root);
        assert_eq!(reopened.cycles(&alpha, 7).expect("reopened cycles"), 0);
    }

    #[test]
    fn concurrent_watchers_never_exceed_the_fix_limit() {
        const THREADS: usize = 10;

        let root = TempRoot::new();
        let first = Arc::new(open(&root));
        let second = Arc::new(open(&root));
        let alpha = project("alpha");
        let failures: Vec<_> = (0..THREADS)
            .map(|index| {
                let watcher = if index % 2 == 0 {
                    Arc::clone(&first)
                } else {
                    Arc::clone(&second)
                };
                let alpha = alpha.clone();
                thread::spawn(move || {
                    watcher
                        .decide(&alpha, 7, CiState::Failed, false, Some(&log_path()))
                        .expect("decide")
                })
            })
            .collect();

        let decisions: Vec<Decision> = failures
            .into_iter()
            .map(|handle| handle.join().expect("join decider"))
            .collect();
        let cycles = decisions
            .iter()
            .filter(|decision| matches!(decision, Decision::FixCycle { .. }))
            .count();
        let escalated = decisions
            .iter()
            .filter(|decision| matches!(decision, Decision::Escalate))
            .count();
        assert_eq!(cycles, usize::from(MAX_FIX_CYCLES));
        assert_eq!(escalated, THREADS - usize::from(MAX_FIX_CYCLES));
        assert_eq!(
            first.cycles(&alpha, 7).expect("first cycles"),
            MAX_FIX_CYCLES
        );
        assert_eq!(
            second.cycles(&alpha, 7).expect("second cycles"),
            MAX_FIX_CYCLES
        );
    }

    #[test]
    fn corrupt_state_and_unknown_version_are_errors() {
        let root = TempRoot::new();
        fs::write(root.path().join(STATE_FILE), b"{not json").expect("write corrupt state");
        assert!(matches!(
            CiWatch::open(root.path()),
            Err(CiError::InvalidState { .. })
        ));

        let bad_version = serde_json::json!({ "version": SCHEMA_VERSION + 1 });
        fs::write(
            root.path().join(STATE_FILE),
            serde_json::to_vec(&bad_version).expect("json"),
        )
        .expect("write bad version");
        assert!(matches!(
            CiWatch::open(root.path()),
            Err(CiError::UnsupportedVersion { version, .. }) if version == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn state_above_the_fix_limit_is_corrupt() {
        let root = TempRoot::new();
        let state = serde_json::json!({
            "version": SCHEMA_VERSION,
            "records": [{
                "project": "alpha",
                "pr": 7,
                "cycles": MAX_FIX_CYCLES + 1,
            }],
        });
        fs::write(
            root.path().join(STATE_FILE),
            serde_json::to_vec(&state).expect("json"),
        )
        .expect("write over-limit state");
        assert!(matches!(
            CiWatch::open(root.path()),
            Err(CiError::CorruptState { .. })
        ));
    }
}
