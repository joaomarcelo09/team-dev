//! Request and response bodies of the hub HTTP API (see PLAN.md §7).

use crate::model::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub harness: Harness,
    pub external_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// PID of the harness process (used for liveness checks and MCP routing).
    pub pid: Option<i64>,
    pub transcript_path: Option<String>,
    /// Why the session (re)started: startup, resume, clear, compact, …
    pub source: Option<String>,
    /// Team role from the `JGT_ROLE` environment variable set by `jg-team-dev start`.
    #[serde(default)]
    pub role: Option<String>,
    /// Hub id of the Coordinator that started this worker (`JGT_PARENT`).
    #[serde(default)]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRequest {
    pub status: SessionStatus,
    /// The final assistant text of the turn that just ended, if any.
    #[serde(default)]
    pub last_reply: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndRequest {
    pub reason: String,
    /// The context is gone for good (the session can never be resumed).
    #[serde(default)]
    pub gone: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessageRequest {
    #[serde(default = "default_kind")]
    pub kind: MessageKind,
    pub body: String,
    #[serde(default)]
    pub meta: serde_json::Value,
}

fn default_kind() -> MessageKind {
    MessageKind::Text
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub kind: MessageKind,
    pub prompt: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub meta: serde_json::Value,
    /// Seconds before the question expires (hub default when absent).
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerRequest {
    pub answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSession {
    pub id: String,
    pub external_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingResponse {
    pub sessions: Vec<PendingSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayRequest {
    /// The session the message comes from (a worker).
    pub from_session: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendRequest {
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// A worker's run finished: how long it took, and its cost when known. Set together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// A worker's model and effort (PRD H7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The tokens a finished worker's harness reported (PRD H8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconnectRequest {
    pub mode: ReconnectMode,
}

/// How a reconnecting client should start the agent again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconnectTicket {
    pub session: Session,
    /// The first prompt, when messages are waiting. The messages themselves are handed over
    /// by the agent's hooks/plugin once it is really running, so a crash cannot lose them.
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceRequest {
    pub focused: bool,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsResponse {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub db_path: String,
    pub sessions: i64,
    pub connected: i64,
    pub ui_connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

// ── board ───────────────────────────────────────────────────────────────────

/// Who sends a Board write. The hub resolves a session to the workspace's Coordinator or to a
/// worker (read-only); `user` is the desktop app, or the CLI run outside any agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActorRequest {
    User,
    Session { session: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskRequest {
    pub actor: ActorRequest,
    pub workspace: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<TaskPriority>,
    /// `backlog` (default) or `on_queue`; the Coordinator may only create in Backlog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    /// Id or key of the task this one belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// The *Waiting for you* flag as a client sets it; the hub adds `since`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitingInput {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
}

/// Changes to a task; absent fields stay. Title, description and priority belong to the user;
/// details, the waiting flag and the links to the Coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchTaskRequest {
    pub actor: ActorRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<TaskPriority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// `null` clears the flag; absent leaves it.
    #[serde(default, deserialize_with = "some_or_null", skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Option<WaitingInput>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

impl PatchTaskRequest {
    /// A request that changes nothing yet.
    pub fn new(actor: ActorRequest) -> Self {
        Self {
            actor,
            title: None,
            description: None,
            priority: None,
            details: None,
            waiting: None,
            run_id: None,
            worktree: None,
        }
    }
}

/// Tells a present `null` (`Some(None)`) from an absent field (`None`).
fn some_or_null<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveTaskRequest {
    pub actor: ActorRequest,
    pub to: TaskStatus,
    /// Index in the target column (default: the end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveResponse {
    pub task: Task,
    /// The Coordinator told about the move, for the caller to wake (`jgt_core::team::wake`).
    #[serde(default)]
    pub coordinator: Option<Session>,
    /// Whether the move put the task on the queue: with no `coordinator`, nobody will take it.
    #[serde(default)]
    pub queued: bool,
    #[serde(default)]
    pub git_steps: Vec<String>,
    /// The task's branch (`Git::branch_name`), set when there are `git_steps` or the task is in a
    /// git column.
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResultInput {
    pub status: TestStatus,
    /// Hub id of the coder session.
    pub worker: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub failures: Vec<String>,
}

/// A finding as the reviewer reports it. `critical` is required on every finding, and a
/// critical finding needs a `category` (K17); the hub rejects anything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<FindingCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default)]
    pub location: String,
    pub problem: String,
    #[serde(default)]
    pub required_action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResultInput {
    pub decision: ReviewDecision,
    /// Hub id of the reviewer session.
    pub worker: String,
    #[serde(default)]
    pub findings: Vec<FindingInput>,
}

/// Records the test or the review of the current pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultRequest {
    pub actor: ActorRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<TestResultInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewResultInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEventRequest {
    pub actor: ActorRequest,
    /// `comment`, `question`, `answer`, `link`, `worker_started` or `worker_finished`.
    pub kind: TaskEventKind,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TasksResponse {
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskEventsResponse {
    pub events: Vec<TaskEvent>,
}
