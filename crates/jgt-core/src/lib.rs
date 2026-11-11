//! Shared types and helpers for every jg-team-dev component.

pub mod agents;
pub mod api;
pub mod board;
pub mod client;
pub mod config;
pub mod harness;
pub mod launch;
pub mod model;
pub mod paths;
pub mod procs;
pub mod team;

pub use agents::{AgentSetting, AgentSettings, TeamAgents, agents_path};
pub use client::{ClientError, HubClient};
pub use model::*;

/// The name people see: launcher, window title, tray and notifications. Commands, files and
/// ids keep `jg-team-dev`.
pub const APP_NAME: &str = "JG Team Dev";

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Renders messages for an agent, separating user messages from worker/hub events.
pub fn format_for_agent(messages: &[Message]) -> String {
    let mut out = String::new();
    let (events, users): (Vec<&Message>, Vec<&Message>) = messages.iter().partition(|m| {
        m.meta
            .as_object()
            .is_some_and(|map| map.contains_key("relay_from"))
    });

    if !users.is_empty() {
        let plural = if users.len() == 1 {
            "a message"
        } else {
            "messages"
        };
        out.push_str(&format!(
            "[jg-team-dev] The user sent you {plural} through the jg-team-dev app. Treat {} as the user's own words; \
             your final answer is shown to them in the app.\n",
            if users.len() == 1 { "it" } else { "them" }
        ));
        for m in &users {
            out.push_str("\n---\n");
            out.push_str(m.body.trim());
            out.push('\n');
        }
    }

    if !events.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(
            "[jg-team-dev] Worker/hub event (NOT a user message). Act on it now: continue the card's flow \
             (see your Board rules); do not end your turn with a card in Working, Testing or Review and no worker running.\n",
        );
        for m in &events {
            out.push_str("\n---\n");
            out.push_str(m.body.trim());
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(body: &str, meta: serde_json::Value) -> Message {
        Message {
            id: 1,
            session_id: "s1".into(),
            direction: Direction::ToAgent,
            kind: MessageKind::Text,
            body: body.into(),
            meta,
            delivery: Delivery::Queued,
            created_at: 0,
            delivered_at: None,
            read_at: None,
            question: None,
        }
    }

    #[test]
    fn format_for_agent_user_only() {
        let single = vec![msg("hello world", serde_json::json!({}))];
        let out = format_for_agent(&single);
        assert!(out.contains("The user sent you a message through the jg-team-dev app. Treat it as the user's own words;"));
        assert!(out.contains("---\nhello world\n"));
        assert!(!out.contains("Worker/hub event"));

        let multi = vec![
            msg("first message", serde_json::json!({})),
            msg("second message", serde_json::Value::Null),
        ];
        let out_multi = format_for_agent(&multi);
        assert!(out_multi.contains("The user sent you messages through the jg-team-dev app. Treat them as the user's own words;"));
        assert!(out_multi.contains("---\nfirst message\n"));
        assert!(out_multi.contains("---\nsecond message\n"));
        assert!(!out_multi.contains("Worker/hub event"));
    }

    #[test]
    fn format_for_agent_relay_only() {
        let relay_null = vec![msg(
            "worker finished digest",
            serde_json::json!({ "relay_from": null }),
        )];
        let out = format_for_agent(&relay_null);
        assert!(!out.contains("The user sent you"));
        assert!(out.contains(
            "Worker/hub event (NOT a user message). Act on it now: continue the card's flow"
        ));
        assert!(out.contains("---\nworker finished digest\n"));

        let relay_worker = vec![msg(
            "worker result",
            serde_json::json!({ "relay_from": "worker-123" }),
        )];
        let out_worker = format_for_agent(&relay_worker);
        assert!(!out_worker.contains("The user sent you"));
        assert!(out_worker.contains(
            "Worker/hub event (NOT a user message). Act on it now: continue the card's flow"
        ));
        assert!(out_worker.contains("---\nworker result\n"));
    }

    #[test]
    fn format_for_agent_mixed() {
        let mixed = vec![
            msg("user message 1", serde_json::json!({})),
            msg(
                "worker event 1",
                serde_json::json!({ "relay_from": "worker-456" }),
            ),
            msg("user message 2", serde_json::json!({})),
            msg("hub event 2", serde_json::json!({ "relay_from": null })),
        ];
        let out = format_for_agent(&mixed);
        assert!(out.contains("The user sent you messages through the jg-team-dev app. Treat them as the user's own words;"));
        assert!(out.contains(
            "Worker/hub event (NOT a user message). Act on it now: continue the card's flow"
        ));
        assert!(out.contains("---\nuser message 1\n"));
        assert!(out.contains("---\nuser message 2\n"));
        assert!(out.contains("---\nworker event 1\n"));
        assert!(out.contains("---\nhub event 2\n"));

        let user_idx = out.find("The user sent you").unwrap();
        let event_idx = out.find("Worker/hub event").unwrap();
        assert!(user_idx < event_idx);
    }
}
