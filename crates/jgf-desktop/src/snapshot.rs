//! Read-only snapshot of the factory state written by the lab runtime.
//!
//! The state lives under `jgf_core::paths::data_dir()` as plain files: the Board in
//! `board.json`, usage in `usage.json`, the scheduler in `scheduler.json` and the role
//! configuration in `roles.toml`. Loading never creates or modifies anything: the files are
//! read directly through the public `jgf-hub` types instead of the openers and accessors that
//! create lock files.

use std::{fs, io::ErrorKind, path::Path};

use jgf_core::{ProjectId, roles::RoleConfig};
use jgf_hub::{
    Message,
    board::Task,
    scheduler::MAX_CODERS,
    usage::{LabState, Level, Policy, Provider, Reading, Window, lab_state},
    watchdog::HEARTBEAT_TTL_SECS,
};
use serde::Deserialize;

use crate::chat_view::{ChatView, build_chat};

/// File name of the Board inside the data directory.
const BOARD_FILE: &str = "board.json";

/// File name of the usage meter inside the data directory.
const USAGE_FILE: &str = "usage.json";

/// File name of the scheduler inside the data directory.
const SCHEDULER_FILE: &str = "scheduler.json";

/// File name of the role configuration inside the data directory.
pub const ROLES_FILE: &str = "roles.toml";

/// Directory under the data directory holding the lab's routing store.
const ROUTING_DIR: &str = "routing";

/// The mailbox and cursor layout mirrors `jgf_hub::Router`: opening a router creates these
/// directories, so a read-only load derives the paths without opening one.
const MAILBOXES_DIR: &str = "mailboxes";
const STATE_DIR: &str = "state";
const CURSORS_DIR: &str = "cursors";
const USER_MAILBOX: &str = "user.jsonl";
const LAB_MAILBOX: &str = "lab-coordinator.jsonl";
const USER_CURSOR: &str = "user.cursor";

/// Lab selected by the desktop shell.
pub const LAB_ID: &str = "default";

/// Coordinator status file below the selected lab directory.
const COORDINATOR_STATUS_FILE: &str = "coordinator/status.json";
/// Directory containing the per-project Co-coordinator states.
const CO_COORDINATORS_DIR: &str = "coordinators";

/// Watchdog heartbeat file below the selected lab directory.
const COORDINATOR_HEARTBEAT_FILE: &str = "watchdog/watchdog.json";

/// Schema version written by the usage meter.
const USAGE_VERSION: u32 = 1;

/// Schema version written by the scheduler.
const SCHEDULER_VERSION: u32 = 1;

/// Everything the desktop shell draws, read from the data directory at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabSnapshot {
    /// Cards of the Board.
    pub tasks: Vec<Task>,
    /// Latest usage window of each provider, in file order.
    pub windows: Vec<(Provider, Window)>,
    /// What the lab is doing at the moment of the load.
    pub lab_state: LabState,
    /// The Lab Coordinator supervisor state.
    pub coordinator: CoordinatorSnapshot,
    /// Per-project Co-coordinator supervisor states.
    pub co_coordinators: Vec<CoCoordinatorSnapshot>,
    /// Role configuration, or the defaults when no file was written.
    pub roles: RoleConfig,
    /// The user's conversation with the Lab Coordinator.
    pub chat: ChatView,
    /// Short messages about sections that could not be read.
    pub errors: Vec<String>,
}

/// Status of one project's Co-coordinator supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoCoordinatorSnapshot {
    /// Project identifier.
    pub project: String,
    /// Persisted supervisor state.
    pub coordinator: CoordinatorSnapshot,
}

/// The supervisor state shown by the desktop shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorState {
    /// A coordinator turn is running.
    Running,
    /// The supervisor is waiting for work.
    Idle,
    /// A failed turn is waiting before retry.
    Backoff,
    /// No fresh supervisor is running.
    Stopped,
}

/// Coordinator status and its latest safe diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorSnapshot {
    /// Current supervisor state.
    pub state: CoordinatorState,
    /// Unix milliseconds when the last turn finished.
    pub last_turn_at_ms: Option<u64>,
    /// Safe diagnostic from the most recent failed turn.
    pub last_error: Option<String>,
    /// Remaining delay before a retry, when backing off.
    pub backoff_secs: Option<u64>,
}

impl Default for CoordinatorSnapshot {
    fn default() -> Self {
        Self {
            state: CoordinatorState::Stopped,
            last_turn_at_ms: None,
            last_error: None,
            backoff_secs: None,
        }
    }
}

impl Default for LabSnapshot {
    fn default() -> Self {
        Self {
            tasks: Vec::new(),
            windows: Vec::new(),
            lab_state: LabState::Idle,
            coordinator: CoordinatorSnapshot::default(),
            co_coordinators: Vec::new(),
            roles: RoleConfig::defaults(),
            chat: ChatView::default(),
            errors: Vec::new(),
        }
    }
}

impl LabSnapshot {
    /// Loads the snapshot from `data_dir` at `now` (unix seconds).
    ///
    /// Every section loads on its own: a missing file is the empty or default value, and a
    /// corrupt or unreadable file adds one message to [`LabSnapshot::errors`] while leaving
    /// that section at its default. No file in `data_dir` is created or changed.
    pub fn load(data_dir: &Path, now: u64) -> Self {
        let mut errors = Vec::new();

        let tasks = match load_board(data_dir) {
            Ok(tasks) => tasks,
            Err(message) => {
                errors.push(message);
                Vec::new()
            }
        };

        let usage = match load_usage(data_dir) {
            Ok(usage) => usage,
            Err(message) => {
                errors.push(message);
                Vec::new()
            }
        };

        let (running_workers, queued_projects) = match load_scheduler(data_dir) {
            Ok(counts) => counts,
            Err(message) => {
                errors.push(message);
                (0, 0)
            }
        };

        let roles = match load_roles(data_dir) {
            Ok(roles) => roles,
            Err(message) => {
                errors.push(message);
                RoleConfig::defaults()
            }
        };

        let (chat, chat_errors) = load_chat(data_dir);
        errors.extend(chat_errors);

        let coordinator = load_coordinator(data_dir, now, &mut errors);
        let co_coordinators = load_co_coordinators(data_dir, now, &mut errors);

        let windows = usage
            .iter()
            .map(|entry| (entry.provider.clone(), entry.reading.window))
            .collect();
        let lab_state = derive_lab_state(&usage, running_workers, queued_projects, now);

        Self {
            tasks,
            windows,
            lab_state,
            coordinator,
            co_coordinators,
            roles,
            chat,
            errors,
        }
    }
}

/// One usage reading together with its provider.
struct UsageEntry {
    provider: Provider,
    reading: Reading,
}

/// On-disk form of the Board, for the read-only load.
#[derive(Deserialize)]
struct BoardFile {
    #[serde(default)]
    tasks: Vec<Task>,
}

/// On-disk form of the usage meter, for the read-only load.
#[derive(Deserialize)]
struct UsageFile {
    version: u32,
    #[serde(default)]
    readings: Vec<UsageReading>,
}

/// One persisted usage reading.
#[derive(Deserialize)]
struct UsageReading {
    provider: Provider,
    window: Window,
    #[serde(default)]
    updated_at: u64,
}

/// On-disk form of the scheduler, for the read-only load.
#[derive(Deserialize)]
struct SchedulerFile {
    version: u32,
    #[serde(default)]
    slots: Vec<serde_json::Value>,
    #[serde(default)]
    projects: Vec<ProjectId>,
}

/// On-disk form of the coordinator status file.
#[derive(Deserialize)]
struct CoordinatorStatusFile {
    status: String,
    #[serde(default)]
    last_turn_at_ms: Option<u64>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    backoff_secs: Option<u64>,
}

/// The last heartbeat written by the supervisor's watchdog.
#[derive(Deserialize)]
struct CoordinatorHeartbeatFile {
    #[serde(default)]
    last_beat: Option<u64>,
}

/// Reads the supervisor status, treating an absent or stale supervisor as stopped.
fn load_coordinator(data_dir: &Path, now: u64, errors: &mut Vec<String>) -> CoordinatorSnapshot {
    let lab_dir = data_dir.join("labs").join(LAB_ID);
    let status_path = lab_dir.join(COORDINATOR_STATUS_FILE);
    let bytes = match read_or_missing(&status_path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return CoordinatorSnapshot::default(),
        Err(error) => {
            errors.push(format!("{COORDINATOR_STATUS_FILE}: {error}"));
            return CoordinatorSnapshot::default();
        }
    };
    let status = match serde_json::from_slice::<CoordinatorStatusFile>(&bytes) {
        Ok(status) => status,
        Err(error) => {
            errors.push(format!("{COORDINATOR_STATUS_FILE}: {error}"));
            return CoordinatorSnapshot::default();
        }
    };
    let state = match status.status.as_str() {
        "running" => CoordinatorState::Running,
        "idle" => CoordinatorState::Idle,
        "backoff" => CoordinatorState::Backoff,
        "stopped" => CoordinatorState::Stopped,
        _ => {
            errors.push(format!(
                "{COORDINATOR_STATUS_FILE}: unknown status {:?}",
                status.status
            ));
            return CoordinatorSnapshot::default();
        }
    };

    let state = if state != CoordinatorState::Stopped
        && !coordinator_heartbeat_is_fresh(&lab_dir.join(COORDINATOR_HEARTBEAT_FILE), now, errors)
    {
        CoordinatorState::Stopped
    } else {
        state
    };

    CoordinatorSnapshot {
        state,
        last_turn_at_ms: status.last_turn_at_ms,
        last_error: status.last_error,
        backoff_secs: status.backoff_secs,
    }
}

/// Reads each project's status without opening a router or creating state.
fn load_co_coordinators(
    data_dir: &Path,
    now: u64,
    errors: &mut Vec<String>,
) -> Vec<CoCoordinatorSnapshot> {
    let root = data_dir.join("labs").join(LAB_ID).join(CO_COORDINATORS_DIR);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            errors.push(format!("{CO_COORDINATORS_DIR}: {error}"));
            return Vec::new();
        }
    };

    let mut coordinators = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("{CO_COORDINATORS_DIR}: {error}"));
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                errors.push(format!("{CO_COORDINATORS_DIR}: {error}"));
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(project) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if ProjectId::parse(&project).is_err() {
            errors.push(format!("{CO_COORDINATORS_DIR}: invalid project directory"));
            continue;
        }
        let relative_status = format!("{CO_COORDINATORS_DIR}/{project}/status.json");
        let status_path = entry.path().join("status.json");
        let bytes = match read_or_missing(&status_path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(error) => {
                errors.push(format!("{relative_status}: {error}"));
                continue;
            }
        };
        let status = match serde_json::from_slice::<CoordinatorStatusFile>(&bytes) {
            Ok(status) => status,
            Err(error) => {
                errors.push(format!("{relative_status}: {error}"));
                continue;
            }
        };
        let mut state = match status.status.as_str() {
            "running" => CoordinatorState::Running,
            "idle" => CoordinatorState::Idle,
            "backoff" => CoordinatorState::Backoff,
            "stopped" => CoordinatorState::Stopped,
            _ => {
                errors.push(format!(
                    "{relative_status}: unknown status {:?}",
                    status.status
                ));
                continue;
            }
        };
        if state != CoordinatorState::Stopped
            && !coordinator_heartbeat_is_fresh(
                &entry.path().join("watchdog/watchdog.json"),
                now,
                errors,
            )
        {
            state = CoordinatorState::Stopped;
        }
        coordinators.push(CoCoordinatorSnapshot {
            project,
            coordinator: CoordinatorSnapshot {
                state,
                last_turn_at_ms: status.last_turn_at_ms,
                last_error: status.last_error,
                backoff_secs: status.backoff_secs,
            },
        });
    }
    coordinators.sort_by(|left, right| left.project.cmp(&right.project));
    coordinators
}

/// Applies the watchdog's heartbeat freshness window without opening or changing its files.
fn coordinator_heartbeat_is_fresh(path: &Path, now: u64, errors: &mut Vec<String>) -> bool {
    let bytes = match read_or_missing(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return false,
        Err(error) => {
            errors.push(format!("{COORDINATOR_HEARTBEAT_FILE}: {error}"));
            return false;
        }
    };
    let heartbeat = match serde_json::from_slice::<CoordinatorHeartbeatFile>(&bytes) {
        Ok(heartbeat) => heartbeat,
        Err(error) => {
            errors.push(format!("{COORDINATOR_HEARTBEAT_FILE}: {error}"));
            return false;
        }
    };
    heartbeat
        .last_beat
        .is_some_and(|last_beat| now.saturating_sub(last_beat) <= HEARTBEAT_TTL_SECS)
}

/// Reads `board.json`, or an empty Board when it does not exist.
fn load_board(data_dir: &Path) -> Result<Vec<Task>, String> {
    let Some(bytes) = read_or_missing(&data_dir.join(BOARD_FILE)).map_err(board_error)? else {
        return Ok(Vec::new());
    };
    serde_json::from_slice::<BoardFile>(&bytes)
        .map(|file| file.tasks)
        .map_err(|error| format!("{BOARD_FILE}: {error}"))
}

/// Reads `usage.json`, or an empty list when it does not exist.
fn load_usage(data_dir: &Path) -> Result<Vec<UsageEntry>, String> {
    let Some(bytes) = read_or_missing(&data_dir.join(USAGE_FILE)).map_err(usage_error)? else {
        return Ok(Vec::new());
    };
    let file = serde_json::from_slice::<UsageFile>(&bytes)
        .map_err(|error| format!("{USAGE_FILE}: {error}"))?;
    if file.version != USAGE_VERSION {
        return Err(format!(
            "{USAGE_FILE}: unsupported version {}",
            file.version
        ));
    }

    file.readings
        .into_iter()
        .map(|record| {
            let window = Window::new(
                record.window.used,
                record.window.limit,
                record.window.resets_at,
            )
            .map_err(|error| format!("{USAGE_FILE}: {error}"))?;
            Ok(UsageEntry {
                provider: record.provider,
                reading: Reading {
                    window,
                    updated_at: record.updated_at,
                },
            })
        })
        .collect()
}

/// Reads `scheduler.json` and returns its running-worker and queued-project counts.
///
/// A missing file is an empty scheduler.
fn load_scheduler(data_dir: &Path) -> Result<(usize, usize), String> {
    let Some(bytes) = read_or_missing(&data_dir.join(SCHEDULER_FILE)).map_err(scheduler_error)?
    else {
        return Ok((0, 0));
    };
    let file = serde_json::from_slice::<SchedulerFile>(&bytes)
        .map_err(|error| format!("{SCHEDULER_FILE}: {error}"))?;
    if file.version != SCHEDULER_VERSION {
        return Err(format!(
            "{SCHEDULER_FILE}: unsupported version {}",
            file.version
        ));
    }
    Ok((file.slots.len(), file.projects.len()))
}

/// Reads `roles.toml`, or the defaults when it does not exist.
fn load_roles(data_dir: &Path) -> Result<RoleConfig, String> {
    RoleConfig::load(&data_dir.join(ROLES_FILE)).map_err(|error| format!("{ROLES_FILE}: {error}"))
}

/// Reads both sides of the user conversation without creating anything.
///
/// A malformed line is skipped and reported; a missing mailbox is empty. The unread count of
/// the user's mailbox is the number of stored messages after the router's read cursor.
fn load_chat(data_dir: &Path) -> (ChatView, Vec<String>) {
    let routing = data_dir.join(ROUTING_DIR);
    let mailboxes = routing.join(MAILBOXES_DIR);
    let mut errors = Vec::new();

    let user_to_lab = read_mailbox(&mailboxes.join(LAB_MAILBOX), &mut errors);
    let lab_to_user = read_mailbox(&mailboxes.join(USER_MAILBOX), &mut errors);
    let cursor_path = routing.join(STATE_DIR).join(CURSORS_DIR).join(USER_CURSOR);
    let cursor = read_cursor(&cursor_path, lab_to_user.len(), &mut errors);
    let unread = lab_to_user.len().saturating_sub(cursor);

    (build_chat(&user_to_lab, &lab_to_user, unread), errors)
}

/// Reads one JSONL mailbox; malformed lines are skipped with one short error each.
fn read_mailbox(path: &Path, errors: &mut Vec<String>) -> Vec<Message> {
    let label = file_label(path);
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            errors.push(format!("{label}: {error}"));
            return Vec::new();
        }
    };

    let mut messages = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Message>(line) {
            Ok(message) => messages.push(message),
            Err(error) => errors.push(format!("{label}: line {} skipped: {error}", index + 1)),
        }
    }
    messages
}

/// Reads the user's read cursor; a missing cursor means no message was read yet.
fn read_cursor(path: &Path, message_count: usize, errors: &mut Vec<String>) -> usize {
    let label = file_label(path);
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return 0,
        Err(error) => {
            errors.push(format!("{label}: {error}"));
            return 0;
        }
    };

    match raw.trim().parse::<usize>() {
        Ok(cursor) if cursor <= message_count => cursor,
        Ok(cursor) => {
            errors.push(format!(
                "{label}: cursor {cursor} is ahead of {message_count} messages"
            ));
            0
        }
        Err(error) => {
            errors.push(format!("{label}: {error}"));
            0
        }
    }
}

/// The file name of `path`, for a short error message.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Reads `path`, returning `None` when it does not exist.
fn read_or_missing(path: &Path) -> Result<Option<Vec<u8>>, std::io::Error> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// A short Board read error.
fn board_error(error: std::io::Error) -> String {
    format!("{BOARD_FILE}: {error}")
}

/// A short usage read error.
fn usage_error(error: std::io::Error) -> String {
    format!("{USAGE_FILE}: {error}")
}

/// A short scheduler read error.
fn scheduler_error(error: std::io::Error) -> String {
    format!("{SCHEDULER_FILE}: {error}")
}

/// Summarizes the lab from its usage readings and scheduler occupancy.
fn derive_lab_state(
    usage: &[UsageEntry],
    running_workers: usize,
    queued_projects: usize,
    now: u64,
) -> LabState {
    let throttle = usage
        .iter()
        .map(|entry| Policy::throttle(Some(&entry.reading), MAX_CODERS, now))
        .max_by_key(|throttle| level_rank(throttle.level))
        .unwrap_or_else(|| Policy::throttle(None, MAX_CODERS, now));
    lab_state(&throttle, running_workers, queued_projects, now)
}

/// Ordering key for throttle levels, from least to most restrictive.
fn level_rank(level: Level) -> u8 {
    match level {
        Level::Normal => 0,
        Level::Soft => 1,
        Level::Hard => 2,
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::Path};

    use jgf_core::{AuditLog, ProjectId};
    use jgf_hub::{
        board::{Actor, Board},
        routing::{MessageKind, OutgoingMessage, Role, Router},
        usage::{LabState, Provider, UsageMeter, Window},
    };

    use super::*;

    /// Every file and directory under `dir`, relative and sorted.
    fn listing(dir: &Path) -> Vec<String> {
        fn collect(root: &Path, dir: &Path, entries: &mut Vec<String>) {
            let Ok(read_dir) = fs::read_dir(dir) else {
                return;
            };
            for entry in read_dir.flatten() {
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                if path.is_dir() {
                    collect(root, &path, entries);
                }
                entries.push(relative);
            }
        }

        let mut entries = Vec::new();
        collect(dir, dir, &mut entries);
        entries.sort();
        entries
    }

    /// Opens the routing store the snapshot reads under `dir`.
    fn open_router(dir: &Path) -> Router {
        Router::open(dir.join(ROUTING_DIR)).expect("open router")
    }

    /// Writes one message in each direction through a real router.
    fn write_chat(dir: &Path) {
        let router = open_router(dir);
        router
            .send(OutgoingMessage::new(
                Role::User,
                Role::LabCoordinator,
                MessageKind::UserRequest,
                "hello",
                "hello lab",
            ))
            .expect("user request");
        router
            .send(OutgoingMessage::new(
                Role::LabCoordinator,
                Role::User,
                MessageKind::Report,
                "progress",
                "working on it",
            ))
            .expect("report");
    }

    /// Writes a valid Board and usage file through the public hub types.
    fn write_board_and_usage(dir: &Path) {
        let audit = AuditLog::open(dir, "test").expect("audit log");
        let board = Board::open(dir, audit).expect("open board");
        board
            .create(
                &Actor::User,
                ProjectId::parse("alpha").expect("project id"),
                "first task",
                "",
            )
            .expect("create task");

        let meter = UsageMeter::open(dir).expect("open meter");
        meter
            .record(
                &Provider::claude(),
                Window::new(2, 10, 10_000).expect("window"),
                1_000,
            )
            .expect("record usage");
    }

    fn write_coordinator_status(dir: &Path, status: &str, last_error: Option<&str>) {
        let path = dir.join("labs").join(LAB_ID).join(COORDINATOR_STATUS_FILE);
        fs::create_dir_all(path.parent().expect("status parent")).expect("create status dir");
        fs::write(
            path,
            serde_json::json!({
                "status": status,
                "last_turn_at_ms": 1234,
                "last_error": last_error,
                "backoff_secs": (status == "backoff").then_some(5),
            })
            .to_string(),
        )
        .expect("write coordinator status");
    }

    fn write_coordinator_heartbeat(dir: &Path, last_beat: u64) {
        let path = dir
            .join("labs")
            .join(LAB_ID)
            .join(COORDINATOR_HEARTBEAT_FILE);
        fs::create_dir_all(path.parent().expect("heartbeat parent")).expect("create heartbeat dir");
        fs::write(
            path,
            serde_json::json!({"version": 1, "last_beat": last_beat}).to_string(),
        )
        .expect("write coordinator heartbeat");
    }

    fn write_co_coordinator_status(dir: &Path, project: &str, status: &str) {
        let path = dir
            .join("labs")
            .join(LAB_ID)
            .join(CO_COORDINATORS_DIR)
            .join(project)
            .join("status.json");
        fs::create_dir_all(path.parent().expect("coordinator status parent"))
            .expect("create project coordinator directory");
        fs::write(
            path,
            serde_json::json!({
                "status": status,
                "last_turn_at_ms": 2345,
                "last_error": null,
                "backoff_secs": null,
            })
            .to_string(),
        )
        .expect("write project coordinator status");
    }

    fn write_co_coordinator_heartbeat(dir: &Path, project: &str, last_beat: u64) {
        let path = dir
            .join("labs")
            .join(LAB_ID)
            .join(CO_COORDINATORS_DIR)
            .join(project)
            .join("watchdog/watchdog.json");
        fs::create_dir_all(path.parent().expect("coordinator heartbeat parent"))
            .expect("create project watchdog directory");
        fs::write(
            path,
            serde_json::json!({"version": 1, "last_beat": last_beat}).to_string(),
        )
        .expect("write project coordinator heartbeat");
    }

    #[test]
    fn empty_directory_is_every_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let snapshot = LabSnapshot::load(dir.path(), 0);

        assert!(snapshot.tasks.is_empty());
        assert!(snapshot.windows.is_empty());
        assert_eq!(snapshot.lab_state, LabState::Idle);
        assert_eq!(snapshot.coordinator, CoordinatorSnapshot::default());
        assert_eq!(snapshot.roles, RoleConfig::defaults());
        assert_eq!(snapshot.chat, ChatView::default());
        assert!(snapshot.errors.is_empty());
    }

    #[test]
    fn coordinator_status_parses_every_state_and_missing_file_as_stopped() {
        let cases = [
            ("running", CoordinatorState::Running),
            ("idle", CoordinatorState::Idle),
            ("backoff", CoordinatorState::Backoff),
            ("stopped", CoordinatorState::Stopped),
        ];

        for (status, expected) in cases {
            let dir = tempfile::tempdir().expect("temp dir");
            write_coordinator_status(dir.path(), status, Some("last failure"));
            if matches!(
                expected,
                CoordinatorState::Running | CoordinatorState::Idle | CoordinatorState::Backoff
            ) {
                write_coordinator_heartbeat(
                    dir.path(),
                    10_000u64.saturating_sub(HEARTBEAT_TTL_SECS),
                );
            }

            let snapshot = LabSnapshot::load(dir.path(), 10_000);

            assert_eq!(snapshot.coordinator.state, expected, "{status}");
            assert_eq!(snapshot.coordinator.last_turn_at_ms, Some(1234));
            assert_eq!(
                snapshot.coordinator.last_error.as_deref(),
                Some("last failure")
            );
            assert_eq!(
                snapshot.coordinator.backoff_secs,
                (status == "backoff").then_some(5)
            );
            assert!(
                snapshot.errors.is_empty(),
                "{status}: {:?}",
                snapshot.errors
            );
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let snapshot = LabSnapshot::load(dir.path(), 10_000);
        assert_eq!(snapshot.coordinator.state, CoordinatorState::Stopped);
        assert_eq!(snapshot.coordinator.last_error, None);
        assert!(snapshot.errors.is_empty());
    }

    #[test]
    fn project_coordinator_statuses_are_loaded_read_only_and_sorted() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_co_coordinator_status(dir.path(), "project-b", "running");
        write_co_coordinator_status(dir.path(), "project-a", "idle");
        write_co_coordinator_heartbeat(dir.path(), "project-b", 10_000);
        write_co_coordinator_heartbeat(dir.path(), "project-a", 10_000);

        let snapshot = LabSnapshot::load(dir.path(), 10_000);

        assert_eq!(
            snapshot
                .co_coordinators
                .iter()
                .map(|coordinator| (coordinator.project.as_str(), coordinator.coordinator.state))
                .collect::<Vec<_>>(),
            [
                ("project-a", CoordinatorState::Idle),
                ("project-b", CoordinatorState::Running)
            ]
        );
        assert!(snapshot.errors.is_empty());
    }

    #[test]
    fn running_idle_and_backoff_with_stale_heartbeats_are_stopped() {
        for status in ["running", "idle", "backoff"] {
            let dir = tempfile::tempdir().expect("temp dir");
            write_coordinator_status(dir.path(), status, None);
            write_coordinator_heartbeat(
                dir.path(),
                10_000u64.saturating_sub(HEARTBEAT_TTL_SECS + 1),
            );

            let snapshot = LabSnapshot::load(dir.path(), 10_000);

            assert_eq!(snapshot.coordinator.state, CoordinatorState::Stopped);
            assert!(snapshot.errors.is_empty());
        }
    }

    #[test]
    fn valid_board_and_usage_load_without_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_board_and_usage(dir.path());

        let snapshot = LabSnapshot::load(dir.path(), 2_000);

        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].key, "ALPHA-1");
        assert_eq!(snapshot.tasks[0].title, "first task");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].0, Provider::claude());
        assert_eq!(snapshot.windows[0].1.percent(), 20);
        assert_eq!(snapshot.roles, RoleConfig::defaults());
    }

    #[test]
    fn corrupt_board_keeps_other_sections() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_board_and_usage(dir.path());
        fs::write(dir.path().join(BOARD_FILE), b"{not json").expect("write corrupt board");

        let snapshot = LabSnapshot::load(dir.path(), 2_000);

        assert!(snapshot.tasks.is_empty());
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.roles, RoleConfig::defaults());
        assert!(
            snapshot
                .errors
                .iter()
                .any(|error| error.contains(BOARD_FILE)),
            "{:?}",
            snapshot.errors
        );
        assert!(
            !snapshot
                .errors
                .iter()
                .any(|error| error.contains(USAGE_FILE)),
            "{:?}",
            snapshot.errors
        );
    }

    #[test]
    fn load_does_not_touch_the_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_board_and_usage(dir.path());
        fs::write(dir.path().join(SCHEDULER_FILE), br#"{"version":1}"#).expect("write scheduler");
        fs::write(dir.path().join(ROLES_FILE), b"").expect("write roles");
        let before = listing(dir.path());

        let snapshot = LabSnapshot::load(dir.path(), 2_000);
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);

        assert_eq!(listing(dir.path()), before);
    }

    #[test]
    fn chat_messages_load_from_both_mailboxes() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_chat(dir.path());

        let snapshot = LabSnapshot::load(dir.path(), 2_000);

        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        assert_eq!(snapshot.chat.lines.len(), 2);
        assert!(snapshot.chat.lines[0].from_user);
        assert_eq!(snapshot.chat.lines[0].subject, "hello");
        assert_eq!(snapshot.chat.lines[0].body, "hello lab");
        assert!(!snapshot.chat.lines[1].from_user);
        assert_eq!(snapshot.chat.lines[1].body, "working on it");
        assert_eq!(snapshot.chat.unread_from_coordinator, 1);
    }

    #[test]
    fn the_user_cursor_sets_the_unread_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_chat(dir.path());
        open_router(dir.path()).ack(&Role::User).expect("ack");

        let snapshot = LabSnapshot::load(dir.path(), 2_000);

        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        assert_eq!(snapshot.chat.unread_from_coordinator, 0);
    }

    #[test]
    fn loading_chat_does_not_touch_the_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_chat(dir.path());
        let before = listing(dir.path());

        let snapshot = LabSnapshot::load(dir.path(), 2_000);
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        assert_eq!(snapshot.chat.lines.len(), 2);

        assert_eq!(listing(dir.path()), before);
    }

    #[test]
    fn a_malformed_chat_line_is_skipped_with_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_chat(dir.path());
        let mailbox = dir
            .path()
            .join(ROUTING_DIR)
            .join(MAILBOXES_DIR)
            .join(LAB_MAILBOX);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&mailbox)
            .expect("open mailbox");
        file.write_all(b"not json\n").expect("append corrupt line");
        file.sync_all().expect("sync mailbox");

        let snapshot = LabSnapshot::load(dir.path(), 2_000);

        assert_eq!(snapshot.chat.lines.len(), 2);
        assert!(
            snapshot
                .errors
                .iter()
                .any(|error| { error.contains(LAB_MAILBOX) && error.contains("line 2") }),
            "{:?}",
            snapshot.errors
        );
    }
}
