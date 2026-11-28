//! Host-side unix-socket message surface for roles running in a lab sandbox.
//!
//! Every request carries an opaque launch token, but never a sender role. The
//! host resolves the sender from its token registry before calling [`Router`].

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use jgf_core::ProjectId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::routing::{Message, MessageKind, OutgoingMessage, Role, Router, RoutingError};

const MAX_REQUEST_BYTES: u64 = 128 * 1024;
// The sandbox maps its host owner to SANDBOX_UID, so lab roles can connect as
// the socket owner without granting access to other users.
const SOCKET_MODE: u32 = 0o600;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Host-generated per-launch capability. Its debug representation is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchToken(String);

impl LaunchToken {
    /// Exposes the token for the role's private process environment.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_raw(token: String) -> Self {
        Self(token)
    }

    pub(crate) fn generate() -> io::Result<Self> {
        random_token().map(Self)
    }
}

impl std::fmt::Debug for LaunchToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LaunchToken([redacted])")
    }
}

/// Typed error returned by the shim protocol.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub enum ShimError {
    /// The provided launch token is unknown or has been revoked.
    #[error("unknown or revoked launch token")]
    UnknownToken,

    /// The host router refused the requested route.
    #[error("message {kind:?} from {from} to {to} is not allowed")]
    NotAllowed {
        /// Sender resolved from the token.
        from: Role,
        /// Requested recipient.
        to: Role,
        /// Requested kind.
        kind: MessageKind,
    },

    /// The submitted message exceeds the router's body limit.
    #[error("message body is {len} bytes; the limit is {max}")]
    BodyTooLarge {
        /// Body size.
        len: usize,
        /// Maximum permitted body size.
        max: usize,
    },

    /// The wire request was malformed or attempted to include unsupported fields.
    #[error("invalid shim request")]
    InvalidRequest,

    /// The Lab Coordinator mailbox is acknowledged by its host supervisor.
    #[error("the Lab Coordinator mailbox is acknowledged by the host supervisor")]
    HostAcknowledgementRequired,

    /// Only the Lab Coordinator may enqueue a project.
    #[error("only the Lab Coordinator may enqueue projects")]
    LabCoordinatorRequired,

    /// This shim was not configured with the lab scheduler.
    #[error("the lab scheduler is unavailable")]
    SchedulerUnavailable,

    /// The scheduler refused the project request.
    #[error("scheduler request failed: {message}")]
    Scheduler {
        /// Safe scheduler error description; never contains a launch token.
        message: String,
    },

    /// A non-route router error occurred.
    #[error("routing failed: {message}")]
    Routing {
        /// Safe router error description; never contains a launch token.
        message: String,
    },
}

/// An audit event emitted for accepted hub actions and refused requests.
///
/// Launch tokens and message bodies are intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ShimAuditEvent {
    /// A message was stored successfully.
    Sent {
        /// Resolved sender.
        from: Role,
        /// Recipient.
        to: Role,
        /// Message kind.
        kind: MessageKind,
        /// Stored message id.
        message_id: u64,
    },
    /// A request was refused.
    Refused {
        /// Sender, if the request had a valid token.
        from: Option<Role>,
        /// Recipient, if one could be parsed.
        to: Option<Role>,
        /// Kind, if one could be parsed.
        kind: Option<MessageKind>,
        /// Stable typed error code.
        error: ShimError,
    },
    /// A project queue request was accepted.
    ProjectQueued {
        /// Project requested by the Lab Coordinator.
        project: ProjectId,
        /// Whether the project was newly appended to the queue.
        added: bool,
    },
}

impl ShimAuditEvent {
    /// Short audit-log representation with no token or message content.
    pub fn audit_summary(&self) -> String {
        match self {
            Self::Sent {
                from,
                to,
                kind,
                message_id,
            } => format!("hub_sent:{from}->{to}:{kind:?}:id={message_id}"),
            Self::Refused {
                from,
                to,
                kind,
                error,
            } => format!(
                "hub_refused:{}:{}:{:?}:{error}",
                from.as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "unknown".to_string()),
                to.as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "unknown".to_string()),
                kind
            ),
            Self::ProjectQueued { project, added } => {
                format!("hub_project_queued:{project}:added={added}")
            }
        }
    }
}

type AuditCallback = Arc<dyn Fn(ShimAuditEvent) + Send + Sync + 'static>;

struct ShimState {
    router: Router,
    tokens: Mutex<HashMap<String, Role>>,
    scheduler: Option<crate::scheduler::Scheduler>,
    audit: AuditCallback,
}

/// Unix-socket server owning the token registry and serving one lab's router.
pub struct HubShim {
    socket_path: PathBuf,
    state: Arc<ShimState>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HubShim {
    /// Binds a socket and starts serving line-delimited JSON requests.
    pub fn start(
        socket_path: impl Into<PathBuf>,
        router: Router,
        audit: impl Fn(ShimAuditEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Self::start_inner(socket_path.into(), router, None, audit)
    }

    /// Binds a socket and serves message and Lab Coordinator scheduler requests.
    pub fn start_with_scheduler(
        socket_path: impl Into<PathBuf>,
        router: Router,
        scheduler: crate::scheduler::Scheduler,
        audit: impl Fn(ShimAuditEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Self::start_inner(socket_path.into(), router, Some(scheduler), audit)
    }

    fn start_inner(
        socket_path: PathBuf,
        router: Router,
        scheduler: Option<crate::scheduler::Scheduler>,
        audit: impl Fn(ShimAuditEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)?;
        }
        remove_stale_socket(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        if let Err(error) =
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(SOCKET_MODE))
        {
            drop(listener);
            let _ = fs::remove_file(&socket_path);
            return Err(error);
        }
        if let Err(error) = listener.set_nonblocking(true) {
            drop(listener);
            let _ = fs::remove_file(&socket_path);
            return Err(error);
        }

        let state = Arc::new(ShimState {
            router,
            tokens: Mutex::new(HashMap::new()),
            scheduler,
            audit: Arc::new(audit),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let server_state = Arc::clone(&state);
        let server_stop = Arc::clone(&stop);
        let thread = match thread::Builder::new()
            .name("jgf-hub-shim".to_string())
            .spawn(move || serve(listener, server_state, server_stop))
        {
            Ok(thread) => thread,
            Err(error) => {
                let _ = fs::remove_file(&socket_path);
                return Err(error);
            }
        };

        Ok(Self {
            socket_path,
            state,
            stop,
            thread: Some(thread),
        })
    }

    /// Host socket path to expose to the role process.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Assigns a fresh unguessable token to a host-selected role.
    pub fn register_role(&self, role: Role) -> Result<LaunchToken, ShimError> {
        role.validate().map_err(map_routing_error)?;
        let mut tokens = self.state.tokens.lock().map_err(|_| ShimError::Routing {
            message: "token registry is unavailable".to_string(),
        })?;
        loop {
            let token = LaunchToken::generate().map_err(|error| ShimError::Routing {
                message: format!("could not generate launch token: {error}"),
            })?;
            if let std::collections::hash_map::Entry::Vacant(entry) =
                tokens.entry(token.expose().to_string())
            {
                entry.insert(role);
                return Ok(token);
            }
        }
    }

    /// Revokes a launch token. Once revoked, it cannot access the router.
    pub fn revoke(&self, token: &LaunchToken) -> bool {
        self.state
            .tokens
            .lock()
            .map(|mut tokens| tokens.remove(token.expose()).is_some())
            .unwrap_or(false)
    }
}

impl Drop for HubShim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// Small in-lab client that authenticates using the launch environment.
pub struct HubClient {
    socket_path: PathBuf,
    token: LaunchToken,
}

impl std::fmt::Debug for HubClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HubClient")
            .field("socket_path", &self.socket_path)
            .field("token", &"[redacted]")
            .finish()
    }
}

impl HubClient {
    /// Reads `JG_HUB_SOCKET` and `JG_HUB_TOKEN` from the current process.
    pub fn from_env() -> Result<Self, HubClientError> {
        let socket_path = std::env::var_os("JG_HUB_SOCKET")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(HubClientError::MissingSocket)?;
        let token = std::env::var("JG_HUB_TOKEN").map_err(|_| HubClientError::MissingToken)?;
        if token.is_empty() {
            return Err(HubClientError::MissingToken);
        }
        Ok(Self {
            socket_path,
            token: LaunchToken(token),
        })
    }

    /// Creates a client with explicit credentials, useful for host-side tests.
    pub fn new(socket_path: impl Into<PathBuf>, token: LaunchToken) -> Self {
        Self {
            socket_path: socket_path.into(),
            token,
        }
    }

    /// Sends a message without supplying a sender identity.
    pub fn send(
        &self,
        to: Role,
        kind: MessageKind,
        project: Option<ProjectId>,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<Message, HubClientError> {
        match self.request(WireRequest::Send {
            token: self.token.expose().to_string(),
            to,
            kind,
            project,
            subject: subject.into(),
            body: body.into(),
        })? {
            WireOutput::Sent(message) => Ok(message),
            _ => Err(HubClientError::Protocol),
        }
    }

    /// Reads all unread messages for this token's role.
    pub fn inbox(&self) -> Result<Vec<Message>, HubClientError> {
        match self.request(WireRequest::Inbox {
            token: self.token.expose().to_string(),
        })? {
            WireOutput::Inbox(messages) => Ok(messages),
            _ => Err(HubClientError::Protocol),
        }
    }

    /// Reads the oldest unread message without advancing the cursor.
    pub fn next(&self) -> Result<Option<Message>, HubClientError> {
        match self.request(WireRequest::Next {
            token: self.token.expose().to_string(),
        })? {
            WireOutput::Next(message) => Ok(message),
            _ => Err(HubClientError::Protocol),
        }
    }

    /// Acknowledges all messages currently in the inbox.
    pub fn ack(&self) -> Result<usize, HubClientError> {
        match self.request(WireRequest::Ack {
            token: self.token.expose().to_string(),
        })? {
            WireOutput::Ack { count } => Ok(count),
            _ => Err(HubClientError::Protocol),
        }
    }

    /// Enqueues a project through the host-owned scheduler.
    pub fn enqueue(&self, project: ProjectId) -> Result<bool, HubClientError> {
        match self.request(WireRequest::Enqueue {
            token: self.token.expose().to_string(),
            project,
        })? {
            WireOutput::Enqueued { added } => Ok(added),
            _ => Err(HubClientError::Protocol),
        }
    }

    fn request(&self, request: WireRequest) -> Result<WireOutput, HubClientError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|error| HubClientError::Socket(error.to_string()))?;
        serde_json::to_writer(&mut stream, &request).map_err(|_| HubClientError::Protocol)?;
        stream
            .write_all(b"\n")
            .map_err(|error| HubClientError::Socket(error.to_string()))?;
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .map_err(|error| HubClientError::Socket(error.to_string()))?;
        if response.is_empty() {
            return Err(HubClientError::Protocol);
        }
        match serde_json::from_str::<WireResponse>(&response)
            .map_err(|_| HubClientError::Protocol)?
        {
            WireResponse::Ok { output } => Ok(output),
            WireResponse::Error { error } => Err(HubClientError::Shim(error)),
        }
    }
}

/// Errors raised by [`HubClient`].
#[derive(Debug, Error)]
pub enum HubClientError {
    /// `JG_HUB_SOCKET` is missing or empty.
    #[error("JG_HUB_SOCKET is missing")]
    MissingSocket,
    /// `JG_HUB_TOKEN` is missing or empty.
    #[error("JG_HUB_TOKEN is missing")]
    MissingToken,
    /// The socket could not be reached.
    #[error("hub socket request failed: {0}")]
    Socket(String),
    /// The peer returned an invalid response.
    #[error("invalid hub shim response")]
    Protocol,
    /// The shim refused the request.
    #[error(transparent)]
    Shim(#[from] ShimError),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum WireRequest {
    Send {
        token: String,
        to: Role,
        kind: MessageKind,
        #[serde(default)]
        project: Option<ProjectId>,
        subject: String,
        body: String,
    },
    Inbox {
        token: String,
    },
    Next {
        token: String,
    },
    Ack {
        token: String,
    },
    Enqueue {
        token: String,
        project: ProjectId,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WireResponse {
    Ok { output: WireOutput },
    Error { error: ShimError },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
enum WireOutput {
    Sent(Message),
    Inbox(Vec<Message>),
    Next(Option<Message>),
    Ack { count: usize },
    Enqueued { added: bool },
}

fn serve(listener: UnixListener, state: Arc<ShimState>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => handle_connection(stream, &state),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn handle_connection(mut stream: UnixStream, state: &ShimState) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    let read = match (&mut BufReader::new(&stream))
        .take(MAX_REQUEST_BYTES + 1)
        .read_line(&mut line)
    {
        Ok(read) => read,
        Err(_) => return,
    };
    let response = if read == 0 || read as u64 > MAX_REQUEST_BYTES || !line.ends_with('\n') {
        audit_refusal(state, None, None, None, ShimError::InvalidRequest);
        WireResponse::Error {
            error: ShimError::InvalidRequest,
        }
    } else {
        match serde_json::from_str::<WireRequest>(&line) {
            Ok(request) => dispatch(state, request),
            Err(_) => {
                audit_refusal(state, None, None, None, ShimError::InvalidRequest);
                WireResponse::Error {
                    error: ShimError::InvalidRequest,
                }
            }
        }
    };
    let _ = serde_json::to_writer(&mut stream, &response);
    let _ = stream.write_all(b"\n");
}

fn dispatch(state: &ShimState, request: WireRequest) -> WireResponse {
    match request {
        WireRequest::Send {
            token,
            to,
            kind,
            project,
            subject,
            body,
        } => {
            let from = match resolve_role(state, &token) {
                Ok(from) => from,
                Err(error) => {
                    audit_refusal(state, None, Some(to), Some(kind), error.clone());
                    return WireResponse::Error { error };
                }
            };
            let draft = OutgoingMessage {
                from: from.clone(),
                to: to.clone(),
                kind,
                project,
                subject,
                body,
                wakes: false,
            };
            match state.router.send(draft) {
                Ok(message) => {
                    (state.audit)(ShimAuditEvent::Sent {
                        from,
                        to,
                        kind,
                        message_id: message.id,
                    });
                    WireResponse::Ok {
                        output: WireOutput::Sent(message),
                    }
                }
                Err(route_error) => {
                    let error = map_routing_error(route_error);
                    audit_refusal(state, Some(from), Some(to), Some(kind), error.clone());
                    WireResponse::Error { error }
                }
            }
        }
        WireRequest::Inbox { token } => match resolve_role(state, &token)
            .and_then(|role| state.router.inbox(&role).map_err(map_routing_error))
        {
            Ok(messages) => WireResponse::Ok {
                output: WireOutput::Inbox(messages),
            },
            Err(error) => {
                audit_refusal(state, None, None, None, error.clone());
                WireResponse::Error { error }
            }
        },
        WireRequest::Next { token } => match resolve_role(state, &token)
            .and_then(|role| state.router.next_unread(&role).map_err(map_routing_error))
        {
            Ok(message) => WireResponse::Ok {
                output: WireOutput::Next(message),
            },
            Err(error) => {
                audit_refusal(state, None, None, None, error.clone());
                WireResponse::Error { error }
            }
        },
        WireRequest::Ack { token } => match resolve_role(state, &token).and_then(|role| {
            if role == Role::LabCoordinator {
                return Err(ShimError::HostAcknowledgementRequired);
            }
            state.router.ack(&role).map_err(map_routing_error)
        }) {
            Ok(count) => WireResponse::Ok {
                output: WireOutput::Ack { count },
            },
            Err(error) => {
                audit_refusal(state, None, None, None, error.clone());
                WireResponse::Error { error }
            }
        },
        WireRequest::Enqueue { token, project } => {
            let role = match resolve_role(state, &token) {
                Ok(role) => role,
                Err(error) => {
                    audit_refusal(state, None, None, None, error.clone());
                    return WireResponse::Error { error };
                }
            };
            if role != Role::LabCoordinator {
                let error = ShimError::LabCoordinatorRequired;
                audit_refusal(state, Some(role), None, None, error.clone());
                return WireResponse::Error { error };
            }
            let Some(scheduler) = state.scheduler.as_ref() else {
                let error = ShimError::SchedulerUnavailable;
                audit_refusal(state, Some(role), None, None, error.clone());
                return WireResponse::Error { error };
            };
            match scheduler.enqueue(project.clone()) {
                Ok(added) => {
                    (state.audit)(ShimAuditEvent::ProjectQueued { project, added });
                    WireResponse::Ok {
                        output: WireOutput::Enqueued { added },
                    }
                }
                Err(error) => {
                    let error = ShimError::Scheduler {
                        message: error.to_string(),
                    };
                    audit_refusal(state, Some(role), None, None, error.clone());
                    WireResponse::Error { error }
                }
            }
        }
    }
}

fn resolve_role(state: &ShimState, token: &str) -> Result<Role, ShimError> {
    state
        .tokens
        .lock()
        .map_err(|_| ShimError::Routing {
            message: "token registry is unavailable".to_string(),
        })?
        .get(token)
        .cloned()
        .ok_or(ShimError::UnknownToken)
}

fn audit_refusal(
    state: &ShimState,
    from: Option<Role>,
    to: Option<Role>,
    kind: Option<MessageKind>,
    error: ShimError,
) {
    (state.audit)(ShimAuditEvent::Refused {
        from,
        to,
        kind,
        error,
    });
}

fn map_routing_error(error: RoutingError) -> ShimError {
    match error {
        RoutingError::NotAllowed { from, to, kind } => ShimError::NotAllowed { from, to, kind },
        RoutingError::BodyTooLarge { len, max } => ShimError::BodyTooLarge { len, max },
        error => ShimError::Routing {
            message: error.to_string(),
        },
    }
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    OpenOptions::new()
        .read(true)
        .open("/dev/urandom")?
        .read_exact(&mut bytes)?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing into a string cannot fail");
    }
    Ok(token)
}

/// Removes a socket file left behind by a shim that died without cleanup.
///
/// A unix socket path that still exists makes `bind` fail with
/// `AddrInUse`, so a crashed run would block every later start. The path is
/// removed only when it is a socket nobody accepts on; a live listener or a
/// non-socket file is reported as `AddrInUse` and left untouched.
fn remove_stale_socket(socket_path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let in_use = |reason: &str| {
        io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{}: {reason}", socket_path.display()),
        )
    };
    if !metadata.file_type().is_socket() {
        return Err(in_use("path exists and is not a socket"));
    }
    match UnixStream::connect(socket_path) {
        Ok(_) => Err(in_use("another hub shim is listening")),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            fs::remove_file(socket_path)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn project(value: &str) -> ProjectId {
        ProjectId::parse(value).expect("project id")
    }

    fn worker(project_id: &str, id: &str) -> Role {
        Role::worker(project(project_id), id, crate::routing::WorkerKind::Coder).expect("worker")
    }

    fn client(shim: &HubShim, token: LaunchToken) -> HubClient {
        HubClient::new(shim.socket_path(), token)
    }

    #[test]
    fn scheduler_enabled_shim_supports_mail_and_coordinator_enqueue() {
        let temp = tempfile::tempdir().expect("tempdir");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let scheduler =
            crate::scheduler::Scheduler::open(temp.path().join("scheduler")).expect("scheduler");
        let events = Arc::new(Mutex::new(Vec::new()));
        let audit_events = Arc::clone(&events);
        let shim = HubShim::start_with_scheduler(
            temp.path().join("hub.sock"),
            router,
            scheduler,
            move |event| audit_events.lock().expect("events").push(event),
        )
        .expect("shim");
        let coordinator = client(
            &shim,
            shim.register_role(Role::LabCoordinator)
                .expect("coordinator token"),
        );
        let user = client(&shim, shim.register_role(Role::User).expect("user token"));

        let sent = coordinator
            .send(
                Role::User,
                MessageKind::Report,
                None,
                "Progress",
                "Project queued.",
            )
            .expect("send report");
        assert_eq!(user.next().expect("next message"), Some(sent.clone()));
        assert_eq!(user.inbox().expect("inbox"), vec![sent]);
        assert_eq!(user.ack().expect("acknowledge"), 1);
        assert!(user.inbox().expect("empty inbox").is_empty());

        assert!(coordinator.enqueue(project("project-a")).expect("enqueue"));
        assert!(
            !coordinator
                .enqueue(project("project-a"))
                .expect("duplicate")
        );
        let scheduler = crate::scheduler::Scheduler::open(temp.path().join("scheduler"))
            .expect("reopened scheduler");
        assert_eq!(
            scheduler.queued_projects().expect("queued projects"),
            vec![project("project-a")]
        );
        let events = events.lock().expect("events");
        let queued_event = events.iter().find(|event| {
            matches!(
                event,
                ShimAuditEvent::ProjectQueued {
                    project: queued,
                    added: true
                } if queued == &project("project-a")
            )
        });
        assert_eq!(
            queued_event.map(ShimAuditEvent::audit_summary).as_deref(),
            Some("hub_project_queued:project-a:added=true")
        );
    }

    #[test]
    fn only_lab_coordinator_can_enqueue_projects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let scheduler =
            crate::scheduler::Scheduler::open(temp.path().join("scheduler")).expect("scheduler");
        let shim =
            HubShim::start_with_scheduler(temp.path().join("hub.sock"), router, scheduler, |_| {})
                .expect("shim");
        let user = client(&shim, shim.register_role(Role::User).expect("user token"));

        assert!(matches!(
            user.enqueue(project("project-a")),
            Err(HubClientError::Shim(ShimError::LabCoordinatorRequired))
        ));
        let unknown = client(&shim, LaunchToken::from_raw("unknown-token".to_string()));
        assert!(matches!(
            unknown.enqueue(project("project-a")),
            Err(HubClientError::Shim(ShimError::UnknownToken))
        ));
    }

    #[test]
    fn shim_replaces_a_stale_socket_left_by_a_dead_run() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("hub.sock");
        drop(UnixListener::bind(&socket_path).expect("stale listener"));
        assert!(socket_path.exists(), "stale socket file stays after drop");

        let router = Router::open(temp.path().join("mail")).expect("router");
        let shim = HubShim::start(&socket_path, router, |_| {}).expect("shim over stale socket");

        assert!(UnixStream::connect(shim.socket_path()).is_ok());
    }

    #[test]
    fn shim_refuses_a_socket_another_shim_is_listening_on() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("hub.sock");
        let first = HubShim::start(
            &socket_path,
            Router::open(temp.path().join("mail")).expect("router"),
            |_| {},
        )
        .expect("first shim");

        let Err(error) = HubShim::start(
            &socket_path,
            Router::open(temp.path().join("mail2")).expect("router"),
            |_| {},
        ) else {
            panic!("second shim must not steal the socket");
        };

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(UnixStream::connect(first.socket_path()).is_ok());
    }

    #[test]
    fn shim_refuses_a_non_socket_file_at_its_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("hub.sock");
        fs::write(&socket_path, "not a socket").expect("plain file");

        let Err(error) = HubShim::start(
            &socket_path,
            Router::open(temp.path().join("mail")).expect("router"),
            |_| {},
        ) else {
            panic!("plain file must not be replaced");
        };

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(
            fs::read_to_string(&socket_path).expect("file"),
            "not a socket"
        );
    }

    #[test]
    fn shim_socket_is_owner_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let shim = HubShim::start(temp.path().join("hub.sock"), router, |_| {}).expect("shim");
        let mode = fs::metadata(shim.socket_path())
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, SOCKET_MODE);
    }

    #[test]
    fn worker_send_is_host_identity_bound_and_route_matrix_is_enforced() {
        let temp = tempfile::tempdir().expect("tempdir");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let events = Arc::new(Mutex::new(Vec::new()));
        let audit_events = Arc::clone(&events);
        let shim = HubShim::start(temp.path().join("hub.sock"), router, move |event| {
            audit_events.lock().expect("events").push(event);
        })
        .expect("shim");
        let worker_token = shim
            .register_role(worker("project-a", "worker-a"))
            .expect("worker token");
        let worker_client = client(&shim, worker_token.clone());
        let own_coordinator = Role::CoCoordinator(project("project-a"));
        let message = worker_client
            .send(
                own_coordinator.clone(),
                MessageKind::WorkerResult,
                None,
                "done",
                "completed",
            )
            .expect("allowed worker message");
        assert_eq!(message.from, worker("project-a", "worker-a"));
        assert_eq!(message.to, own_coordinator);
        for recipient in [
            Role::LabCoordinator,
            Role::CoCoordinator(project("project-b")),
            Role::User,
        ] {
            let error = worker_client
                .send(
                    recipient,
                    MessageKind::WorkerResult,
                    None,
                    "refused",
                    "not allowed",
                )
                .expect_err("route must be refused");
            assert!(matches!(
                error,
                HubClientError::Shim(ShimError::NotAllowed { .. })
            ));
        }

        let coordinator_token = shim
            .register_role(Role::CoCoordinator(project("project-a")))
            .expect("coordinator token");
        let coordinator_client = client(&shim, coordinator_token);
        let report = coordinator_client
            .send(
                Role::LabCoordinator,
                MessageKind::Milestone,
                Some(project("project-a")),
                "milestone",
                "complete",
            )
            .expect("coordinator can report");
        assert_eq!(report.from, Role::CoCoordinator(project("project-a")));
        assert_eq!(
            events
                .lock()
                .expect("events")
                .iter()
                .filter(|event| matches!(event, ShimAuditEvent::Sent { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn unknown_and_revoked_tokens_are_refused_and_client_cannot_claim_sender() {
        let temp = tempfile::tempdir().expect("tempdir");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let shim = HubShim::start(temp.path().join("hub.sock"), router, |_| {}).expect("shim");
        let token = shim
            .register_role(worker("project-a", "worker-a"))
            .expect("worker token");
        let unknown = HubClient::new(shim.socket_path(), LaunchToken("unknown".to_string()));
        assert!(matches!(
            unknown.send(
                Role::CoCoordinator(project("project-a")),
                MessageKind::WorkerResult,
                None,
                "x",
                "y"
            ),
            Err(HubClientError::Shim(ShimError::UnknownToken))
        ));

        let forged = serde_json::json!({
            "op": "send",
            "token": token.expose(),
            "from": { "lab_coordinator": null },
            "to": { "co_coordinator": "project-a" },
            "kind": "worker_result",
            "subject": "forged",
            "body": "not a coordinator"
        });
        let mut stream = UnixStream::connect(shim.socket_path()).expect("socket");
        serde_json::to_writer(&mut stream, &forged).expect("write forged request");
        stream.write_all(b"\n").expect("newline");
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .expect("read response");
        assert!(matches!(
            serde_json::from_str::<WireResponse>(&response),
            Ok(WireResponse::Error {
                error: ShimError::InvalidRequest
            })
        ));

        let worker_client = client(&shim, token.clone());
        assert!(worker_client.inbox().expect("own inbox").is_empty());
        assert!(shim.revoke(&token));
        assert!(matches!(
            worker_client.inbox(),
            Err(HubClientError::Shim(ShimError::UnknownToken))
        ));
    }

    #[test]
    fn inbox_next_and_ack_are_bound_to_the_registered_role() {
        let temp = tempfile::tempdir().expect("tempdir");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let shim = HubShim::start(temp.path().join("hub.sock"), router, |_| {}).expect("shim");
        let coordinator = client(
            &shim,
            shim.register_role(Role::CoCoordinator(project("project-a")))
                .expect("coordinator token"),
        );
        let worker = client(
            &shim,
            shim.register_role(worker("project-a", "worker-a"))
                .expect("worker token"),
        );

        coordinator
            .send(
                Role::worker(
                    project("project-a"),
                    "worker-a",
                    crate::routing::WorkerKind::Coder,
                )
                .expect("worker role"),
                MessageKind::Instruction,
                None,
                "task",
                "implement",
            )
            .expect("instruction");
        let unread = worker.inbox().expect("inbox");
        assert_eq!(unread.len(), 1);
        assert_eq!(worker.next().expect("next"), unread.first().cloned());
        assert_eq!(worker.ack().expect("ack"), 1);
        assert!(worker.inbox().expect("empty inbox").is_empty());
    }

    #[test]
    fn lab_coordinator_cannot_ack_before_its_host_turn_succeeds() {
        let temp = tempfile::tempdir().expect("tempdir");
        let router = Router::open(temp.path().join("mail")).expect("router");
        router
            .send(OutgoingMessage::new(
                Role::User,
                Role::LabCoordinator,
                MessageKind::UserRequest,
                "request",
                "do work",
            ))
            .expect("message");
        let shim =
            HubShim::start(temp.path().join("hub.sock"), router.clone(), |_| {}).expect("shim");
        let coordinator = client(
            &shim,
            shim.register_role(Role::LabCoordinator)
                .expect("coordinator token"),
        );

        assert!(matches!(
            coordinator.ack(),
            Err(HubClientError::Shim(ShimError::HostAcknowledgementRequired))
        ));
        assert_eq!(router.inbox(&Role::LabCoordinator).expect("inbox").len(), 1);
    }

    #[test]
    fn audit_output_never_contains_launch_tokens() {
        let temp = tempfile::tempdir().expect("tempdir");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let events = Arc::new(Mutex::new(Vec::new()));
        let audit_events = Arc::clone(&events);
        let shim = HubShim::start(temp.path().join("hub.sock"), router, move |event| {
            audit_events.lock().expect("events").push(event);
        })
        .expect("shim");
        let token = shim
            .register_role(worker("project-a", "worker-a"))
            .expect("worker token");
        let authorized = client(&shim, token.clone());
        authorized
            .send(
                Role::CoCoordinator(project("project-a")),
                MessageKind::WorkerResult,
                None,
                "result",
                "body not in audit",
            )
            .expect("send");
        authorized
            .send(
                Role::User,
                MessageKind::WorkerResult,
                None,
                "refused",
                "body not in audit",
            )
            .expect_err("refused");
        let events = events.lock().expect("events");
        let serialized = serde_json::to_string(&*events).expect("serialize audit");
        let summaries = events
            .iter()
            .map(ShimAuditEvent::audit_summary)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!serialized.contains(token.expose()));
        assert!(!summaries.contains(token.expose()));
        assert!(!serialized.contains("body not in audit"));
        assert!(!format!("{token:?}").contains(token.expose()));
    }
}
