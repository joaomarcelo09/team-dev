//! Host-side Unix-socket broker for roles running in a lab sandbox.
//!
//! A role sends an opaque launch token and a GitHub action. The host resolves the
//! token to the GitHub owner, project, and worker before applying broker policy.

use std::{
    collections::HashMap,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
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

use jgf_core::{GithubAction, ProjectId, SecretStore};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    Broker,
    broker::GithubRequest,
    broker_exec::{BackendOutput, BrokerError, GithubBackend},
    git_flow::MergeContext,
    shim::LaunchToken,
};

const MAX_REQUEST_BYTES: u64 = 128 * 1024;
const SOCKET_MODE: u32 = 0o600;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Action fields accepted from an in-lab broker client.
///
/// The GitHub owner and project are intentionally absent; both are resolved from
/// the host-owned launch-token registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerAction {
    /// Action to perform.
    pub action: GithubAction,
    /// Repository to target.
    pub repo: String,
    /// Branch the action targets, when it has one.
    #[serde(default)]
    pub branch: Option<String>,
    /// Branch or ref the task branch is based on.
    #[serde(default)]
    pub base: Option<String>,
    /// Whether the caller wants a force-push. Always refused.
    #[serde(default)]
    pub force: bool,
    /// Whether the caller wants to delete the repository or branch. Always refused.
    #[serde(default)]
    pub delete: bool,
    /// Whether the targeted branch is the repository's default branch.
    #[serde(default)]
    pub target_is_default_branch: bool,
    /// Branch a pull request merges into.
    #[serde(default)]
    pub merge_target: Option<String>,
    /// The run's merge policy.
    #[serde(default)]
    pub merge_context: Option<MergeContext>,
    /// Commit message to check and clean, when the action creates a commit.
    #[serde(default)]
    pub commit_message: Option<String>,
}

impl BrokerAction {
    fn into_request(self, owner: String) -> GithubRequest {
        GithubRequest {
            action: self.action,
            owner,
            repo: self.repo,
            branch: self.branch,
            base: self.base,
            force: self.force,
            delete: self.delete,
            target_is_default_branch: self.target_is_default_branch,
            merge_target: self.merge_target,
            merge_context: self.merge_context,
            commit_message: self.commit_message,
        }
    }

    fn contains(&self, needle: &str) -> bool {
        self.repo.contains(needle)
            || self
                .branch
                .as_deref()
                .is_some_and(|value| value.contains(needle))
            || self
                .base
                .as_deref()
                .is_some_and(|value| value.contains(needle))
            || self
                .merge_target
                .as_deref()
                .is_some_and(|value| value.contains(needle))
            || self
                .commit_message
                .as_deref()
                .is_some_and(|value| value.contains(needle))
            || self.merge_context.as_ref().is_some_and(|context| {
                context.default_branch.contains(needle)
                    || context
                        .integration_branch
                        .as_deref()
                        .is_some_and(|value| value.contains(needle))
            })
    }
}

/// Typed error returned by the broker socket protocol.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub enum BrokerShimError {
    /// The provided launch token is unknown or has been revoked.
    #[error("unknown or revoked broker launch token")]
    UnknownToken,
    /// The request was malformed or attempted to include unsupported fields.
    #[error("invalid broker request")]
    InvalidRequest,
    /// The broker refused the requested action.
    #[error("broker denied action: {message}")]
    Denied {
        /// Safe policy refusal.
        message: String,
    },
    /// The approved action could not be completed.
    #[error("broker action failed: {message}")]
    Execution {
        /// Safe execution failure.
        message: String,
    },
    /// The host token registry could not be read.
    #[error("broker token registry is unavailable")]
    Unavailable,
}

/// Token-free audit event emitted by the broker socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BrokerShimAuditEvent {
    /// An action completed.
    Completed {
        /// Project resolved from the token registry.
        project: ProjectId,
        /// Worker resolved from the token registry.
        worker: String,
        /// Action that completed.
        action: GithubAction,
    },
    /// A request was refused.
    Refused {
        /// Project, when a valid token was resolved.
        project: Option<ProjectId>,
        /// Worker, when a valid token was resolved.
        worker: Option<String>,
        /// Action, when the request could be parsed.
        action: Option<GithubAction>,
        /// Stable, token-free refusal.
        error: BrokerShimError,
    },
}

impl BrokerShimAuditEvent {
    /// Short audit-log representation. Launch tokens are never included.
    pub fn audit_summary(&self) -> String {
        match self {
            Self::Completed {
                project,
                worker,
                action,
            } => format!("broker_completed:{project}:{worker}:{action:?}"),
            Self::Refused {
                project,
                worker,
                action,
                error,
            } => format!(
                "broker_refused:{}:{}:{action:?}:{error}",
                project
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "unknown".to_string()),
                worker.as_deref().unwrap_or("unknown"),
            ),
        }
    }
}

type AuditCallback = Arc<dyn Fn(BrokerShimAuditEvent) + Send + Sync + 'static>;

#[derive(Clone)]
struct BrokerIdentity {
    owner: String,
    project: ProjectId,
    worker: String,
}

struct BrokerState<B, S> {
    broker: Broker,
    backend: B,
    store: S,
    tokens: Mutex<HashMap<String, BrokerIdentity>>,
    audit: AuditCallback,
}

/// Unix-socket server for approved GitHub actions from lab roles.
pub struct BrokerShim<B, S> {
    socket_path: PathBuf,
    state: Arc<BrokerState<B, S>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl<B, S> BrokerShim<B, S>
where
    B: GithubBackend + Send + Sync + 'static,
    S: SecretStore + Send + Sync + 'static,
{
    /// Binds a socket and starts serving line-delimited JSON requests.
    pub fn start(
        socket_path: impl Into<PathBuf>,
        broker: Broker,
        backend: B,
        store: S,
        audit: impl Fn(BrokerShimAuditEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let socket_path = socket_path.into();
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)?;
        }
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

        let state = Arc::new(BrokerState {
            broker,
            backend,
            store,
            tokens: Mutex::new(HashMap::new()),
            audit: Arc::new(audit),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let server_state = Arc::clone(&state);
        let server_stop = Arc::clone(&stop);
        let thread = match thread::Builder::new()
            .name("jgf-broker-shim".to_string())
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

    /// Assigns a fresh capability to the host-selected owner, project, and worker.
    pub fn register_role(
        &self,
        owner: impl Into<String>,
        project: ProjectId,
        worker: impl Into<String>,
    ) -> Result<LaunchToken, BrokerShimError> {
        let identity = BrokerIdentity {
            owner: owner.into(),
            project,
            worker: worker.into(),
        };
        let mut tokens = self
            .state
            .tokens
            .lock()
            .map_err(|_| BrokerShimError::Unavailable)?;
        loop {
            let token = LaunchToken::generate().map_err(|_| BrokerShimError::Unavailable)?;
            if let std::collections::hash_map::Entry::Vacant(entry) =
                tokens.entry(token.expose().to_string())
            {
                entry.insert(identity);
                return Ok(token);
            }
        }
    }

    /// Revokes a launch token. Once revoked, it cannot access the broker.
    pub fn revoke(&self, token: &LaunchToken) -> bool {
        self.state
            .tokens
            .lock()
            .map(|mut tokens| tokens.remove(token.expose()).is_some())
            .unwrap_or(false)
    }
}

impl<B, S> Drop for BrokerShim<B, S> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// Small in-lab client that authenticates using the broker launch environment.
pub struct BrokerClient {
    socket_path: PathBuf,
    token: LaunchToken,
}

impl std::fmt::Debug for BrokerClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerClient")
            .field("socket_path", &self.socket_path)
            .field("token", &"[redacted]")
            .finish()
    }
}

impl BrokerClient {
    /// Reads `JG_BROKER_SOCKET` and `JG_BROKER_TOKEN` from the current process.
    pub fn from_env() -> Result<Self, BrokerClientError> {
        let socket_path = std::env::var_os("JG_BROKER_SOCKET")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(BrokerClientError::MissingSocket)?;
        let token =
            std::env::var("JG_BROKER_TOKEN").map_err(|_| BrokerClientError::MissingToken)?;
        if token.is_empty() {
            return Err(BrokerClientError::MissingToken);
        }
        Ok(Self {
            socket_path,
            token: LaunchToken::from_raw(token),
        })
    }

    /// Creates a client with explicit credentials, useful for host-side tests.
    pub fn new(socket_path: impl Into<PathBuf>, token: LaunchToken) -> Self {
        Self {
            socket_path: socket_path.into(),
            token,
        }
    }

    /// Submits an action without supplying an owner, project, or worker identity.
    pub fn request(&self, action: BrokerAction) -> Result<BackendOutput, BrokerClientError> {
        match self.request_wire(WireRequest {
            token: self.token.expose().to_string(),
            action,
        })? {
            WireOutput::Action(output) => Ok(output),
        }
    }

    fn request_wire(&self, request: WireRequest) -> Result<WireOutput, BrokerClientError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|error| BrokerClientError::Socket(error.to_string()))?;
        serde_json::to_writer(&mut stream, &request).map_err(|_| BrokerClientError::Protocol)?;
        stream
            .write_all(b"\n")
            .map_err(|error| BrokerClientError::Socket(error.to_string()))?;
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .map_err(|error| BrokerClientError::Socket(error.to_string()))?;
        if response.is_empty() {
            return Err(BrokerClientError::Protocol);
        }
        match serde_json::from_str::<WireResponse>(&response)
            .map_err(|_| BrokerClientError::Protocol)?
        {
            WireResponse::Ok { output } => Ok(output),
            WireResponse::Error { error } => Err(BrokerClientError::Shim(error)),
        }
    }
}

/// Errors raised by [`BrokerClient`].
#[derive(Debug, Error)]
pub enum BrokerClientError {
    /// `JG_BROKER_SOCKET` is missing or empty.
    #[error("JG_BROKER_SOCKET is missing")]
    MissingSocket,
    /// `JG_BROKER_TOKEN` is missing or empty.
    #[error("JG_BROKER_TOKEN is missing")]
    MissingToken,
    /// The socket could not be reached.
    #[error("broker socket request failed: {0}")]
    Socket(String),
    /// The peer returned an invalid response.
    #[error("invalid broker shim response")]
    Protocol,
    /// The shim refused the request.
    #[error(transparent)]
    Shim(#[from] BrokerShimError),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    token: String,
    action: BrokerAction,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WireResponse {
    Ok { output: WireOutput },
    Error { error: BrokerShimError },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
enum WireOutput {
    Action(BackendOutput),
}

fn serve<B, S>(listener: UnixListener, state: Arc<BrokerState<B, S>>, stop: Arc<AtomicBool>)
where
    B: GithubBackend + Send + Sync + 'static,
    S: SecretStore + Send + Sync + 'static,
{
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

fn handle_connection<B, S>(mut stream: UnixStream, state: &BrokerState<B, S>)
where
    B: GithubBackend + Send + Sync + 'static,
    S: SecretStore + Send + Sync + 'static,
{
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
        audit_refusal(state, None, None, BrokerShimError::InvalidRequest);
        WireResponse::Error {
            error: BrokerShimError::InvalidRequest,
        }
    } else {
        match serde_json::from_str::<WireRequest>(&line) {
            Ok(request) => dispatch(state, request),
            Err(_) => {
                audit_refusal(state, None, None, BrokerShimError::InvalidRequest);
                WireResponse::Error {
                    error: BrokerShimError::InvalidRequest,
                }
            }
        }
    };
    let _ = serde_json::to_writer(&mut stream, &response);
    let _ = stream.write_all(b"\n");
}

fn dispatch<B, S>(state: &BrokerState<B, S>, request: WireRequest) -> WireResponse
where
    B: GithubBackend + Send + Sync + 'static,
    S: SecretStore + Send + Sync + 'static,
{
    let token = request.token;
    let action = request.action;
    let identity = match resolve_identity(state, &token) {
        Ok(identity) => identity,
        Err(error) => {
            audit_refusal(state, None, Some(action.action), error.clone());
            return WireResponse::Error { error };
        }
    };
    if action.contains(&token) {
        audit_refusal(
            state,
            Some(&identity),
            Some(action.action),
            BrokerShimError::InvalidRequest,
        );
        return WireResponse::Error {
            error: BrokerShimError::InvalidRequest,
        };
    }

    let request = action.into_request(identity.owner.clone());
    match state.broker.execute_for_launch(
        &request,
        &identity.project,
        &identity.worker,
        &state.backend,
        &state.store,
        &token,
    ) {
        Ok(mut output) => {
            output.stdout = redact_literal(&output.stdout, &token);
            output.stderr = redact_literal(&output.stderr, &token);
            (state.audit)(BrokerShimAuditEvent::Completed {
                project: identity.project,
                worker: identity.worker,
                action: output.action,
            });
            WireResponse::Ok {
                output: WireOutput::Action(output),
            }
        }
        Err(error) => {
            let error = map_broker_error(error, &token);
            audit_refusal(state, Some(&identity), Some(request.action), error.clone());
            WireResponse::Error { error }
        }
    }
}

fn resolve_identity<B, S>(
    state: &BrokerState<B, S>,
    token: &str,
) -> Result<BrokerIdentity, BrokerShimError> {
    state
        .tokens
        .lock()
        .map_err(|_| BrokerShimError::Unavailable)?
        .get(token)
        .cloned()
        .ok_or(BrokerShimError::UnknownToken)
}

fn audit_refusal<B, S>(
    state: &BrokerState<B, S>,
    identity: Option<&BrokerIdentity>,
    action: Option<GithubAction>,
    error: BrokerShimError,
) {
    (state.audit)(BrokerShimAuditEvent::Refused {
        project: identity.map(|identity| identity.project.clone()),
        worker: identity.map(|identity| identity.worker.clone()),
        action,
        error,
    });
}

fn map_broker_error(error: BrokerError, launch_token: &str) -> BrokerShimError {
    let message = redact_literal(&error.to_string(), launch_token);
    match error {
        BrokerError::Denied(_) => BrokerShimError::Denied { message },
        _ => BrokerShimError::Execution { message },
    }
}

fn redact_literal(value: &str, marker: &str) -> String {
    if marker.is_empty() {
        value.to_string()
    } else {
        value.replace(marker, "[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{BufRead, BufReader, Write},
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicUsize, Ordering},
        sync::{Arc, Mutex},
    };

    use jgf_core::{AuditLog, FileSecretStore, GithubGrant, Secret, SecretKey, SecretScope};

    use super::*;
    use crate::{BackendError, BackendOp, GITHUB_TOKEN_KEY, GithubBackend};

    const LAUNCH_MARKER: &str = "broker-launch-token-marker";

    struct RecordingBackend {
        calls: AtomicUsize,
        last_op: Mutex<Option<BackendOp>>,
        output: String,
        fail: bool,
    }

    impl GithubBackend for RecordingBackend {
        fn run(
            &self,
            op: &BackendOp,
            _token: &jgf_core::Secret,
        ) -> Result<BackendOutput, BackendError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_op.lock().expect("recorded operation") = Some(op.clone());
            if self.fail {
                return Err(BackendError::CommitLog {
                    message: self.output.clone(),
                });
            }
            Ok(BackendOutput {
                action: op.action(),
                stdout: self.output.clone(),
                stderr: String::new(),
            })
        }
    }

    struct TestServer {
        broker: BrokerShim<ArcRecordingBackend, FileSecretStore>,
        broker_audit: AuditLog,
        events: Arc<Mutex<Vec<BrokerShimAuditEvent>>>,
        backend: Arc<RecordingBackend>,
        socket: PathBuf,
        _root: tempfile::TempDir,
    }

    fn server(output: &str) -> TestServer {
        server_with_failure(output, false)
    }

    fn server_with_failure(output: &str, fail: bool) -> TestServer {
        let root = tempfile::tempdir().expect("temp root");
        let audit = AuditLog::open(root.path(), "broker-shim-test").expect("audit");
        let broker = Broker::open(
            root.path().join("broker"),
            "broker-shim-test",
            GithubGrant {
                owners: vec!["trusted-owner".to_string()],
                actions: vec![GithubAction::CreateRepo],
                max_prs_per_day: 0,
                max_pushes_per_day: 0,
            },
            audit,
        )
        .expect("broker");
        let store = FileSecretStore::new(root.path());
        let key = SecretKey::parse(GITHUB_TOKEN_KEY).expect("github key");
        store
            .set(&SecretScope::Lab, &key, Secret::new("github-secret"))
            .expect("store github token");
        let backend = Arc::new(RecordingBackend {
            calls: AtomicUsize::new(0),
            last_op: Mutex::new(None),
            output: output.to_string(),
            fail,
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback_events = Arc::clone(&events);
        let socket = root.path().join("broker.sock");
        let broker = BrokerShim::start(
            &socket,
            broker,
            ArcRecordingBackend(Arc::clone(&backend)),
            store,
            move |event| callback_events.lock().expect("events").push(event),
        )
        .expect("broker shim");
        let broker_audit = AuditLog::open(root.path(), "broker-shim-test").expect("audit log");
        TestServer {
            broker,
            broker_audit,
            events,
            backend,
            socket,
            _root: root,
        }
    }

    struct ArcRecordingBackend(Arc<RecordingBackend>);

    impl GithubBackend for ArcRecordingBackend {
        fn run(
            &self,
            op: &BackendOp,
            token: &jgf_core::Secret,
        ) -> Result<BackendOutput, BackendError> {
            self.0.run(op, token)
        }
    }

    fn action(delete: bool) -> BrokerAction {
        BrokerAction {
            action: GithubAction::CreateRepo,
            repo: "sample".to_string(),
            branch: None,
            base: None,
            force: false,
            delete,
            target_is_default_branch: false,
            merge_target: None,
            merge_context: None,
            commit_message: None,
        }
    }

    fn identity(server: &TestServer) -> LaunchToken {
        server
            .broker
            .register_role(
                "trusted-owner",
                ProjectId::parse("project-a").expect("project"),
                "worker-a",
            )
            .expect("register role")
    }

    #[test]
    fn socket_is_private_and_owner_and_project_are_bound_to_the_token() {
        let server = server("");
        let mode = fs::metadata(&server.socket)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, SOCKET_MODE);

        let token = identity(&server);
        let client = BrokerClient::new(&server.socket, token);
        let output = client.request(action(false)).expect("approved request");
        assert_eq!(output.action, GithubAction::CreateRepo);
        assert_eq!(server.backend.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            server.backend.last_op.lock().expect("recorded operation").as_ref(),
            Some(BackendOp::CreateRepo { owner, repo })
                if owner == "trusted-owner" && repo == "sample"
        ));
    }

    #[test]
    fn unknown_and_revoked_tokens_are_refused_and_denials_are_audited() {
        let server = server("");
        let unknown = BrokerClient::new(
            &server.socket,
            LaunchToken::from_raw("unknown-marker".to_string()),
        );
        assert!(matches!(
            unknown.request(action(false)),
            Err(BrokerClientError::Shim(BrokerShimError::UnknownToken))
        ));

        let revoked_token = identity(&server);
        assert!(server.broker.revoke(&revoked_token));
        let revoked = BrokerClient::new(&server.socket, revoked_token);
        assert!(matches!(
            revoked.request(action(false)),
            Err(BrokerClientError::Shim(BrokerShimError::UnknownToken))
        ));

        let token = identity(&server);
        let client = BrokerClient::new(&server.socket, token);
        assert!(matches!(
            client.request(action(true)),
            Err(BrokerClientError::Shim(BrokerShimError::Denied { .. }))
        ));
        let broker_events = server.broker_audit.read_all().expect("broker audit");
        assert!(broker_events.iter().any(|event| matches!(
            event.kind,
            jgf_core::AuditKind::GithubAction { allowed: false, .. }
        )));
        assert!(
            server
                .events
                .lock()
                .expect("shim events")
                .iter()
                .any(|event| matches!(event, BrokerShimAuditEvent::Refused { .. }))
        );
        assert_eq!(server.backend.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn request_cannot_override_owner_or_project() {
        let server = server("");
        let token = identity(&server);
        let request = format!(
            r#"{{"token":"{}","action":{{"action":"create_repo","repo":"sample","owner":"untrusted","project":"other"}}}}"#,
            token.expose()
        );
        let mut stream = UnixStream::connect(&server.socket).expect("connect");
        writeln!(stream, "{request}").expect("send request");
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .expect("read response");
        assert!(response.contains("invalid_request"));
        assert!(!response.contains(token.expose()));
        assert_eq!(server.backend.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn launch_token_is_redacted_from_output_errors_audit_and_debug() {
        let output_marker = format!("{LAUNCH_MARKER} github-secret");
        let server = server(&output_marker);
        let token = LaunchToken::from_raw(LAUNCH_MARKER.to_string());
        server
            .broker
            .state
            .tokens
            .lock()
            .expect("token registry")
            .insert(
                LAUNCH_MARKER.to_string(),
                BrokerIdentity {
                    owner: "trusted-owner".to_string(),
                    project: ProjectId::parse("project-a").expect("project"),
                    worker: "worker-a".to_string(),
                },
            );
        let client = BrokerClient::new(&server.socket, token);
        let output = client.request(action(false)).expect("approved request");
        assert!(!output.stdout.contains(LAUNCH_MARKER));
        assert!(!output.stdout.contains("github-secret"));
        assert!(!format!("{output:?}").contains(LAUNCH_MARKER));
        assert!(!format!("{client:?}").contains(LAUNCH_MARKER));

        let token = LaunchToken::from_raw(LAUNCH_MARKER.to_string());
        let failing_server = server_with_failure(&output_marker, true);
        failing_server
            .broker
            .state
            .tokens
            .lock()
            .expect("token registry")
            .insert(
                LAUNCH_MARKER.to_string(),
                BrokerIdentity {
                    owner: "trusted-owner".to_string(),
                    project: ProjectId::parse("project-a").expect("project"),
                    worker: "worker-a".to_string(),
                },
            );
        let failing_client = BrokerClient::new(&failing_server.socket, token);
        let error = failing_client
            .request(action(false))
            .expect_err("backend failure");
        assert!(!error.to_string().contains(LAUNCH_MARKER));
        assert!(!error.to_string().contains("github-secret"));
        assert!(!format!("{error:?}").contains(LAUNCH_MARKER));
        let broker_events = failing_server
            .broker_audit
            .read_all()
            .expect("broker audit");
        let audit_debug = format!("{broker_events:?}");
        assert!(!audit_debug.contains(LAUNCH_MARKER));
        assert!(!audit_debug.contains("github-secret"));
        let shim_debug = format!("{:?}", failing_server.events.lock().expect("shim events"));
        assert!(!shim_debug.contains(LAUNCH_MARKER));
        assert!(!shim_debug.contains("github-secret"));
    }
}
