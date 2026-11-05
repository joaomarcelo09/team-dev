//! Talking to the hub: the live event subscription and one-shot API calls.

use futures::{SinkExt, Stream, channel::mpsc};
use jgt_core::{
    Decision, Event, HubClient, Message, Question, ReconnectMode, Session, SessionStatus, Task, TaskEvent, TaskEventKind,
    TaskStatus,
    api::{
        ActorRequest, AnswerRequest, CreateTaskRequest, DecisionRequest, MessagesResponse, MoveResponse,
        MoveTaskRequest, PatchSessionRequest, PatchTaskRequest, PresenceRequest, SendRequest, SessionsResponse,
        TaskEventRequest, TaskEventsResponse, TasksResponse,
    },
    client::encode_query,
};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Feed {
    Connected,
    Event(Box<Event>),
    Disconnected(String),
}

/// Follows the hub's event stream forever, reconnecting when the hub restarts.
pub fn feed() -> impl Stream<Item = Feed> {
    iced::stream::channel(1024, async |mut output: mpsc::Sender<Feed>| {
        let client = HubClient::default();
        loop {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
            let follow = {
                let client = client.clone();
                tokio::spawn(async move { client.follow_events(move |e| { let _ = tx.send(e); }).await })
            };
            while let Some(event) = rx.recv().await {
                let item = match event {
                    Event::Hello { .. } => Feed::Connected,
                    other => Feed::Event(Box::new(other)),
                };
                if output.send(item).await.is_err() {
                    return;
                }
            }
            let reason = match follow.await {
                Ok(Ok(())) => "the hub closed the connection".to_string(),
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
            };
            if output.send(Feed::Disconnected(reason)).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
}

fn client() -> HubClient {
    HubClient::default()
}

pub async fn sessions() -> Result<Vec<Session>, String> {
    client()
        .get::<SessionsResponse>("/v1/sessions?archived=true")
        .await
        .map(|r| r.sessions)
        .map_err(|e| e.to_string())
}

pub async fn messages(session_id: String, before: Option<i64>) -> Result<Vec<Message>, String> {
    let path = match before {
        Some(b) => format!("/v1/sessions/{session_id}/messages?limit=200&before={b}"),
        None => format!("/v1/sessions/{session_id}/messages?limit=200"),
    };
    client().get::<MessagesResponse>(&path).await.map(|r| r.messages).map_err(|e| e.to_string())
}

pub async fn send(session_id: String, text: String) -> Result<Message, String> {
    client()
        .post(&format!("/v1/sessions/{session_id}/send"), &SendRequest { text })
        .await
        .map_err(|e| e.to_string())
}

pub async fn answer(question_id: String, answer: String) -> Result<Question, String> {
    client()
        .post(&format!("/v1/questions/{question_id}/answer"), &AnswerRequest { answer })
        .await
        .map_err(|e| e.to_string())
}

pub async fn patch(session_id: String, req: PatchSessionRequest) -> Result<Session, String> {
    client().patch(&format!("/v1/sessions/{session_id}"), &req).await.map_err(|e| e.to_string())
}

pub async fn delete(session_id: String) -> Result<(), String> {
    client()
        .delete::<serde_json::Value>(&format!("/v1/sessions/{session_id}"))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub async fn delete_workspace(folder: String) -> Result<(), String> {
    client()
        .delete::<serde_json::Value>(&format!("/v1/workspaces?folder={}", encode_query(&folder)))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub async fn mark_read(session_id: String) -> Result<(), String> {
    client()
        .post::<_, serde_json::Value>(&format!("/v1/sessions/{session_id}/read"), &())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub async fn presence(focused: bool, session_id: Option<String>) -> Result<(), String> {
    client()
        .post::<_, serde_json::Value>("/v1/ui/presence", &PresenceRequest { focused, session_id })
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub async fn reload_config() -> Result<(), String> {
    client()
        .post::<_, serde_json::Value>("/v1/config/reload", &())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub async fn reconnect(session: Session, mode: ReconnectMode) -> Result<String, String> {
    jgt_core::launch::reconnect(&client(), &session, mode).await
}

/// Sends a message; a sleeping Coordinator or a finished worker is woken so it reads it right away.
pub async fn send_and_wake(session: Session, text: String) -> Result<Message, String> {
    let message = send(session.id.clone(), text).await?;
    if session.role.is_some() && session.status == SessionStatus::Disconnected {
        let who = if session.is_coordinator() { "the Coordinator" } else { "the worker" };
        reconnect(session, ReconnectMode::Background)
            .await
            .map_err(|e| format!("sent, but waking {who} failed: {e}"))?;
    }
    Ok(message)
}

/// Starts (or wakes) the Coordinator for `folder` with `jg-team-dev start`.
pub async fn start_team(folder: String) -> Result<String, String> {
    let folder = match folder.trim().strip_prefix("~") {
        Some(rest) => format!("{}{rest}", jgt_core::paths::home_dir().display()),
        None => folder.trim().to_string(),
    };
    // The CLI installed next to this binary, else the one in PATH.
    let cli = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("jg-team-dev")))
        .filter(|p| p.is_file())
        .or_else(|| jgt_core::launch::which("jg-team-dev"))
        .ok_or("the jg-team-dev command was not found")?;
    let out = tokio::process::Command::new(cli)
        .arg("start")
        .arg(&folder)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text.lines().next().unwrap_or_default().to_string())
    } else {
        Err(text.trim().trim_start_matches("jg-team-dev: ").to_string())
    }
}

// ── board ───────────────────────────────────────────────────────────────────
// The app acts for the user: every Board write carries `ActorRequest::User`.

pub async fn tasks(workspace: String) -> Result<Vec<Task>, String> {
    client()
        .get::<TasksResponse>(&format!("/v1/tasks?workspace={}", encode_query(&workspace)))
        .await
        .map(|r| r.tasks)
        .map_err(|e| e.to_string())
}

pub async fn task_events(task_id: String) -> Result<Vec<TaskEvent>, String> {
    client()
        .get::<TaskEventsResponse>(&format!("/v1/tasks/{task_id}/events"))
        .await
        .map(|r| r.events)
        .map_err(|e| e.to_string())
}

pub async fn create_task(workspace: String, title: String, status: TaskStatus) -> Result<Task, String> {
    let req = CreateTaskRequest {
        actor: ActorRequest::User,
        workspace,
        title,
        description: String::new(),
        priority: None,
        status: Some(status),
        parent_id: None,
    };
    client().post("/v1/tasks", &req).await.map_err(|e| e.to_string())
}

/// `edit` fills in the fields to change on a request made for the user.
pub async fn patch_task(task_id: String, edit: impl FnOnce(&mut PatchTaskRequest)) -> Result<Task, String> {
    let mut req = PatchTaskRequest::new(ActorRequest::User);
    edit(&mut req);
    client().patch(&format!("/v1/tasks/{task_id}"), &req).await.map_err(|e| e.to_string())
}

/// Moves a task; a sleeping Coordinator told about it (a task put on the queue) is woken.
pub async fn move_task(task_id: String, to: TaskStatus, position: Option<i64>) -> Result<MoveResponse, String> {
    let req = MoveTaskRequest { actor: ActorRequest::User, to, position, reason: None };
    let client = client();
    let moved: MoveResponse = client.post(&format!("/v1/tasks/{task_id}/move"), &req).await.map_err(|e| e.to_string())?;
    if let Some(coordinator) = &moved.coordinator {
        jgt_core::team::wake(&client, coordinator)
            .await
            .map_err(|e| format!("moved, but waking the Coordinator failed: {e}"))?;
    }
    Ok(moved)
}

/// Comments on a task as the user (K13). The hub relays it to the workspace's Coordinator, which
/// is woken here; returns whether a Coordinator is there to read it.
pub async fn comment(task_id: String, workspace: String, text: String) -> Result<bool, String> {
    let req = TaskEventRequest { actor: ActorRequest::User, kind: TaskEventKind::Comment, body: text, meta: Default::default() };
    let client = client();
    let _: TaskEvent = client.post(&format!("/v1/tasks/{task_id}/events"), &req).await.map_err(|e| e.to_string())?;
    let Some(coordinator) = jgt_core::team::current_coordinator(&client, &workspace).await? else { return Ok(false) };
    jgt_core::team::wake(&client, &coordinator)
        .await
        .map_err(|e| format!("sent, but waking the Coordinator failed: {e}"))?;
    Ok(true)
}

pub async fn delete_task(task_id: String) -> Result<(), String> {
    client().delete::<()>(&format!("/v1/tasks/{task_id}?actor=user")).await.map_err(|e| e.to_string())
}

pub async fn decide(task_id: String, choice: Decision) -> Result<Task, String> {
    let req = DecisionRequest { actor: ActorRequest::User, choice, question_id: None };
    client().post(&format!("/v1/tasks/{task_id}/decision"), &req).await.map_err(|e| e.to_string())
}
