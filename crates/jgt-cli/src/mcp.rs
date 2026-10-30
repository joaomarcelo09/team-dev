//! `jg-team-dev mcp`: the MCP server (stdio) that gives agents tools to talk to the user.
//!
//! Each harness starts one server per process. The session a tool call belongs to is found
//! from the harness process id (Claude Code: one session per process) or from the
//! `jgt_session` argument the opencode plugin fills in.

use crate::tasks::{describe as describe_event, parse_column, parse_decision, show};
use jgt_core::{
    ClientError, Harness, HubClient, Message, MessageKind, NotifyLevel, QuestionStatus, Session, Task, TaskEvent,
    TaskEventKind, TaskPriority, TaskStatus,
    api::{
        ActorRequest, AgentMessageRequest, CreateTaskRequest, DecisionRequest, MessagesResponse, MoveResponse,
        MoveTaskRequest, PatchSessionRequest, PatchTaskRequest, QuestionRequest, ResultRequest, TaskEventRequest,
        TaskEventsResponse, TasksResponse, WaitingInput,
    },
    client::encode_query,
    procs,
    team::TASK_ENV,
};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::time::Duration;

const INSTRUCTIONS: &str = "jg-team-dev is the user's desktop messenger for AI agents. Each of your sessions has \
its own chat box in the jg-team-dev app. Use chat_send to tell the user something, chat_notify when you finish \
something important or need attention, chat_ask to ask a question and wait for the answer, and chat_status \
to show what you are doing. Messages the user writes in jg-team-dev reach you automatically, marked [jg-team-dev]; \
chat_inbox fetches any that are still waiting and chat_wait waits for the next one. The task_* tools read \
and (for the Coordinator only) change the folder's Board; a worker reads its task with task_get.";

/// Told to a worker calling a tool that changes the Board (the hub refuses it too, with a 403).
const WORKERS_READ_ONLY: &str = "Only the Coordinator moves cards and writes to the Board; report to it instead.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendArgs {
    /// The message, in Markdown.
    pub text: String,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotifyArgs {
    /// Short headline shown in the desktop notification.
    pub title: String,
    /// Details, in Markdown.
    #[serde(default)]
    pub body: Option<String>,
    /// One of: info, success, warning, error. Defaults to info.
    #[serde(default)]
    pub level: Option<String>,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskArgs {
    /// The question, in Markdown.
    pub question: String,
    /// Optional answers shown as buttons. The user can always type a free answer too.
    #[serde(default)]
    pub options: Vec<String>,
    /// How long to wait for the answer, in seconds (default 900).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitArgs {
    /// How long to wait for a message, in seconds (default 300, max 3600).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionOnlyArgs {
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextArgs {
    /// The text. Empty clears it.
    pub text: String,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskListArgs {
    /// Only this column: backlog, on_queue, working, testing, review or done.
    #[serde(default)]
    pub status: Option<String>,
    /// Include Done tasks (left out by default).
    #[serde(default)]
    pub include_done: bool,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskGetArgs {
    /// The task key (JGT-12). Default: the task you were started on.
    #[serde(default)]
    pub key: Option<String>,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitingArg {
    /// Why the task waits for the user (the open question).
    pub reason: String,
    /// The chat_ask question id.
    #[serde(default)]
    pub question_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskUpdateArgs {
    /// The task key (JGT-12).
    pub key: String,
    /// The Details: plan, files, what must not change, acceptance criteria (markdown). Never the
    /// user's description.
    #[serde(default)]
    pub details: Option<String>,
    /// Set *Waiting for you*.
    #[serde(default)]
    pub waiting: Option<WaitingArg>,
    /// Clear *Waiting for you* (the user answered).
    #[serde(default)]
    pub clear_waiting: bool,
    /// The test result of a tester or verifier: {"status": "pass"|"fail", "worker": "<tester or
    /// verifier hub id>", "summary": "…", "failures": ["…"]}. Recorded in Testing.
    #[serde(default)]
    pub test_result: Option<serde_json::Value>,
    /// The review result of a reviewer or verifier: {"decision": "approved"|"changes", "worker":
    /// "<reviewer or verifier hub id>", "findings": [{"severity": "critical"|"high"|"other", "critical": bool,
    /// "category": "security"|"secret"|"performance"|"patterns"|"docs"|"behavior"|null, "location": "…",
    /// "problem": "…", "required_action": "…"}]}. Recorded in Review. "changes" needs a critical or high
    /// finding; an approved review carries only "other" ones.
    #[serde(default)]
    pub review_result: Option<serde_json::Value>,
    /// The run folder name (`.team/runs/<run-id>/`).
    #[serde(default)]
    pub run_id: Option<String>,
    /// The worktree name (`.team/worktrees/<name>`).
    #[serde(default)]
    pub worktree: Option<String>,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskMoveArgs {
    /// The task key (JGT-12).
    pub key: String,
    /// The column: backlog, on_queue, working, testing, review, orchestrator_review or done.
    pub to: String,
    /// Why, for the task's activity log.
    #[serde(default)]
    pub reason: Option<String>,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskCreateArgs {
    pub title: String,
    /// What to do (markdown).
    #[serde(default)]
    pub description: String,
    /// The task this one is a subtask or a follow-up of (key).
    #[serde(default)]
    pub parent: Option<String>,
    /// Queue the subtask right away (only a subtask of a task the user queued).
    #[serde(default)]
    pub queue: bool,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskDecideArgs {
    /// The task key (JGT-12).
    pub key: String,
    /// The user's answer: done, more_rounds (one more fix), take_over or stop.
    pub choice: String,
    /// The answered chat_ask question that carries the user's choice.
    pub question_id: String,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskLogArgs {
    /// The task key (JGT-12).
    pub key: String,
    /// question, answer, link or comment.
    pub kind: String,
    /// The text for the log.
    pub text: String,
    #[schemars(description = "Internal: filled in automatically by the client. Leave empty.")]
    #[serde(default)]
    pub jgt_session: Option<String>,
}

#[derive(Clone)]
pub struct JgtMcp {
    client: HubClient,
    harness: Harness,
    harness_pid: Option<i64>,
    #[allow(dead_code)] // read by the #[tool_handler] macro
    tool_router: ToolRouter<Self>,
}

fn tool_error(text: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(text.into())]))
}

fn ok(text: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(text.into())]))
}

fn describe(e: &ClientError) -> String {
    match e {
        ClientError::Unavailable(_) => {
            "jg-team-dev is not running, so the user cannot be reached through it right now.".to_string()
        }
        other => format!("jg-team-dev error: {other}"),
    }
}

/// `/v1/tasks/<task>[<tail>]?workspace=…`, so `<task>` may be a key.
fn task_path(workspace: &str, task: &str, tail: &str) -> String {
    format!("/v1/tasks/{}{tail}?workspace={}", encode_query(task.trim()), encode_query(workspace))
}

fn render_inbox(messages: &[Message]) -> String {
    if messages.is_empty() {
        return "No new messages from the user.".into();
    }
    jgt_core::format_for_agent(messages)
}

/// The Board for `task_list`: one line per task, in column order.
fn render_tasks(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "No tasks.".into();
    }
    tasks
        .iter()
        .map(|t| {
            let mut line = format!("- {} [{}] {}", t.key, t.status.label(), t.title);
            if t.priority != TaskPriority::Normal {
                line.push_str(&format!(" ({})", t.priority));
            }
            if t.shows_round() {
                let (round, limit) = t.round();
                line.push_str(&format!(" · round {round}/{limit}"));
            }
            if let Some(w) = &t.waiting {
                line.push_str(&format!(" · waiting: {}", w.reason));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Short Markdown for a task after a change: key, column, round, waiting.
fn render_change(t: &Task) -> String {
    let mut out = format!("{} is in {}", t.key, t.status.label());
    if t.shows_round() {
        let (round, limit) = t.round();
        out.push_str(&format!(", round {round}/{limit}"));
    }
    if let Some(w) = &t.waiting {
        out.push_str(&format!(". Waiting for the user: {}", w.reason));
    }
    out.push('.');
    out
}

#[tool_router]
impl JgtMcp {
    pub fn new(harness: Harness) -> Self {
        Self {
            client: HubClient::default(),
            harness,
            harness_pid: procs::harness_pid(),
            tool_router: Self::tool_router(),
        }
    }

    /// Finds the jg-team-dev session this call belongs to.
    async fn session(&self, explicit: Option<&str>) -> Result<Session, String> {
        let explicit = explicit.map(str::trim).filter(|s| !s.is_empty() && *s != "null");
        let result = match (explicit, self.harness_pid) {
            (Some(ext), _) => {
                let path = format!(
                    "/v1/resolve?harness={}&external_id={}",
                    self.harness.as_str(),
                    encode_query(ext)
                );
                self.client.get::<Session>(&path).await
            }
            (None, Some(pid)) => self.client.get::<Session>(&format!("/v1/resolve?pid={pid}")).await,
            (None, None) => return Err("could not identify the agent process".into()),
        };
        result.map_err(|e| {
            if e.is_not_found() {
                "This session is not registered with jg-team-dev yet (are the jg-team-dev hooks/plugin installed? \
                 run `jg-team-dev connect`)."
                    .to_string()
            } else {
                describe(&e)
            }
        })
    }

    async fn post(&self, session: &Session, kind: MessageKind, body: String, meta: serde_json::Value) -> Result<(), String> {
        self.client
            .post::<_, Message>(
                &format!("/v1/sessions/{}/messages", session.id),
                &AgentMessageRequest { kind, body, meta },
            )
            .await
            .map(|_| ())
            .map_err(|e| describe(&e))
    }

    #[tool(description = "Send a message to the user's jg-team-dev chat for this session. Use it to report \
        progress, results, or anything the user should read even when they are away from the terminal.")]
    async fn chat_send(&self, Parameters(args): Parameters<SendArgs>) -> Result<CallToolResult, McpError> {
        let session = match self.session(args.jgt_session.as_deref()).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        match self.post(&session, MessageKind::Text, args.text, serde_json::Value::Null).await {
            Ok(()) => ok("Sent."),
            Err(e) => tool_error(e),
        }
    }

    #[tool(description = "Send the user a desktop notification (also shown highlighted in the chat). Use it \
        when you finish a task, hit a blocker, or need the user's attention.")]
    async fn chat_notify(&self, Parameters(args): Parameters<NotifyArgs>) -> Result<CallToolResult, McpError> {
        let session = match self.session(args.jgt_session.as_deref()).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        let level: NotifyLevel = args.level.as_deref().unwrap_or("info").parse().unwrap_or(NotifyLevel::Info);
        let body = match args.body.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            Some(b) => format!("**{}**\n\n{b}", args.title.trim()),
            None => format!("**{}**", args.title.trim()),
        };
        let meta = serde_json::json!({ "title": args.title.trim(), "level": level.as_str() });
        match self.post(&session, MessageKind::Notify, body, meta).await {
            Ok(()) => ok("Notification sent."),
            Err(e) => tool_error(e),
        }
    }

    #[tool(description = "Ask the user a question in jg-team-dev and wait for the answer. Offer `options` for \
        quick replies. Returns the user's answer, or says that nobody answered in time.")]
    async fn chat_ask(&self, Parameters(args): Parameters<AskArgs>) -> Result<CallToolResult, McpError> {
        let session = match self.session(args.jgt_session.as_deref()).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        let timeout = args.timeout_secs.unwrap_or(900).clamp(10, 3600);
        let question = self
            .client
            .post::<_, jgt_core::Question>(
                &format!("/v1/sessions/{}/questions", session.id),
                &QuestionRequest {
                    kind: MessageKind::Question,
                    prompt: args.question,
                    options: args.options,
                    meta: serde_json::Value::Null,
                    timeout_secs: Some(timeout),
                },
            )
            .await;
        let question = match question {
            Ok(q) => q,
            Err(e) => return tool_error(describe(&e)),
        };
        let answered = self
            .client
            .get_long::<jgt_core::Question>(
                &format!("/v1/questions/{}/wait?timeout={}", question.id, timeout + 2),
                Duration::from_secs(timeout + 30),
            )
            .await;
        match answered {
            Ok(q) if q.status == QuestionStatus::Answered => {
                ok(format!("The user answered: {}", q.answer.unwrap_or_default()))
            }
            Ok(_) => ok(format!("The user did not answer within {timeout} seconds.")),
            Err(e) => tool_error(describe(&e)),
        }
    }

    #[tool(description = "Fetch messages the user sent from jg-team-dev that you have not received yet.")]
    async fn chat_inbox(&self, Parameters(args): Parameters<SessionOnlyArgs>) -> Result<CallToolResult, McpError> {
        let session = match self.session(args.jgt_session.as_deref()).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        match self.client.post::<_, MessagesResponse>(&format!("/v1/sessions/{}/take", session.id), &()).await {
            Ok(r) => ok(render_inbox(&r.messages)),
            Err(e) => tool_error(describe(&e)),
        }
    }

    #[tool(description = "Wait until the user sends a message from jg-team-dev, then return it. Use it when you \
        have nothing left to do and should stay available for the user's next instruction.")]
    async fn chat_wait(&self, Parameters(args): Parameters<WaitArgs>) -> Result<CallToolResult, McpError> {
        let session = match self.session(args.jgt_session.as_deref()).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        let timeout = args.timeout_secs.unwrap_or(300).clamp(1, 3600);
        let result = self
            .client
            .get_long::<MessagesResponse>(
                &format!("/v1/sessions/{}/wait?timeout={timeout}", session.id),
                Duration::from_secs(timeout + 30),
            )
            .await;
        match result {
            Ok(r) if r.messages.is_empty() => ok(format!("No message from the user in {timeout} seconds.")),
            Ok(r) => ok(render_inbox(&r.messages)),
            Err(e) => tool_error(describe(&e)),
        }
    }

    #[tool(description = "Show a short status line (what you are doing right now) in the jg-team-dev chat header. \
        An empty text clears it.")]
    async fn chat_status(&self, Parameters(args): Parameters<TextArgs>) -> Result<CallToolResult, McpError> {
        self.patch(args.jgt_session.as_deref(), PatchSessionRequest {
            status_text: Some(args.text),
            ..Default::default()
        })
        .await
    }

    #[tool(description = "Rename this session's chat box in jg-team-dev (a short title for the task).")]
    async fn chat_set_title(&self, Parameters(args): Parameters<TextArgs>) -> Result<CallToolResult, McpError> {
        self.patch(args.jgt_session.as_deref(), PatchSessionRequest { title: Some(args.text), ..Default::default() })
            .await
    }

    // ── the Board ───────────────────────────────────────────────────────────

    /// The caller and its Board's workspace: a Coordinator's own folder, its Coordinator's for a
    /// worker.
    async fn board(&self, explicit: Option<&str>) -> Result<(Session, String), String> {
        let session = self.session(explicit).await?;
        let folder = match (&session.parent_id, session.is_coordinator()) {
            (Some(parent), false) => {
                let coordinator: Session =
                    self.client.get(&format!("/v1/sessions/{parent}")).await.map_err(|e| describe(&e))?;
                coordinator.cwd
            }
            _ => session.cwd.clone(),
        };
        let folder = folder.ok_or("This session has no folder, so it has no Board.")?;
        Ok((session, folder))
    }

    /// The Coordinator's Board; workers get `WORKERS_READ_ONLY` before the hub is asked.
    async fn coordinator_board(&self, explicit: Option<&str>) -> Result<(ActorRequest, String), String> {
        let (session, workspace) = self.board(explicit).await?;
        if !session.is_coordinator() {
            return Err(WORKERS_READ_ONLY.into());
        }
        Ok((ActorRequest::Session { session: session.id }, workspace))
    }

    #[tool(description = "List this folder's Board in column order (Done left out unless include_done). \
        Filter with status: backlog, on_queue, working, testing, review, orchestrator_review or done.")]
    async fn task_list(&self, Parameters(args): Parameters<TaskListArgs>) -> Result<CallToolResult, McpError> {
        let (_, workspace) = match self.board(args.jgt_session.as_deref()).await {
            Ok(b) => b,
            Err(e) => return tool_error(e),
        };
        let status = match args.status.as_deref().map(parse_column).transpose() {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        match self.client.get::<TasksResponse>(&format!("/v1/tasks?workspace={}", encode_query(&workspace))).await {
            Ok(r) => {
                let tasks: Vec<Task> = r
                    .tasks
                    .into_iter()
                    .filter(|t| match status {
                        Some(s) => t.status == s,
                        None => args.include_done || t.status != TaskStatus::Done,
                    })
                    .collect();
                ok(render_tasks(&tasks))
            }
            Err(e) => tool_error(describe(&e)),
        }
    }

    #[tool(description = "Read a Board task: description, Details, results, round and log. Without key: the \
        task you were started on.")]
    async fn task_get(&self, Parameters(args): Parameters<TaskGetArgs>) -> Result<CallToolResult, McpError> {
        let (_, workspace) = match self.board(args.jgt_session.as_deref()).await {
            Ok(b) => b,
            Err(e) => return tool_error(e),
        };
        let key = args.key.filter(|k| !k.trim().is_empty()).or_else(|| std::env::var(TASK_ENV).ok());
        let Some(key) = key else {
            return tool_error("Name the task: key = \"JGT-12\" (you were not started on a task).");
        };
        let task = match self.client.get::<Task>(&task_path(&workspace, &key, "")).await {
            Ok(t) => t,
            Err(e) => return tool_error(describe(&e)),
        };
        let mut text = show(&task);
        if let Ok(r) = self.client.get::<TaskEventsResponse>(&task_path(&workspace, &task.id, "/events")).await {
            text.push_str("\n## Log\n");
            for e in r.events.iter().rev().take(20).rev() {
                text.push_str(&format!("- {}: {}\n", e.actor.label(), describe_event(e)));
            }
        }
        ok(text)
    }

    #[tool(description = "Coordinator only. Write a task's Details (never the user's description), set or \
        clear Waiting for you, record the tester's or the reviewer's result, or link its run and worktree.")]
    async fn task_update(&self, Parameters(args): Parameters<TaskUpdateArgs>) -> Result<CallToolResult, McpError> {
        let (actor, workspace) = match self.coordinator_board(args.jgt_session.as_deref()).await {
            Ok(b) => b,
            Err(e) => return tool_error(e),
        };
        let mut task = None;
        let mut req = PatchTaskRequest::new(actor.clone());
        req.details = args.details;
        req.run_id = args.run_id;
        req.worktree = args.worktree;
        req.waiting = match (args.waiting, args.clear_waiting) {
            (Some(w), _) => Some(Some(WaitingInput { reason: w.reason, question_id: w.question_id })),
            (None, true) => Some(None),
            (None, false) => None,
        };
        if req.details.is_some() || req.waiting.is_some() || req.run_id.is_some() || req.worktree.is_some() {
            match self.client.patch::<_, Task>(&task_path(&workspace, &args.key, ""), &req).await {
                Ok(t) => task = Some(t),
                Err(e) => return tool_error(describe(&e)),
            }
        }
        for (test, review) in [(args.test_result, None), (None, args.review_result)] {
            if test.is_none() && review.is_none() {
                continue;
            }
            let req = ResultRequest {
                actor: actor.clone(),
                test: match test.map(serde_json::from_value).transpose() {
                    Ok(t) => t,
                    Err(e) => return tool_error(format!("test_result: {e}")),
                },
                review: match review.map(serde_json::from_value).transpose() {
                    Ok(r) => r,
                    Err(e) => return tool_error(format!("review_result: {e}")),
                },
            };
            match self.client.post::<_, Task>(&task_path(&workspace, &args.key, "/results"), &req).await {
                Ok(t) => task = Some(t),
                Err(e) => return tool_error(describe(&e)),
            }
        }
        match task {
            Some(t) => ok(render_change(&t)),
            None => tool_error("Nothing to change: pass details, waiting, clear_waiting, a result, run_id or worktree."),
        }
    }

    #[tool(description = "Coordinator only. Move a task to another column: backlog, on_queue, working, \
        testing, review, orchestrator_review or done. The hub checks the gates; a refusal is final, so report it.")]
    async fn task_move(&self, Parameters(args): Parameters<TaskMoveArgs>) -> Result<CallToolResult, McpError> {
        let (actor, workspace) = match self.coordinator_board(args.jgt_session.as_deref()).await {
            Ok(b) => b,
            Err(e) => return tool_error(e),
        };
        let to = match parse_column(&args.to) {
            Ok(to) => to,
            Err(e) => return tool_error(e),
        };
        let req = MoveTaskRequest { actor, to, position: None, reason: args.reason };
        match self.client.post::<_, MoveResponse>(&task_path(&workspace, &args.key, "/move"), &req).await {
            Ok(r) => ok(render_change(&r.task)),
            Err(e) => tool_error(describe(&e)),
        }
    }

    #[tool(description = "Coordinator only. Create a task in Backlog: a planner's subtask or a review \
        follow-up (parent = its task). queue = true queues a subtask of a task the user queued.")]
    async fn task_create(&self, Parameters(args): Parameters<TaskCreateArgs>) -> Result<CallToolResult, McpError> {
        let (actor, workspace) = match self.coordinator_board(args.jgt_session.as_deref()).await {
            Ok(b) => b,
            Err(e) => return tool_error(e),
        };
        let parent_id = match &args.parent {
            Some(key) => match self.client.get::<Task>(&task_path(&workspace, key, "")).await {
                Ok(t) => Some(t.id),
                Err(e) => return tool_error(describe(&e)),
            },
            None => None,
        };
        let req = CreateTaskRequest {
            actor: actor.clone(),
            workspace: workspace.clone(),
            title: args.title,
            description: args.description,
            priority: None,
            status: None,
            parent_id,
        };
        let task = match self.client.post::<_, Task>("/v1/tasks", &req).await {
            Ok(t) => t,
            Err(e) => return tool_error(describe(&e)),
        };
        if !args.queue {
            return ok(format!("{} created in Backlog.", task.key));
        }
        let req = MoveTaskRequest { actor, to: TaskStatus::OnQueue, position: None, reason: Some("subtask".into()) };
        match self.client.post::<_, MoveResponse>(&task_path(&workspace, &task.id, "/move"), &req).await {
            Ok(r) => ok(format!("{} created and queued. {}", r.task.key, render_change(&r.task))),
            Err(e) => tool_error(format!("{} created in Backlog, but not queued: {}", task.key, describe(&e))),
        }
    }

    #[tool(description = "Coordinator only. Pass on the user's decision on a task in Orchestrator review: \
        done, more_rounds (one more fix), take_over or stop, citing the answered chat_ask question_id.")]
    async fn task_decide(&self, Parameters(args): Parameters<TaskDecideArgs>) -> Result<CallToolResult, McpError> {
        let (actor, workspace) = match self.coordinator_board(args.jgt_session.as_deref()).await {
            Ok(b) => b,
            Err(e) => return tool_error(e),
        };
        let choice = match parse_decision(&args.choice) {
            Ok(c) => c,
            Err(e) => return tool_error(e),
        };
        let req = DecisionRequest { actor, choice, question_id: Some(args.question_id) };
        match self.client.post::<_, Task>(&task_path(&workspace, &args.key, "/decision"), &req).await {
            Ok(t) => ok(render_change(&t)),
            Err(e) => tool_error(describe(&e)),
        }
    }

    #[tool(description = "Coordinator only. Add a question, answer, link or comment to a task's activity log.")]
    async fn task_log(&self, Parameters(args): Parameters<TaskLogArgs>) -> Result<CallToolResult, McpError> {
        let (actor, workspace) = match self.coordinator_board(args.jgt_session.as_deref()).await {
            Ok(b) => b,
            Err(e) => return tool_error(e),
        };
        let kind = match args.kind.trim().to_lowercase().parse::<TaskEventKind>() {
            Ok(k @ (TaskEventKind::Question | TaskEventKind::Answer | TaskEventKind::Link | TaskEventKind::Comment)) => k,
            _ => return tool_error("kind is one of: question, answer, link, comment."),
        };
        let req = TaskEventRequest { actor, kind, body: args.text, meta: serde_json::Value::Null };
        match self.client.post::<_, TaskEvent>(&task_path(&workspace, &args.key, "/events"), &req).await {
            Ok(_) => ok("Logged."),
            Err(e) => tool_error(describe(&e)),
        }
    }

    async fn patch(&self, explicit: Option<&str>, req: PatchSessionRequest) -> Result<CallToolResult, McpError> {
        let session = match self.session(explicit).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        match self.client.patch::<_, Session>(&format!("/v1/sessions/{}?source=agent", session.id), &req).await {
            Ok(_) => ok("Updated."),
            Err(e) => tool_error(describe(&e)),
        }
    }
}

#[tool_handler]
impl ServerHandler for JgtMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("jg-team-dev", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

pub async fn run(harness: Harness) -> anyhow::Result<()> {
    let service = JgtMcp::new(harness).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_board_tools_are_listed() {
        let names: Vec<String> = JgtMcp::tool_router().list_all().into_iter().map(|t| t.name.to_string()).collect();
        for tool in ["task_list", "task_get", "task_update", "task_move", "task_create", "task_decide", "task_log"] {
            assert!(names.iter().any(|n| n == tool), "{tool} in {names:?}");
        }
    }

    #[test]
    fn tasks_render_as_short_lines() {
        assert_eq!(render_tasks(&[]), "No tasks.");
        let mut a = Task::new("/w", 1, "fix navbar", TaskStatus::Review);
        a.priority = TaskPriority::High;
        a.review_rounds = 1;
        let b = Task::new("/w", 2, "later", TaskStatus::Backlog);
        assert_eq!(render_tasks(&[a.clone(), b]), "- JGT-1 [Review] fix navbar (high) · round 2/2\n- JGT-2 [Backlog] later");
        assert_eq!(render_change(&a), "JGT-1 is in Review, round 2/2.");
        assert_eq!(task_path("/w", " JGT-1 ", "/move"), "/v1/tasks/JGT-1/move?workspace=%2Fw");
    }
}
