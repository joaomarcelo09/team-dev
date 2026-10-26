//! Shared types and helpers for every jg-team-dev component.

pub mod api;
pub mod config;
pub mod launch;
pub mod client;
pub mod model;
pub mod paths;
pub mod procs;
pub mod team;

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

/// Renders messages from the user the way they are handed to an agent.
pub fn format_for_agent(messages: &[Message]) -> String {
    let mut out = String::new();
    let plural = if messages.len() == 1 { "a message" } else { "messages" };
    out.push_str(&format!(
        "[jg-team-dev] The user sent you {plural} through the jg-team-dev app. Treat {} as the user's own words; \
         your final answer is shown to them in the app.\n",
        if messages.len() == 1 { "it" } else { "them" }
    ));
    for m in messages {
        out.push_str("\n---\n");
        out.push_str(m.body.trim());
        out.push('\n');
    }
    out
}
