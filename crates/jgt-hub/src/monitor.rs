//! Background checks: notices dead harness processes, deleted transcripts and expired
//! questions, so the UI always shows the truth even when no hook fired.

use crate::state::SharedHub;
use jgt_core::{Harness, SessionStatus, procs};
use std::time::Duration;

pub fn start(hub: SharedHub) {
    tokio::spawn(async move {
        let mut tick: u64 = 0;
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        loop {
            interval.tick().await;
            if let Err(e) = check(&hub, tick.is_multiple_of(10)) {
                tracing::warn!("monitor: {e:?}");
            }
            tick = tick.wrapping_add(1);
        }
    });
}

fn check(hub: &SharedHub, slow: bool) -> Result<(), crate::state::HubError> {
    for session in hub.db.connected_sessions()? {
        if let Some(pid) = session.pid
            && !procs::is_alive(pid, session.pid_start) {
                tracing::info!("session {} lost its process {pid}", session.id);
                hub.mark_disconnected(&session, "process exited")?;
            }
    }
    if slow {
        for session in hub.db.sessions_with_status(SessionStatus::Disconnected)? {
            let gone = session.harness == Harness::ClaudeCode
                && session.transcript_path.as_deref().is_some_and(|p| !std::path::Path::new(p).exists());
            if gone {
                hub.mark_ended(&session, "transcript deleted")?;
            }
        }
        hub.archive_done_tasks()?;
        for coordinator in hub.resume_usage_limits()? {
            wake(coordinator);
        }
    }
    hub.expire_questions()?;
    hub.check_reconnects()?;
    Ok(())
}

/// Wakes a sleeping Coordinator after a usage limit reset (E7). The one place the hub starts an
/// agent: no app or CLI is there to do it at the reset time. It runs the same reconnect plan the
/// app would, in the background; the queued message reaches the Coordinator either way.
fn wake(session: jgt_core::Session) {
    if !(session.is_coordinator() && session.status == SessionStatus::Disconnected) {
        return;
    }
    tokio::spawn(async move {
        let client = jgt_core::HubClient::default();
        match jgt_core::launch::reconnect(&client, &session, jgt_core::ReconnectMode::Background).await {
            Ok(started) => tracing::info!("woke {} after the usage limit: {started}", session.id),
            Err(e) => tracing::warn!("could not wake {} after the usage limit: {e}", session.id),
        }
    });
}
