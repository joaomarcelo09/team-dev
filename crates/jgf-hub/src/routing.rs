//! Message routing inside a Dark Factory lab.
//!
//! A lab has one user-facing Lab Coordinator, one Co-coordinator per project and
//! the workers of each project. Messages travel along one fixed matrix:
//!
//! - the user may send [`MessageKind::UserRequest`] only to the Lab Coordinator;
//! - the Lab Coordinator may send [`MessageKind::Demand`] to any Co-coordinator,
//!   tagged with that Co-coordinator's project, and [`MessageKind::Report`] to
//!   the user, never to a worker;
//! - a Co-coordinator may send [`MessageKind::Instruction`] only to workers of
//!   its own project and [`MessageKind::Milestone`], [`MessageKind::Failure`] or
//!   [`MessageKind::BudgetEvent`] only to the Lab Coordinator with its own
//!   `project`;
//! - a worker may send [`MessageKind::WorkerResult`] only to the Co-coordinator
//!   of its own project.
//!
//! The project tag is checked on every route: a sender that belongs to a
//! project may only tag its messages with that project, and the router fills in
//! the sender's project when the tag is missing. [`MessageKind::UserRequest`]
//! and [`MessageKind::Report`] may carry any project because the user and the
//! Lab span projects.
//!
//! Anything outside that matrix is refused with [`RoutingError::NotAllowed`].
//! There is no silent drop and no catch-all delivery.
//!
//! Each recipient has one append-only JSONL mailbox under
//! `<dir>/mailboxes/`; messages are never edited or deleted. A per-recipient
//! cursor under `<dir>/state/cursors/` tracks what has been read. Appends and
//! cursor updates run under an exclusive `flock`, so several [`Router`]
//! instances may share a directory. Only the Lab Coordinator's inbox has a wake
//! notion: the router sets `wakes` from the message kind, never from the
//! sender, and routine worker results never reach the Lab inbox.
//!
//! The [`crate::shim`] module exposes this router over a host-owned unix socket
//! for sandboxed roles. It binds each launch to a host-issued token, returns
//! typed refusals, and emits token-free send/refusal events for runtime audit
//! callbacks. The lab runtime owns the callback and the launch-token lifetime.

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use jgf_core::{ModelError, ProjectId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum length of a worker id, matching [`ProjectId`].
const MAX_WORKER_ID_LEN: usize = 40;

/// Maximum size of a message body in bytes.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Name of the directory holding the recipient mailboxes.
const MAILBOXES_DIR: &str = "mailboxes";

/// Name of the directory holding routing bookkeeping.
const STATE_DIR: &str = "state";

/// Name of the cross-process lock file.
const LOCK_FILE: &str = "store.lock";

/// Name of the file holding the message id sequence.
const SEQUENCE_FILE: &str = "sequence";

/// Name of the directory holding the read cursors.
const CURSORS_DIR: &str = "cursors";

/// The kind of worker a project team runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    /// Implements a task.
    Coder,
    /// Writes and runs acceptance tests.
    Tester,
    /// Reviews a change.
    Reviewer,
    /// Commits, pushes and opens pull requests.
    GitCoder,
}

impl WorkerKind {
    /// Stable name used in mailbox file names and in logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Coder => "coder",
            Self::Tester => "tester",
            Self::Reviewer => "reviewer",
            Self::GitCoder => "git-coder",
        }
    }
}

/// One participant of the lab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "RoleData")]
pub enum Role {
    /// The user, who talks only to the Lab Coordinator.
    User,
    /// The only session that talks to the user.
    LabCoordinator,
    /// The coordinator of one project.
    CoCoordinator(ProjectId),
    /// A worker of one project.
    Worker {
        /// Project the worker belongs to.
        project: ProjectId,
        /// Validated worker identifier.
        id: String,
        /// Role of the worker.
        kind: WorkerKind,
    },
}

impl Role {
    /// Builds a worker role after validating its id.
    pub fn worker(
        project: ProjectId,
        id: impl Into<String>,
        kind: WorkerKind,
    ) -> Result<Self, RoutingError> {
        let id = id.into();
        validate_worker_id(&id)?;
        Ok(Self::Worker { project, id, kind })
    }

    /// Validates a role, including a worker id built outside of [`Role::worker`].
    pub fn validate(&self) -> Result<(), RoutingError> {
        if let Self::Worker { id, .. } = self {
            validate_worker_id(id)?;
        }
        Ok(())
    }

    /// Safe stem of this role's mailbox and cursor file names.
    fn file_stem(&self) -> String {
        match self {
            Self::User => "user".to_string(),
            Self::LabCoordinator => "lab-coordinator".to_string(),
            Self::CoCoordinator(project) => format!("co-coordinator.{project}"),
            Self::Worker { project, id, kind } => {
                format!("worker.{project}.{}.{id}", kind.as_str())
            }
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User => f.write_str("user"),
            Self::LabCoordinator => f.write_str("lab-coordinator"),
            Self::CoCoordinator(project) => write!(f, "co-coordinator:{project}"),
            Self::Worker { project, id, kind } => {
                write!(f, "worker:{project}:{}:{id}", kind.as_str())
            }
        }
    }
}

/// Wire form of [`Role`], used to validate worker ids while deserializing.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RoleData {
    User,
    LabCoordinator,
    CoCoordinator(ProjectId),
    Worker {
        project: ProjectId,
        id: String,
        kind: WorkerKind,
    },
}

impl TryFrom<RoleData> for Role {
    type Error = RoutingError;

    fn try_from(data: RoleData) -> Result<Self, Self::Error> {
        let role = match data {
            RoleData::User => Self::User,
            RoleData::LabCoordinator => Self::LabCoordinator,
            RoleData::CoCoordinator(project) => Self::CoCoordinator(project),
            RoleData::Worker { project, id, kind } => Self::Worker { project, id, kind },
        };
        role.validate()?;
        Ok(role)
    }
}

/// What a message is for; the kind determines its allowed route and wake flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// User to Lab Coordinator: a request or answer.
    UserRequest,
    /// Lab Coordinator to user: progress only.
    Report,
    /// Lab Coordinator to a Co-coordinator: work to start.
    Demand,
    /// Worker to its Co-coordinator: the outcome of a task.
    WorkerResult,
    /// Co-coordinator to one of its workers: what to do.
    Instruction,
    /// Co-coordinator to Lab Coordinator: a project milestone.
    Milestone,
    /// Co-coordinator to Lab Coordinator: something failed.
    Failure,
    /// Co-coordinator to Lab Coordinator: a budget event.
    BudgetEvent,
}

impl MessageKind {
    /// Whether this kind wakes the Lab Coordinator.
    pub const fn wakes(self) -> bool {
        matches!(
            self,
            Self::Milestone | Self::Failure | Self::BudgetEvent | Self::UserRequest
        )
    }
}

/// A message as a sender hands it to the router.
///
/// The sender may set `wakes`, but [`Router::send`] ignores it and derives the
/// flag from [`MessageKind::wakes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingMessage {
    /// Sender.
    pub from: Role,
    /// Recipient.
    pub to: Role,
    /// Purpose of the message.
    pub kind: MessageKind,
    /// Project the message belongs to, when it belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
    /// Short subject.
    pub subject: String,
    /// Message body, capped by [`MAX_BODY_BYTES`].
    pub body: String,
    /// Ignored by the router; kept so a sender cannot spoof a wake.
    #[serde(default)]
    pub wakes: bool,
}

impl OutgoingMessage {
    /// Builds an outgoing message with no project and no wake flag.
    pub fn new(
        from: Role,
        to: Role,
        kind: MessageKind,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            from,
            to,
            kind,
            project: None,
            subject: subject.into(),
            body: body.into(),
            wakes: false,
        }
    }

    /// Adds the project the message belongs to.
    pub fn with_project(mut self, project: ProjectId) -> Self {
        self.project = Some(project);
        self
    }

    /// Sets the sender's wake wish, which [`Router::send`] ignores.
    pub fn with_wakes(mut self, wakes: bool) -> Self {
        self.wakes = wakes;
        self
    }
}

/// A message as stored in a mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Monotonic id within the mailbox store.
    pub id: u64,
    /// Sender.
    pub from: Role,
    /// Recipient.
    pub to: Role,
    /// Purpose of the message.
    pub kind: MessageKind,
    /// Project the message belongs to, when it belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
    /// Short subject.
    pub subject: String,
    /// Message body.
    pub body: String,
    /// Wall-clock time the router stored the message, in milliseconds.
    pub at_ms: u64,
    /// Whether the Lab Coordinator should wake for this message.
    pub wakes: bool,
}

/// Configuration of one project's Co-coordinator.
///
/// This is the harness-agnostic registry entry; [`Router`] only stores and
/// routes messages and never runs a coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "CoordinatorConfigData")]
pub struct CoordinatorConfig {
    /// Project the Co-coordinator runs.
    pub project: ProjectId,
    /// Harness that runs the Co-coordinator, for example `opencode`.
    pub harness: String,
    /// Optional model override for that harness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl CoordinatorConfig {
    /// Builds a validated Co-coordinator configuration.
    pub fn new(
        project: ProjectId,
        harness: impl Into<String>,
        model: Option<String>,
    ) -> Result<Self, RoutingError> {
        let config = Self {
            project,
            harness: harness.into(),
            model,
        };
        config.validate()?;
        Ok(config)
    }

    /// Refuses a blank harness or a present but blank model.
    pub fn validate(&self) -> Result<(), RoutingError> {
        if self.harness.trim().is_empty() {
            return Err(RoutingError::EmptyHarness);
        }
        if self
            .model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(RoutingError::EmptyModel);
        }
        Ok(())
    }
}

/// Wire form of [`CoordinatorConfig`], used to validate while deserializing.
#[derive(Deserialize)]
struct CoordinatorConfigData {
    project: ProjectId,
    harness: String,
    #[serde(default)]
    model: Option<String>,
}

impl TryFrom<CoordinatorConfigData> for CoordinatorConfig {
    type Error = RoutingError;

    fn try_from(data: CoordinatorConfigData) -> Result<Self, Self::Error> {
        Self::new(data.project, data.harness, data.model)
    }
}

/// Errors raised while validating or delivering a message.
#[derive(Debug, Error)]
pub enum RoutingError {
    /// Reading or writing routing state failed.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// A worker id does not follow the worker id rules.
    #[error("invalid worker id {id:?}")]
    InvalidWorkerId {
        /// Rejected worker id.
        id: String,
    },

    /// The sender may not send this kind to this recipient.
    #[error("message {kind:?} from {from} to {to} is not allowed")]
    NotAllowed {
        /// Sender of the refused message.
        from: Role,
        /// Intended recipient.
        to: Role,
        /// Refused message kind.
        kind: MessageKind,
    },

    /// The message body is larger than [`MAX_BODY_BYTES`].
    #[error("message body is {len} bytes; the limit is {max}")]
    BodyTooLarge {
        /// Body size in bytes.
        len: usize,
        /// Configured limit.
        max: usize,
    },

    /// A Co-coordinator harness is empty.
    #[error("co-coordinator harness must not be empty")]
    EmptyHarness,

    /// A Co-coordinator model override is present but empty.
    #[error("co-coordinator model must not be empty when set")]
    EmptyModel,

    /// A mailbox name derived from a role is not safe as a file name.
    #[error("invalid mailbox name {name:?}")]
    InvalidMailboxName {
        /// Rejected file stem.
        name: String,
    },

    /// A persisted mailbox contains a line that is not a message.
    #[error("corrupt mailbox at {} line {line}: {detail}", path.display())]
    CorruptMailbox {
        /// Path of the rejected mailbox.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// What is wrong.
        detail: String,
    },

    /// Persisted routing state is not valid.
    #[error("corrupt routing state at {}: {detail}", path.display())]
    CorruptState {
        /// Path of the rejected state file.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },
}

impl From<serde_json::Error> for RoutingError {
    fn from(source: serde_json::Error) -> Self {
        Self::Model(ModelError::Json(source))
    }
}

/// One lab mailbox store: append-only recipient mailboxes plus read cursors.
///
/// Every instance may be cloned and used from several threads; cross-process
/// exclusion comes from an exclusive `flock` around each append and cursor
/// update.
#[derive(Debug, Clone)]
pub struct Router {
    dir: PathBuf,
    mailboxes: PathBuf,
    cursors: PathBuf,
    lock_path: PathBuf,
    sequence_path: PathBuf,
}

impl Router {
    /// Opens (creating when needed) the routing directories below `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, RoutingError> {
        let dir = dir.into();
        let mailboxes = dir.join(MAILBOXES_DIR);
        let state = dir.join(STATE_DIR);
        let cursors = state.join(CURSORS_DIR);
        fs::create_dir_all(&mailboxes).map_err(|source| io_error(&mailboxes, source))?;
        fs::create_dir_all(&cursors).map_err(|source| io_error(&cursors, source))?;
        Ok(Self {
            dir,
            mailboxes,
            cursors,
            lock_path: state.join(LOCK_FILE),
            sequence_path: state.join(SEQUENCE_FILE),
        })
    }

    /// Root directory of this routing store.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Validates the route and appends the message to its recipient's mailbox.
    ///
    /// The router sets the id, timestamp, wake flag and normalised project tag.
    /// The sender's `wakes` field is ignored. Nothing is delivered unless the
    /// whole route is allowed.
    pub fn send(&self, draft: OutgoingMessage) -> Result<Message, RoutingError> {
        draft.from.validate()?;
        draft.to.validate()?;
        let project = ensure_route(&draft.from, &draft.to, draft.kind, draft.project.as_ref())?;
        if draft.body.len() > MAX_BODY_BYTES {
            return Err(RoutingError::BodyTooLarge {
                len: draft.body.len(),
                max: MAX_BODY_BYTES,
            });
        }

        let path = self.mailbox_path(&draft.to)?;
        let _lock = self.lock_exclusive()?;
        let id = self.next_id()?;
        let message = Message {
            id,
            from: draft.from,
            to: draft.to,
            kind: draft.kind,
            project,
            subject: draft.subject,
            body: draft.body,
            at_ms: now_ms(),
            wakes: draft.kind.wakes(),
        };
        self.append(&path, &message)?;
        Ok(message)
    }

    /// Path of the JSONL mailbox of `role`.
    pub fn mailbox_path(&self, role: &Role) -> Result<PathBuf, RoutingError> {
        Ok(self
            .mailboxes
            .join(format!("{}.jsonl", self.role_file_stem(role)?)))
    }

    /// Unread messages of `role`, in delivery order.
    pub fn inbox(&self, role: &Role) -> Result<Vec<Message>, RoutingError> {
        role.validate()?;
        let _lock = self.lock_shared()?;
        let messages = self.read_messages(role)?;
        let cursor = self.read_cursor(role, messages.len())?;
        Ok(messages.into_iter().skip(cursor).collect())
    }

    /// Oldest unread message of `role`, without advancing the cursor.
    pub fn next_unread(&self, role: &Role) -> Result<Option<Message>, RoutingError> {
        role.validate()?;
        let _lock = self.lock_shared()?;
        let messages = self.read_messages(role)?;
        let cursor = self.read_cursor(role, messages.len())?;
        Ok(messages.get(cursor).cloned())
    }

    /// Marks every message currently in the mailbox as read.
    ///
    /// Returns how many messages the cursor advanced over.
    pub fn ack(&self, role: &Role) -> Result<usize, RoutingError> {
        role.validate()?;
        let _lock = self.lock_exclusive()?;
        let messages = self.read_messages(role)?;
        let cursor = self.read_cursor(role, messages.len())?;
        let acked = messages.len() - cursor;
        if acked > 0 {
            self.write_cursor(role, messages.len())?;
        }
        Ok(acked)
    }

    /// Marks messages through `message_id` as read without acknowledging later deliveries.
    ///
    /// This is useful when a consumer processes a snapshot while new messages may arrive.
    pub fn ack_through(&self, role: &Role, message_id: u64) -> Result<usize, RoutingError> {
        role.validate()?;
        let _lock = self.lock_exclusive()?;
        let messages = self.read_messages(role)?;
        let cursor = self.read_cursor(role, messages.len())?;
        let Some(index) = messages.iter().position(|message| message.id == message_id) else {
            return Ok(0);
        };
        let target = index + 1;
        if target <= cursor {
            return Ok(0);
        }
        self.write_cursor(role, target)?;
        Ok(target - cursor)
    }

    /// Whether the Lab Coordinator has an unread wake message.
    ///
    /// Only the Lab Coordinator's inbox has a wake notion; every other role
    /// answers `false`.
    pub fn pending_wake(&self, role: &Role) -> Result<bool, RoutingError> {
        Ok(self.pending_wake_count(role)? > 0)
    }

    /// Number of unread wake messages of `role`.
    ///
    /// Only the Lab Coordinator's inbox has a wake notion; every other role
    /// answers `0`.
    pub fn pending_wake_count(&self, role: &Role) -> Result<u64, RoutingError> {
        role.validate()?;
        if !matches!(role, Role::LabCoordinator) {
            return Ok(0);
        }
        let _lock = self.lock_shared()?;
        let messages = self.read_messages(role)?;
        let cursor = self.read_cursor(role, messages.len())?;
        let count = messages[cursor..]
            .iter()
            .filter(|message| message.wakes)
            .count();
        Ok(u64::try_from(count).unwrap_or(u64::MAX))
    }

    fn role_file_stem(&self, role: &Role) -> Result<String, RoutingError> {
        role.validate()?;
        let stem = role.file_stem();
        let unsafe_stem = stem.is_empty()
            || stem == "."
            || stem == ".."
            || stem.contains('/')
            || stem.contains('\\')
            || stem.contains('\0');
        if unsafe_stem {
            return Err(RoutingError::InvalidMailboxName { name: stem });
        }
        Ok(stem)
    }

    fn cursor_path(&self, role: &Role) -> Result<PathBuf, RoutingError> {
        Ok(self
            .cursors
            .join(format!("{}.cursor", self.role_file_stem(role)?)))
    }

    /// Reads every stored message of `role`; an unreadable line is an error.
    fn read_messages(&self, role: &Role) -> Result<Vec<Message>, RoutingError> {
        let path = self.mailbox_path(role)?;
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error(&path, source).into()),
        };

        let mut messages = Vec::new();
        for (index, line) in contents.lines().enumerate() {
            let line_number = index + 1;
            let corruption = |detail: String| RoutingError::CorruptMailbox {
                path: path.clone(),
                line: line_number,
                detail,
            };
            if line.is_empty() {
                return Err(corruption("empty line".to_string()));
            }
            let message = serde_json::from_str::<Message>(line)
                .map_err(|source| corruption(source.to_string()))?;
            message
                .from
                .validate()
                .map_err(|error| corruption(error.to_string()))?;
            message
                .to
                .validate()
                .map_err(|error| corruption(error.to_string()))?;
            if &message.to != role {
                return Err(corruption(format!(
                    "message for {} stored in the mailbox of {role}",
                    message.to
                )));
            }
            messages.push(message);
        }
        Ok(messages)
    }

    fn read_cursor(&self, role: &Role, message_count: usize) -> Result<usize, RoutingError> {
        let path = self.cursor_path(role)?;
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(source) if source.kind() == ErrorKind::NotFound => return Ok(0),
            Err(source) => return Err(io_error(&path, source).into()),
        };
        let cursor = raw
            .trim()
            .parse::<usize>()
            .map_err(|source| RoutingError::CorruptState {
                path: path.clone(),
                detail: source.to_string(),
            })?;
        if cursor > message_count {
            return Err(RoutingError::CorruptState {
                path,
                detail: format!(
                    "cursor at {cursor} is ahead of the {message_count} stored messages"
                ),
            });
        }
        Ok(cursor)
    }

    fn write_cursor(&self, role: &Role, cursor: usize) -> Result<(), RoutingError> {
        let path = self.cursor_path(role)?;
        write_atomic(&path, cursor.to_string().as_bytes())?;
        Ok(())
    }

    /// Allocates the next global message id; the caller holds the exclusive lock.
    fn next_id(&self) -> Result<u64, RoutingError> {
        let current = match fs::read_to_string(&self.sequence_path) {
            Ok(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|source| RoutingError::CorruptState {
                    path: self.sequence_path.clone(),
                    detail: source.to_string(),
                })?,
            Err(source) if source.kind() == ErrorKind::NotFound => 0,
            Err(source) => return Err(io_error(&self.sequence_path, source).into()),
        };
        let next = current
            .checked_add(1)
            .ok_or_else(|| RoutingError::CorruptState {
                path: self.sequence_path.clone(),
                detail: "message id counter is exhausted".to_string(),
            })?;
        write_atomic(&self.sequence_path, next.to_string().as_bytes())?;
        Ok(next)
    }

    /// Appends one JSON line; the caller holds the exclusive lock.
    fn append(&self, path: &Path, message: &Message) -> Result<(), RoutingError> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|source| io_error(path, source))?;
        file.write_all(&line)
            .map_err(|source| io_error(path, source))?;
        file.sync_all().map_err(|source| io_error(path, source))?;
        Ok(())
    }

    fn lock_exclusive(&self) -> Result<FileLock, RoutingError> {
        FileLock::acquire(&self.lock_path, libc::LOCK_EX).map_err(RoutingError::from)
    }

    fn lock_shared(&self) -> Result<FileLock, RoutingError> {
        FileLock::acquire(&self.lock_path, libc::LOCK_SH).map_err(RoutingError::from)
    }
}

fn validate_worker_id(id: &str) -> Result<(), RoutingError> {
    let valid = !id.is_empty()
        && id.len() <= MAX_WORKER_ID_LEN
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(RoutingError::InvalidWorkerId { id: id.to_string() })
    }
}

/// Refuses every route outside the lab's message matrix.
///
/// Also checks the project tag and returns the tag to store: a route that
/// belongs to a project rejects a foreign tag and fills in its own when the
/// tag is missing. `UserRequest` and `Report` keep whatever tag they carry,
/// because the user and the Lab span projects.
fn ensure_route(
    from: &Role,
    to: &Role,
    kind: MessageKind,
    project: Option<&ProjectId>,
) -> Result<Option<ProjectId>, RoutingError> {
    match (from, to, kind) {
        (Role::User, Role::LabCoordinator, MessageKind::UserRequest)
        | (Role::LabCoordinator, Role::User, MessageKind::Report) => Ok(project.cloned()),
        (Role::LabCoordinator, Role::CoCoordinator(coordinator), MessageKind::Demand) => {
            normalise_project(from, to, kind, project, coordinator)
        }
        (
            Role::CoCoordinator(coordinator),
            Role::Worker {
                project: worker_project,
                ..
            },
            MessageKind::Instruction,
        ) if coordinator == worker_project => {
            normalise_project(from, to, kind, project, coordinator)
        }
        (
            Role::Worker {
                project: worker_project,
                ..
            },
            Role::CoCoordinator(coordinator),
            MessageKind::WorkerResult,
        ) if worker_project == coordinator => {
            normalise_project(from, to, kind, project, worker_project)
        }
        (
            Role::CoCoordinator(coordinator),
            Role::LabCoordinator,
            MessageKind::Milestone | MessageKind::Failure | MessageKind::BudgetEvent,
        ) if project == Some(coordinator) => Ok(project.cloned()),
        _ => Err(not_allowed(from, to, kind)),
    }
}

/// Project tag to store for a message belonging to `expected`: a missing tag
/// becomes `Some(expected)`, a matching tag is kept and a foreign tag is
/// refused.
fn normalise_project(
    from: &Role,
    to: &Role,
    kind: MessageKind,
    project: Option<&ProjectId>,
    expected: &ProjectId,
) -> Result<Option<ProjectId>, RoutingError> {
    match project {
        None => Ok(Some(expected.clone())),
        Some(project) if project == expected => Ok(Some(project.clone())),
        Some(_) => Err(not_allowed(from, to, kind)),
    }
}

fn not_allowed(from: &Role, to: &Role, kind: MessageKind) -> RoutingError {
    RoutingError::NotAllowed {
        from: from.clone(),
        to: to.clone(),
        kind,
    }
}

/// Exclusive or shared advisory lock on an open lock file, released on drop.
struct FileLock {
    file: File,
}

impl FileLock {
    fn acquire(path: &Path, operation: libc::c_int) -> Result<Self, ModelError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| io_error(path, source))?;
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

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor stays open until this borrow ends.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
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

    let dir = File::open(parent).map_err(|source| io_error(parent, source))?;
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
    let mut file = File::create(path).map_err(|source| io_error(path, source))?;
    file.write_all(contents)
        .map_err(|source| io_error(path, source))?;
    file.sync_all().map_err(|source| io_error(path, source))
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
        fs,
        io::Write,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        thread,
    };

    use super::*;

    /// Temporary routing directory removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jgf-hub-routing-{}-{suffix}", std::process::id()));
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

    fn user() -> Role {
        Role::User
    }

    fn lab() -> Role {
        Role::LabCoordinator
    }

    fn co(raw: &str) -> Role {
        Role::CoCoordinator(project(raw))
    }

    fn worker_of(raw: &str, id: &str) -> Role {
        Role::worker(project(raw), id, WorkerKind::Coder).expect("valid worker")
    }

    fn open(root: &TempRoot) -> Router {
        Router::open(root.path()).expect("open router")
    }

    fn draft(from: Role, to: Role, kind: MessageKind) -> OutgoingMessage {
        OutgoingMessage::new(from, to, kind, "subject", "body")
    }

    fn expected_allowed(from: &Role, to: &Role, kind: MessageKind) -> bool {
        match (from, to, kind) {
            (Role::User, Role::LabCoordinator, MessageKind::UserRequest) => true,
            (Role::LabCoordinator, Role::User, MessageKind::Report) => true,
            (Role::LabCoordinator, Role::CoCoordinator(_), MessageKind::Demand) => true,
            (
                Role::CoCoordinator(coordinator),
                Role::Worker {
                    project: worker_project,
                    ..
                },
                MessageKind::Instruction,
            ) => coordinator == worker_project,
            (
                Role::Worker {
                    project: worker_project,
                    ..
                },
                Role::CoCoordinator(coordinator),
                MessageKind::WorkerResult,
            ) => worker_project == coordinator,
            (
                Role::CoCoordinator(_),
                Role::LabCoordinator,
                MessageKind::Milestone | MessageKind::Failure | MessageKind::BudgetEvent,
            ) => true,
            _ => false,
        }
    }

    #[test]
    fn every_allowed_route_is_delivered() {
        let root = TempRoot::new();
        let router = open(&root);

        let request = router
            .send(draft(user(), lab(), MessageKind::UserRequest))
            .expect("user request");
        assert_eq!(request.id, 1);
        assert!(request.wakes);

        let report = router
            .send(
                OutgoingMessage::new(lab(), user(), MessageKind::Report, "progress", "half done")
                    .with_project(project("alpha")),
            )
            .expect("report");
        assert!(!report.wakes);
        assert_eq!(report.project, Some(project("alpha")));

        router
            .send(draft(lab(), co("alpha"), MessageKind::Demand))
            .expect("demand");
        router
            .send(draft(
                co("alpha"),
                worker_of("alpha", "coder-1"),
                MessageKind::Instruction,
            ))
            .expect("instruction");
        router
            .send(draft(
                worker_of("alpha", "coder-1"),
                co("alpha"),
                MessageKind::WorkerResult,
            ))
            .expect("worker result");

        for kind in [
            MessageKind::Milestone,
            MessageKind::Failure,
            MessageKind::BudgetEvent,
        ] {
            let escalation = router
                .send(
                    OutgoingMessage::new(co("alpha"), lab(), kind, "subject", "body")
                        .with_project(project("alpha")),
                )
                .expect("escalation");
            assert!(escalation.wakes, "{kind:?} wakes the lab");
        }

        assert_eq!(router.inbox(&lab()).expect("lab inbox").len(), 4);
        assert_eq!(router.inbox(&user()).expect("user inbox").len(), 1);
        assert_eq!(router.inbox(&co("alpha")).expect("co inbox").len(), 2);
        assert_eq!(
            router
                .inbox(&worker_of("alpha", "coder-1"))
                .expect("worker inbox")
                .len(),
            1
        );
    }

    #[test]
    fn the_route_matrix_refuses_every_other_combination() {
        let root = TempRoot::new();
        let router = open(&root);
        let roles = [
            user(),
            lab(),
            co("alpha"),
            co("beta"),
            worker_of("alpha", "coder-1"),
        ];
        let kinds = [
            MessageKind::UserRequest,
            MessageKind::Report,
            MessageKind::Demand,
            MessageKind::WorkerResult,
            MessageKind::Instruction,
            MessageKind::Milestone,
            MessageKind::Failure,
            MessageKind::BudgetEvent,
        ];

        for from in &roles {
            let own_project = match from {
                Role::CoCoordinator(project) | Role::Worker { project, .. } => {
                    Some(project.clone())
                }
                Role::User | Role::LabCoordinator => None,
            };
            for to in &roles {
                for kind in kinds {
                    let mut message =
                        OutgoingMessage::new(from.clone(), to.clone(), kind, "subject", "body");
                    message.project = own_project.clone();
                    let result = router.send(message);
                    if expected_allowed(from, to, kind) {
                        assert!(
                            result.is_ok(),
                            "{from} -> {to} {kind:?} should be allowed: {result:?}"
                        );
                    } else {
                        assert!(
                            matches!(result, Err(RoutingError::NotAllowed { .. })),
                            "{from} -> {to} {kind:?} should be refused: {result:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cross_project_worker_and_coordinator_routes_are_refused() {
        let root = TempRoot::new();
        let router = open(&root);

        let cross_result = router.send(draft(
            worker_of("alpha", "coder-1"),
            co("beta"),
            MessageKind::WorkerResult,
        ));
        assert!(matches!(cross_result, Err(RoutingError::NotAllowed { .. })));

        let cross_instruction = router.send(draft(
            co("alpha"),
            worker_of("beta", "coder-1"),
            MessageKind::Instruction,
        ));
        assert!(matches!(
            cross_instruction,
            Err(RoutingError::NotAllowed { .. })
        ));

        let wrong_project = router.send(
            OutgoingMessage::new(
                co("alpha"),
                lab(),
                MessageKind::Milestone,
                "subject",
                "body",
            )
            .with_project(project("beta")),
        );
        assert!(matches!(
            wrong_project,
            Err(RoutingError::NotAllowed { .. })
        ));

        let missing_project = router.send(draft(co("alpha"), lab(), MessageKind::Failure));
        assert!(matches!(
            missing_project,
            Err(RoutingError::NotAllowed { .. })
        ));
    }

    #[test]
    fn every_project_tag_is_checked_and_normalised_or_refused() {
        let allowed = [
            (
                "worker result without a project",
                worker_of("alpha", "coder-1"),
                co("alpha"),
                MessageKind::WorkerResult,
                None,
                Some("alpha"),
            ),
            (
                "worker result with its own project",
                worker_of("alpha", "coder-1"),
                co("alpha"),
                MessageKind::WorkerResult,
                Some("alpha"),
                Some("alpha"),
            ),
            (
                "instruction without a project",
                co("alpha"),
                worker_of("alpha", "coder-1"),
                MessageKind::Instruction,
                None,
                Some("alpha"),
            ),
            (
                "instruction with its own project",
                co("alpha"),
                worker_of("alpha", "coder-1"),
                MessageKind::Instruction,
                Some("alpha"),
                Some("alpha"),
            ),
            (
                "demand without a project",
                lab(),
                co("alpha"),
                MessageKind::Demand,
                None,
                Some("alpha"),
            ),
            (
                "demand with the target project",
                lab(),
                co("alpha"),
                MessageKind::Demand,
                Some("alpha"),
                Some("alpha"),
            ),
            (
                "milestone with its own project",
                co("alpha"),
                lab(),
                MessageKind::Milestone,
                Some("alpha"),
                Some("alpha"),
            ),
            (
                "failure with its own project",
                co("alpha"),
                lab(),
                MessageKind::Failure,
                Some("alpha"),
                Some("alpha"),
            ),
            (
                "budget event with its own project",
                co("alpha"),
                lab(),
                MessageKind::BudgetEvent,
                Some("alpha"),
                Some("alpha"),
            ),
            (
                "user request without a project",
                user(),
                lab(),
                MessageKind::UserRequest,
                None,
                None,
            ),
            (
                "user request with any project",
                user(),
                lab(),
                MessageKind::UserRequest,
                Some("beta"),
                Some("beta"),
            ),
            (
                "report without a project",
                lab(),
                user(),
                MessageKind::Report,
                None,
                None,
            ),
            (
                "report with any project",
                lab(),
                user(),
                MessageKind::Report,
                Some("beta"),
                Some("beta"),
            ),
        ];

        for (label, from, to, kind, tag, stored) in allowed {
            let root = TempRoot::new();
            let router = open(&root);
            let mut message =
                OutgoingMessage::new(from.clone(), to.clone(), kind, "subject", "body");
            message.project = tag.map(project);

            let sent = router.send(message).unwrap_or_else(|error| {
                panic!("{label}: {from} -> {to} {kind:?} should be allowed: {error}")
            });
            assert_eq!(sent.project, stored.map(project), "{label}");
        }

        let refused = [
            (
                "worker result tagged with another project",
                worker_of("alpha", "coder-1"),
                co("alpha"),
                MessageKind::WorkerResult,
                Some("beta"),
            ),
            (
                "worker result sent to another project's co-coordinator",
                worker_of("alpha", "coder-1"),
                co("beta"),
                MessageKind::WorkerResult,
                Some("beta"),
            ),
            (
                "instruction tagged with another project",
                co("alpha"),
                worker_of("alpha", "coder-1"),
                MessageKind::Instruction,
                Some("beta"),
            ),
            (
                "instruction sent to another project's worker",
                co("alpha"),
                worker_of("beta", "coder-1"),
                MessageKind::Instruction,
                Some("beta"),
            ),
            (
                "demand tagged with another project",
                lab(),
                co("alpha"),
                MessageKind::Demand,
                Some("beta"),
            ),
            (
                "milestone without a project",
                co("alpha"),
                lab(),
                MessageKind::Milestone,
                None,
            ),
            (
                "failure tagged with another project",
                co("alpha"),
                lab(),
                MessageKind::Failure,
                Some("beta"),
            ),
            (
                "budget event tagged with another project",
                co("alpha"),
                lab(),
                MessageKind::BudgetEvent,
                Some("beta"),
            ),
        ];

        for (label, from, to, kind, tag) in refused {
            let root = TempRoot::new();
            let router = open(&root);
            let mut message =
                OutgoingMessage::new(from.clone(), to.clone(), kind, "subject", "body");
            message.project = tag.map(project);

            let result = router.send(message);
            assert!(
                matches!(result, Err(RoutingError::NotAllowed { .. })),
                "{label}: {from} -> {to} {kind:?} should be refused: {result:?}"
            );
            assert!(
                router.inbox(&to).expect("inbox").is_empty(),
                "{label}: a refused message must not be stored"
            );
        }
    }

    #[test]
    fn the_router_sets_the_wake_flag_and_ignores_the_sender() {
        let root = TempRoot::new();
        let router = open(&root);

        let request = router
            .send(
                OutgoingMessage::new(user(), lab(), MessageKind::UserRequest, "subject", "body")
                    .with_wakes(false),
            )
            .expect("user request");
        assert!(request.wakes);

        let report = router
            .send(
                OutgoingMessage::new(lab(), user(), MessageKind::Report, "subject", "body")
                    .with_wakes(true),
            )
            .expect("report");
        assert!(!report.wakes);

        for kind in [
            MessageKind::Milestone,
            MessageKind::Failure,
            MessageKind::BudgetEvent,
        ] {
            let escalation = router
                .send(
                    OutgoingMessage::new(co("alpha"), lab(), kind, "subject", "body")
                        .with_project(project("alpha"))
                        .with_wakes(false),
                )
                .expect("escalation");
            assert!(escalation.wakes, "{kind:?} wakes the lab");
        }

        for (from, to, kind) in [
            (lab(), co("alpha"), MessageKind::Demand),
            (
                co("alpha"),
                worker_of("alpha", "coder-1"),
                MessageKind::Instruction,
            ),
            (
                worker_of("alpha", "coder-1"),
                co("alpha"),
                MessageKind::WorkerResult,
            ),
        ] {
            let routine = router
                .send(draft(from, to, kind).with_wakes(true))
                .expect("routine message");
            assert!(!routine.wakes, "{kind:?} must not wake the lab");
        }

        assert!(router.pending_wake(&lab()).expect("pending wake"));
        assert_eq!(
            router
                .pending_wake_count(&lab())
                .expect("pending wake count"),
            4
        );
        assert!(!router.pending_wake(&user()).expect("user wake"));
        assert!(
            !router
                .pending_wake(&co("alpha"))
                .expect("co-coordinator wake")
        );
    }

    #[test]
    fn routine_worker_results_never_reach_the_lab() {
        let root = TempRoot::new();
        let router = open(&root);

        for _ in 0..3 {
            router
                .send(draft(
                    worker_of("alpha", "coder-1"),
                    co("alpha"),
                    MessageKind::WorkerResult,
                ))
                .expect("worker result");
        }

        assert!(router.inbox(&lab()).expect("lab inbox").is_empty());
        assert!(!router.pending_wake(&lab()).expect("pending wake"));
        assert_eq!(router.pending_wake_count(&lab()).expect("wake count"), 0);
        assert_eq!(router.inbox(&co("alpha")).expect("co inbox").len(), 3);
    }

    #[test]
    fn message_ids_are_monotonic_across_routers_and_recipients() {
        let root = TempRoot::new();
        let first = open(&root);
        let second = open(&root);

        let routes = [
            (user(), lab(), MessageKind::UserRequest),
            (lab(), user(), MessageKind::Report),
            (lab(), co("alpha"), MessageKind::Demand),
            (
                co("alpha"),
                worker_of("alpha", "coder-1"),
                MessageKind::Instruction,
            ),
            (
                worker_of("alpha", "coder-1"),
                co("alpha"),
                MessageKind::WorkerResult,
            ),
        ];
        for (index, (from, to, kind)) in routes.into_iter().enumerate() {
            let router = if index % 2 == 0 { &first } else { &second };
            let message = router.send(draft(from, to, kind)).expect("deliver");
            assert_eq!(message.id, index as u64 + 1);
        }

        let reopened = open(&root);
        assert_eq!(
            reopened
                .inbox(&co("alpha"))
                .expect("co inbox")
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [3, 5]
        );
    }

    #[test]
    fn concurrent_senders_get_unique_consecutive_ids() {
        const THREADS: usize = 8;
        const MESSAGES_PER_THREAD: usize = 8;

        let root = TempRoot::new();
        let first = Arc::new(open(&root));
        let second = Arc::new(open(&root));

        let senders: Vec<_> = (0..THREADS)
            .map(|thread_index| {
                let router = if thread_index % 2 == 0 {
                    Arc::clone(&first)
                } else {
                    Arc::clone(&second)
                };
                thread::spawn(move || {
                    (0..MESSAGES_PER_THREAD)
                        .map(|_| {
                            router
                                .send(draft(user(), lab(), MessageKind::UserRequest))
                                .expect("send")
                                .id
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        let mut ids: Vec<u64> = senders
            .into_iter()
            .flat_map(|handle| handle.join().expect("join sender"))
            .collect();
        ids.sort_unstable();
        let total = (THREADS * MESSAGES_PER_THREAD) as u64;
        assert_eq!(ids, (1..=total).collect::<Vec<_>>());

        let inbox = first.inbox(&lab()).expect("lab inbox");
        assert_eq!(inbox.len(), THREADS * MESSAGES_PER_THREAD);
        assert!(
            inbox.windows(2).all(|pair| pair[0].id < pair[1].id),
            "delivery order follows the ids"
        );
    }

    #[test]
    fn the_read_cursor_survives_a_reopen() {
        let root = TempRoot::new();
        let router = open(&root);

        let first = router
            .send(draft(user(), lab(), MessageKind::UserRequest))
            .expect("first");
        let second = router
            .send(draft(user(), lab(), MessageKind::UserRequest))
            .expect("second");

        assert_eq!(
            router
                .inbox(&lab())
                .expect("inbox")
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [first.id, second.id]
        );
        assert_eq!(
            router
                .next_unread(&lab())
                .expect("next unread")
                .map(|message| message.id),
            Some(first.id)
        );
        assert_eq!(router.ack(&lab()).expect("ack"), 2);
        assert!(router.inbox(&lab()).expect("inbox").is_empty());
        assert!(router.next_unread(&lab()).expect("next unread").is_none());

        let reopened = open(&root);
        assert!(reopened.inbox(&lab()).expect("inbox").is_empty());
        assert_eq!(reopened.ack(&lab()).expect("ack"), 0);
        let third = reopened
            .send(draft(user(), lab(), MessageKind::UserRequest))
            .expect("third");
        assert_eq!(third.id, 3);
        assert_eq!(reopened.inbox(&lab()).expect("inbox")[0].id, third.id);
    }

    #[test]
    fn ack_through_preserves_messages_delivered_after_the_snapshot() {
        let root = TempRoot::new();
        let router = open(&root);
        router
            .send(draft(user(), lab(), MessageKind::UserRequest))
            .expect("first");
        let second = router
            .send(draft(user(), lab(), MessageKind::UserRequest))
            .expect("second");
        let snapshot_last_id = second.id;

        let third = router
            .send(draft(user(), lab(), MessageKind::UserRequest))
            .expect("third");

        assert_eq!(
            router.ack_through(&lab(), snapshot_last_id).expect("ack"),
            2
        );
        assert_eq!(
            router
                .inbox(&lab())
                .expect("unread after snapshot ack")
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [third.id]
        );
    }

    #[test]
    fn a_corrupt_mailbox_line_is_an_error() {
        let root = TempRoot::new();
        let router = open(&root);
        router
            .send(draft(user(), lab(), MessageKind::UserRequest))
            .expect("send");

        let path = router.mailbox_path(&lab()).expect("mailbox path");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open mailbox");
        file.write_all(b"not json\n").expect("append corrupt line");
        file.sync_all().expect("sync mailbox");

        assert!(matches!(
            router.inbox(&lab()),
            Err(RoutingError::CorruptMailbox { line: 2, .. })
        ));
        assert!(matches!(
            router.next_unread(&lab()),
            Err(RoutingError::CorruptMailbox { line: 2, .. })
        ));
    }

    #[test]
    fn hostile_ids_are_rejected_and_never_escape_the_mailbox_directory() {
        let root = TempRoot::new();
        let router = open(&root);

        let huge_worker = "a".repeat(MAX_WORKER_ID_LEN + 1);
        let hostile_workers = [
            "../x",
            "a/b",
            "a\\b",
            "a\0b",
            huge_worker.as_str(),
            "",
            "A1",
        ];
        for id in hostile_workers {
            let result = Role::worker(project("alpha"), id, WorkerKind::Coder);
            assert!(
                matches!(result, Err(RoutingError::InvalidWorkerId { .. })),
                "worker id {id:?} must be rejected"
            );
        }

        let bypassed = Role::Worker {
            project: project("alpha"),
            id: "../escape".to_string(),
            kind: WorkerKind::Coder,
        };
        assert!(matches!(
            router.mailbox_path(&bypassed),
            Err(RoutingError::InvalidWorkerId { .. })
        ));
        assert!(matches!(
            router.send(draft(bypassed, co("alpha"), MessageKind::WorkerResult,)),
            Err(RoutingError::InvalidWorkerId { .. })
        ));

        let huge_project = "a".repeat(41);
        let hostile_projects = [
            "../x",
            "a/b",
            "a\\b",
            "a\0b",
            huge_project.as_str(),
            "",
            "A1",
        ];
        for raw in hostile_projects {
            assert!(
                ProjectId::parse(raw).is_err(),
                "project id {raw:?} must be rejected"
            );
        }

        let mailboxes = router.dir().join(MAILBOXES_DIR);
        for role in [user(), lab(), co("alpha"), worker_of("alpha", "coder-1")] {
            let path = router.mailbox_path(&role).expect("safe mailbox path");
            assert!(
                path.starts_with(&mailboxes),
                "{} is outside {}",
                path.display(),
                mailboxes.display()
            );
        }
    }

    #[test]
    fn bodies_over_the_cap_are_refused() {
        let root = TempRoot::new();
        let router = open(&root);

        let at_cap = "x".repeat(MAX_BODY_BYTES);
        let message = router
            .send(OutgoingMessage::new(
                user(),
                lab(),
                MessageKind::UserRequest,
                "subject",
                at_cap,
            ))
            .expect("body at the cap");
        assert_eq!(message.body.len(), MAX_BODY_BYTES);

        let oversized = "x".repeat(MAX_BODY_BYTES + 1);
        let error = router
            .send(OutgoingMessage::new(
                user(),
                lab(),
                MessageKind::UserRequest,
                "subject",
                oversized,
            ))
            .expect_err("body over the cap");
        match error {
            RoutingError::BodyTooLarge { len, max } => {
                assert_eq!(len, MAX_BODY_BYTES + 1);
                assert_eq!(max, MAX_BODY_BYTES);
            }
            other => panic!("expected BodyTooLarge, got {other:?}"),
        }

        assert_eq!(router.inbox(&lab()).expect("lab inbox").len(), 1);
    }

    #[test]
    fn coordinator_configs_are_validated_and_round_trip() {
        let config = CoordinatorConfig::new(project("alpha"), "gemini", Some("flash".to_string()))
            .expect("valid config");
        let json = serde_json::to_string(&config).expect("serialize config");
        let back: CoordinatorConfig = serde_json::from_str(&json).expect("deserialize config");
        assert_eq!(config, back);

        assert!(CoordinatorConfig::new(project("alpha"), "  ", None).is_err());
        assert!(CoordinatorConfig::new(project("alpha"), "gemini", Some(" ".to_string())).is_err());

        let empty_harness: Result<CoordinatorConfig, _> =
            serde_json::from_str(r#"{"project":"alpha","harness":"  "}"#);
        assert!(empty_harness.is_err());
        let empty_model: Result<CoordinatorConfig, _> =
            serde_json::from_str(r#"{"project":"alpha","harness":"gemini","model":""}"#);
        assert!(empty_model.is_err());

        let without_model: CoordinatorConfig =
            serde_json::from_str(r#"{"project":"alpha","harness":"gemini"}"#)
                .expect("model is optional");
        assert_eq!(without_model.model, None);
    }
}
