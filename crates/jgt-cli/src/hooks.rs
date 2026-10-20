//! `jg-team-dev hook <event>`: Claude Code hooks. They read the hook JSON on stdin and print the
//! hook's JSON answer on stdout. A hook must never break the user's session, so every
//! failure (hub not running, …) ends quietly with exit code 0 and no output.

use jgt_core::{
    Harness, HubClient, Message, MessageKind, QuestionStatus, Session, SessionStatus,
    api::{EndRequest, MessagesResponse, QuestionRequest, RegisterRequest, StateRequest},
    client::encode_query,
    config::Config,
    format_for_agent, launch, procs, short_id,
    team::{self, PARENT_ENV, ROLE_ENV},
};
use serde_json::{Value, json};
use std::{io::Read, time::Duration};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    Stop,
    PermissionRequest,
    Notification,
    SessionEnd,
}

impl HookEvent {
    pub fn arg(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session-start",
            HookEvent::UserPromptSubmit => "user-prompt-submit",
            HookEvent::PostToolUse => "post-tool-use",
            HookEvent::Stop => "stop",
            HookEvent::PermissionRequest => "permission-request",
            HookEvent::Notification => "notification",
            HookEvent::SessionEnd => "session-end",
        }
    }

    /// The Claude Code event name in settings.json.
    pub fn claude_name(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::Stop => "Stop",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::Notification => "Notification",
            HookEvent::SessionEnd => "SessionEnd",
        }
    }

    pub const ALL: [HookEvent; 7] = [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PostToolUse,
        HookEvent::Stop,
        HookEvent::PermissionRequest,
        HookEvent::Notification,
        HookEvent::SessionEnd,
    ];
}

const CONNECTED_NOTE: &str = "This session is connected to jg-team-dev, the user's desktop messenger for AI agents; it \
has its own chat box there. To reach the user use the `jg-team-dev` MCP tools: chat_send for messages, chat_notify when \
you finish something important or need attention, chat_ask to ask and wait for an answer, chat_status for a short \
status line. Messages the user writes in jg-team-dev reach you automatically, marked [jg-team-dev]; reply normally. A message that \
arrives while you wait between turns is shown twice (as Stop hook feedback and as a hook blocking error): it is one \
message, not an error.";

pub async fn run(event: HookEvent) -> anyhow::Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let hook = Hook { client: HubClient::default().with_timeout(Duration::from_secs(3)), input };
    match hook.handle(event).await {
        Ok(Some(output)) => println!("{output}"),
        Ok(None) => {}
        Err(e) => eprintln!("jg-team-dev: {e}"),
    }
    Ok(())
}

struct Hook {
    client: HubClient,
    input: Value,
}

impl Hook {
    fn str(&self, key: &str) -> Option<&str> {
        self.input.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    fn session_id(&self) -> anyhow::Result<&str> {
        self.str("session_id").ok_or_else(|| anyhow::anyhow!("hook input has no session_id"))
    }

    async fn register(&self, source: Option<&str>) -> anyhow::Result<Session> {
        let req = RegisterRequest {
            harness: Harness::ClaudeCode,
            external_id: self.session_id()?.to_string(),
            cwd: self.str("cwd").map(str::to_owned),
            title: self.str("session_title").map(str::to_owned),
            pid: procs::harness_pid(),
            transcript_path: self.str("transcript_path").map(str::to_owned),
            source: source.map(str::to_owned),
            role: std::env::var(ROLE_ENV).ok().filter(|r| !r.is_empty()),
            parent: std::env::var(PARENT_ENV).ok().filter(|p| !p.is_empty()),
        };
        Ok(self.client.post("/v1/sessions/register", &req).await?)
    }

    /// The session for this hook as the hub knows it, without registering it.
    async fn known(&self) -> anyhow::Result<Option<Session>> {
        let path = format!(
            "/v1/resolve?harness={}&external_id={}",
            Harness::ClaudeCode.as_str(),
            encode_query(self.session_id()?)
        );
        match self.client.get::<Session>(&path).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The session for this hook, registering it if the hub has not seen it (e.g. the hub
    /// started after the session, or a session that ended and was resumed).
    async fn session(&self) -> anyhow::Result<Session> {
        let path = format!(
            "/v1/resolve?harness={}&external_id={}",
            Harness::ClaudeCode.as_str(),
            encode_query(self.session_id()?)
        );
        match self.client.get::<Session>(&path).await {
            Ok(s) if s.status.is_connected() => Ok(s),
            Ok(_) => self.register(None).await,
            Err(e) if e.is_not_found() => self.register(None).await,
            Err(e) => Err(e.into()),
        }
    }

    async fn set_state(&self, session: &Session, status: SessionStatus, last_reply: Option<String>) -> anyhow::Result<Session> {
        Ok(self
            .client
            .post(&format!("/v1/sessions/{}/state", session.id), &StateRequest { status, last_reply })
            .await?)
    }

    async fn take(&self, session: &Session) -> anyhow::Result<Vec<Message>> {
        let r: MessagesResponse = self.client.post(&format!("/v1/sessions/{}/take", session.id), &()).await?;
        Ok(r.messages)
    }

    async fn handle(&self, event: HookEvent) -> anyhow::Result<Option<Value>> {
        match event {
            HookEvent::SessionStart => {
                let session = self.register(self.str("source")).await?;
                let mut note = CONNECTED_NOTE.to_string();
                if session.is_coordinator()
                    && let Ok(exe) = std::env::current_exe()
                {
                    note.push_str(&format!(
                        " You are the Coordinator: run `jg-team-dev worker` and `jg-team-dev workers` as `{}`.",
                        exe.canonicalize().unwrap_or(exe).display()
                    ));
                }
                Ok(Some(json!({
                    "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": note }
                })))
            }
            HookEvent::UserPromptSubmit => {
                let session = self.session().await?;
                self.set_state(&session, SessionStatus::Active, None).await?;
                let messages = self.take(&session).await?;
                Ok((!messages.is_empty()).then(|| {
                    json!({ "hookSpecificOutput": {
                        "hookEventName": "UserPromptSubmit",
                        "additionalContext": format_for_agent(&messages),
                    }})
                }))
            }
            HookEvent::PostToolUse => {
                let session = self.session().await?;
                if session.status != SessionStatus::Active {
                    self.set_state(&session, SessionStatus::Active, None).await?;
                }
                if session.queued == 0 {
                    return Ok(None);
                }
                let messages = self.take(&session).await?;
                Ok((!messages.is_empty()).then(|| {
                    json!({ "hookSpecificOutput": {
                        "hookEventName": "PostToolUse",
                        "additionalContext": format_for_agent(&messages),
                    }})
                }))
            }
            HookEvent::Stop => self.stop().await,
            HookEvent::PermissionRequest => self.permission().await,
            HookEvent::Notification => self.notification().await,
            HookEvent::SessionEnd => {
                // Already disconnected (e.g. the Coordinator went to sleep): nothing to report.
                let Some(session) = self.known().await?.filter(|s| s.status.is_connected()) else {
                    return Ok(None);
                };
                let reason = if session.is_worker() {
                    "finished".to_string()
                } else {
                    self.str("reason").unwrap_or("exit").replace('_', " ")
                };
                let _: Session = self
                    .client
                    .post(&format!("/v1/sessions/{}/end", session.id), &EndRequest { reason, gone: false })
                    .await?;
                Ok(None)
            }
        }
    }

    /// End of a turn: mirror the reply, hand over waiting messages, or park in listen mode.
    async fn stop(&self) -> anyhow::Result<Option<Value>> {
        let session = self.session().await?;
        let reply = self.str("last_assistant_message").map(str::to_owned);
        // Read before the reply is mirrored: what the user wrote to this worker directly.
        let direct = if session.is_worker() && session.awaiting_reply {
            self.direct_messages(&session).await.unwrap_or_default()
        } else {
            vec![]
        };
        let session = self.set_state(&session, SessionStatus::Idle, reply.clone()).await?;
        if let (false, Some(parent)) = (direct.is_empty(), session.parent_id.as_deref()) {
            let quote = |t: &str| t.trim().lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n");
            let text = format!(
                "[jg-team-dev] The user talked directly to worker **{}**.\n\nThey wrote:\n{}\n\nThe worker replied:\n{}",
                session.display_name(),
                direct.iter().map(|m| quote(m)).collect::<Vec<_>>().join("\n>\n"),
                quote(reply.as_deref().unwrap_or("(no reply)")),
            );
            team::relay(&self.client, parent, Some(&session.id), &text).await.map_err(anyhow::Error::msg)?;
        }

        let mut messages = self.take(&session).await?;
        if messages.is_empty() && session.listen {
            let config = Config::load();
            let secs = config.timeouts.listen_secs.clamp(10, 3500);
            let deadline = std::time::Instant::now() + Duration::from_secs(secs);
            // Keep waiting across hub restarts (e.g. a package upgrade): the connection drops,
            // so wait for the hub to come back and park again for the time that is left.
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now()).as_secs();
                if left < 5 {
                    break;
                }
                match self
                    .client
                    .get_long::<MessagesResponse>(
                        &format!("/v1/sessions/{}/wait?timeout={left}", session.id),
                        Duration::from_secs(left + 30),
                    )
                    .await
                {
                    Ok(r) => {
                        messages = r.messages;
                        break;
                    }
                    Err(e @ jgt_core::client::ClientError::Status { .. }) => return Err(e.into()),
                    Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
                }
            }
        }
        if messages.is_empty() {
            if session.listen && session.is_coordinator() {
                self.sleep(&session).await?;
            }
            return Ok(None);
        }
        self.set_state(&session, SessionStatus::Active, None).await?;
        Ok(Some(json!({ "decision": "block", "reason": format_for_agent(&messages) })))
    }

    /// The user's messages since the worker's last mirrored reply (relays from other agents excluded).
    async fn direct_messages(&self, session: &Session) -> anyhow::Result<Vec<String>> {
        let r: MessagesResponse = self.client.get(&format!("/v1/sessions/{}/messages?limit=30", session.id)).await?;
        let since = r.messages.iter().rposition(|m| m.kind == MessageKind::Reply).map_or(0, |i| i + 1);
        Ok(r.messages[since..]
            .iter()
            .filter(|m| m.direction == jgt_core::Direction::ToAgent && m.meta_str("relay_from").is_none())
            .map(|m| m.body.clone())
            .collect())
    }

    /// After a listen period with no message the Coordinator's process stops; the user's next
    /// message wakes it with `claude --bg --resume`, context intact.
    async fn sleep(&self, session: &Session) -> anyhow::Result<()> {
        use std::os::unix::process::CommandExt;

        let config = Config::load();
        let claude = launch::find_program("claude", config.commands.claude.as_deref()).map_err(anyhow::Error::msg)?;
        let secs = config.timeouts.listen_secs.clamp(10, 3500);
        let idle = if secs < 120 { format!("{secs} s") } else { format!("{} min", secs / 60) };
        let _: Session = self
            .client
            .post(
                &format!("/v1/sessions/{}/end", session.id),
                &EndRequest { reason: format!("asleep after {idle} without messages"), gone: false },
            )
            .await?;
        // Stop the background session once this hook has returned.
        std::process::Command::new("sh")
            .args(["-c", "sleep 3; exec \"$0\" stop \"$1\""])
            .arg(&claude)
            .arg(short_id(&session.external_id))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()?;
        Ok(())
    }

    /// Routes the permission prompt to the app when nobody is at the terminal.
    async fn permission(&self) -> anyhow::Result<Option<Value>> {
        let session = self.session().await?;
        let config = Config::load();
        let interactive = procs::harness_pid().is_some_and(procs::has_tty);
        let route = match config.permissions.route.as_str() {
            "always" => true,
            "never" => false,
            _ => session.listen || !interactive,
        };
        if !route {
            return Ok(None);
        }
        let tool = self.str("tool_name").unwrap_or("a tool");
        let tool_input = self.input.get("tool_input").cloned().unwrap_or(Value::Null);
        let prompt = describe_tool(tool, &tool_input);
        let timeout = config.timeouts.permission_secs.max(10);
        let question: jgt_core::Question = self
            .client
            .post(
                &format!("/v1/sessions/{}/questions", session.id),
                &QuestionRequest {
                    kind: MessageKind::Permission,
                    prompt,
                    options: vec!["Allow".into(), "Deny".into()],
                    meta: json!({ "tool": tool, "input": tool_input }),
                    timeout_secs: Some(timeout),
                },
            )
            .await?;
        let answered: jgt_core::Question = self
            .client
            .get_long(
                &format!("/v1/questions/{}/wait?timeout={}", question.id, timeout + 1),
                Duration::from_secs(timeout + 30),
            )
            .await?;
        if answered.status != QuestionStatus::Answered {
            return Ok(None); // fall back to Claude Code's own prompt
        }
        let decision = match answered.answer.as_deref() {
            Some("Allow") => json!({ "behavior": "allow" }),
            Some(other) => json!({
                "behavior": "deny",
                "message": if other == "Deny" {
                    "The user denied this in jg-team-dev.".to_string()
                } else {
                    format!("The user denied this in jg-team-dev and said: {other}")
                },
            }),
            None => return Ok(None),
        };
        Ok(Some(json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision } })))
    }

    async fn notification(&self) -> anyhow::Result<Option<Value>> {
        let kind = self.str("notification_type").unwrap_or("");
        // Turn ends and permission prompts are covered by the Stop/PermissionRequest hooks.
        if matches!(kind, "idle_prompt" | "auth_success" | "permission_prompt" | "elicitation_complete" | "elicitation_response") {
            return Ok(None);
        }
        let session = self.session().await?;
        let message = self.str("message").unwrap_or("Claude needs your attention");
        let title = self.str("title").unwrap_or("Claude Code");
        let _: Message = self
            .client
            .post(
                &format!("/v1/sessions/{}/messages", session.id),
                &jgt_core::api::AgentMessageRequest {
                    kind: MessageKind::Notify,
                    body: format!("**{title}**\n\n{message}"),
                    meta: json!({ "title": title, "level": "info", "source": kind }),
                },
            )
            .await?;
        Ok(None)
    }
}

/// A readable summary of a tool call for the Allow/Deny card.
pub fn describe_tool(tool: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str);
    let detail = match tool {
        "Bash" => field("command").map(|c| format!("```sh\n{c}\n```")),
        "Edit" | "Write" | "Read" | "MultiEdit" | "NotebookEdit" => {
            field("file_path").or(field("notebook_path")).map(|p| format!("`{p}`"))
        }
        "WebFetch" => field("url").map(|u| format!("<{u}>")),
        "WebSearch" => field("query").map(|q| format!("\"{q}\"")),
        _ => None,
    };
    let detail = detail.unwrap_or_else(|| {
        let text = serde_json::to_string_pretty(input).unwrap_or_default();
        let text: String = text.chars().take(1200).collect();
        format!("```json\n{text}\n```")
    });
    let description = field("description").map(|d| format!("\n\n{d}")).unwrap_or_default();
    format!("Allow **{tool}**?\n\n{detail}{description}")
}
