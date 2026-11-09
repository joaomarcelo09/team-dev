//! Behaviour tests for the hub's message routing rules.

use crate::{
    db::Db,
    state::{Hub, HubError, SharedHub},
};
use jgt_core::{
    Delivery, Direction, Harness, MessageKind, QuestionStatus, ReconnectMode, SessionStatus, api::*, config::Config,
};
use std::time::Duration;

fn hub() -> SharedHub {
    let mut config = Config::default();
    config.notifications.enabled = false;
    config.git.auto_commit = true;
    config.git.auto_merge = true;
    config.git.auto_push = true;
    Hub::new(Db::open(std::path::Path::new(":memory:")).unwrap(), config)
}

fn register(hub: &Hub, harness: Harness, ext: &str, pid: Option<i64>) -> jgt_core::Session {
    hub.register(&RegisterRequest {
        harness,
        external_id: ext.into(),
        cwd: Some("/tmp/project".into()),
        title: None,
        pid,
        transcript_path: None,
        source: Some("startup".into()),
        role: None,
        parent: None,
    })
    .unwrap()
}

fn say(hub: &Hub, id: &str, kind: MessageKind, body: &str) {
    hub.agent_message(id, &AgentMessageRequest { kind, body: body.into(), meta: serde_json::Value::Null }).unwrap();
}

#[test]
fn register_is_idempotent_and_keeps_the_same_chat() {
    let hub = hub();
    let a = register(&hub, Harness::ClaudeCode, "abc", None);
    let b = register(&hub, Harness::ClaudeCode, "abc", None);
    assert_eq!(a.id, b.id);
    let other = register(&hub, Harness::Opencode, "abc", None);
    assert_ne!(a.id, other.id, "the same id in another harness is another session");
    assert_eq!(hub.db.sessions(true).unwrap().len(), 2);
}

#[test]
fn sessions_by_prefix_matches_alphanumeric_prefix_only() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    // Exact match
    let matches = hub.db.sessions_by_prefix(&s.id).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].id, s.id);

    // 10-char prefix match
    let matches = hub.db.sessions_by_prefix(&s.id[..10]).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].id, s.id);

    // Non-alphanumeric rejected (returns empty)
    assert!(hub.db.sessions_by_prefix("").unwrap().is_empty());
    assert!(hub.db.sessions_by_prefix("%").unwrap().is_empty());
    assert!(hub.db.sessions_by_prefix("_").unwrap().is_empty());
    assert!(hub.db.sessions_by_prefix(&format!("{}%", &s.id[..5])).unwrap().is_empty());
}

#[test]
fn messages_queue_until_taken_and_are_taken_once() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    hub.send_from_user(&s.id, "first").unwrap();
    hub.send_from_user(&s.id, "second").unwrap();
    assert_eq!(hub.session(&s.id).unwrap().queued, 2);

    let taken = hub.take(&s.id).unwrap();
    assert_eq!(taken.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), ["first", "second"]);
    assert!(taken.iter().all(|m| m.delivery == Delivery::Delivered));
    assert!(hub.take(&s.id).unwrap().is_empty());
    let s = hub.session(&s.id).unwrap();
    assert_eq!(s.queued, 0);
    assert!(s.awaiting_reply);
}

#[test]
fn messages_to_a_disconnected_session_are_kept() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    hub.end(&s.id, &EndRequest { reason: "exit".into(), gone: false }).unwrap();
    assert_eq!(hub.session(&s.id).unwrap().status, SessionStatus::Disconnected);
    hub.send_from_user(&s.id, "while away").unwrap();
    // The agent comes back (resume) and receives it.
    let back = register(&hub, Harness::ClaudeCode, "s1", None);
    assert_eq!(back.id, s.id);
    assert_eq!(back.status, SessionStatus::Idle);
    assert_eq!(hub.take(&s.id).unwrap()[0].body, "while away");
}

#[test]
fn ended_sessions_are_read_only_and_fail_queued_messages() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    let queued = hub.send_from_user(&s.id, "never delivered").unwrap();
    hub.end(&s.id, &EndRequest { reason: "context deleted".into(), gone: true }).unwrap();

    let s = hub.session(&s.id).unwrap();
    assert_eq!(s.status, SessionStatus::Ended);
    assert_eq!(hub.db.message(queued.id).unwrap().unwrap().delivery, Delivery::Undeliverable);
    assert!(matches!(hub.send_from_user(&s.id, "hello?"), Err(HubError::Gone(_))));
    assert!(matches!(
        hub.prepare_reconnect(&s.id, &ReconnectRequest { mode: ReconnectMode::Background }),
        Err(HubError::Gone(_))
    ));
}

#[test]
fn final_reply_is_mirrored_only_for_turns_started_from_the_app() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    let idle = |reply: &str| StateRequest { status: SessionStatus::Idle, last_reply: Some(reply.into()) };
    let replies = |hub: &Hub| {
        hub.messages(&s.id, None, 100).unwrap().into_iter().filter(|m| m.kind == MessageKind::Reply).count()
    };

    // A turn the user started in the terminal: not mirrored.
    hub.set_state(&s.id, &idle("terminal answer")).unwrap();
    assert_eq!(replies(&hub), 0);

    // A turn started by a jg-team-dev message: mirrored once.
    hub.send_from_user(&s.id, "question from the app").unwrap();
    hub.take(&s.id).unwrap();
    hub.set_state(&s.id, &idle("app answer")).unwrap();
    assert_eq!(replies(&hub), 1);
    hub.set_state(&s.id, &idle("next terminal answer")).unwrap();
    assert_eq!(replies(&hub), 1);

    // The agent already answered through chat_send: no duplicate.
    hub.send_from_user(&s.id, "another").unwrap();
    hub.take(&s.id).unwrap();
    say(&hub, &s.id, MessageKind::Text, "answered with chat_send");
    hub.set_state(&s.id, &idle("same answer again")).unwrap();
    assert_eq!(replies(&hub), 1);

    // "All replies" mirrors everything.
    hub.patch(&s.id, &PatchSessionRequest { mirror: Some(true), ..Default::default() }, true).unwrap();
    hub.set_state(&s.id, &idle("mirrored anyway")).unwrap();
    assert_eq!(replies(&hub), 2);
}

#[tokio::test]
async fn wait_returns_as_soon_as_the_user_writes() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    let waiter = {
        let hub = hub.clone();
        let id = s.id.clone();
        tokio::spawn(async move { hub.wait_messages(&id, Duration::from_secs(10)).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hub.session(&s.id).unwrap().status, SessionStatus::Listening);
    hub.send_from_user(&s.id, "ping").unwrap();
    let got = tokio::time::timeout(Duration::from_secs(2), waiter).await.unwrap().unwrap().unwrap();
    assert_eq!(got[0].body, "ping");
    assert_eq!(hub.session(&s.id).unwrap().status, SessionStatus::Active);
}

#[tokio::test]
async fn wait_times_out_back_to_idle() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    let got = hub.wait_messages(&s.id, Duration::from_millis(200)).await.unwrap();
    assert!(got.is_empty());
    assert_eq!(hub.session(&s.id).unwrap().status, SessionStatus::Idle);
}

#[tokio::test]
async fn questions_are_answered_once() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    let q = hub
        .ask(&s.id, &QuestionRequest {
            kind: MessageKind::Permission,
            prompt: "Allow Bash?".into(),
            options: vec!["Allow".into(), "Deny".into()],
            meta: serde_json::Value::Null,
            timeout_secs: Some(60),
        })
        .unwrap();
    let waiter = {
        let hub = hub.clone();
        let id = q.id.clone();
        tokio::spawn(async move { hub.wait_answer(&id, Duration::from_secs(10)).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    hub.answer(&q.id, "Allow").unwrap();
    let answered = waiter.await.unwrap().unwrap();
    assert_eq!(answered.status, QuestionStatus::Answered);
    assert_eq!(answered.answer.as_deref(), Some("Allow"));
    assert!(matches!(hub.answer(&q.id, "Deny"), Err(HubError::Conflict(_))));
}

#[test]
fn disconnecting_expires_pending_questions() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    let q = hub
        .ask(&s.id, &QuestionRequest {
            kind: MessageKind::Question,
            prompt: "Deploy?".into(),
            options: vec![],
            meta: serde_json::Value::Null,
            timeout_secs: None,
        })
        .unwrap();
    hub.end(&s.id, &EndRequest { reason: "exit".into(), gone: false }).unwrap();
    assert_eq!(hub.db.question(&q.id).unwrap().unwrap().status, QuestionStatus::Expired);
}

#[test]
fn reconnect_needs_a_disconnected_session_and_keeps_messages_queued() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    let req = ReconnectRequest { mode: ReconnectMode::Background };
    assert!(matches!(hub.prepare_reconnect(&s.id, &req), Err(HubError::Conflict(_))));

    hub.end(&s.id, &EndRequest { reason: "exit".into(), gone: false }).unwrap();
    let ticket = hub.prepare_reconnect(&s.id, &req).unwrap();
    assert!(ticket.prompt.is_none(), "nothing waiting, no prompt");
    assert!(ticket.session.listen, "background sessions keep listening");

    hub.send_from_user(&s.id, "hi").unwrap();
    let ticket = hub.prepare_reconnect(&s.id, &req).unwrap();
    assert!(ticket.prompt.unwrap().contains("1 message"));
    // Delivery happens only when the restarted agent takes them.
    assert_eq!(hub.session(&s.id).unwrap().queued, 1);

    let custom = register(&hub, Harness::Custom, "script", None);
    hub.end(&custom.id, &EndRequest { reason: "exit".into(), gone: false }).unwrap();
    assert!(matches!(hub.prepare_reconnect(&custom.id, &req), Err(HubError::BadRequest(_))));
}

#[tokio::test]
async fn pending_lists_idle_sessions_of_a_process() {
    let hub = hub();
    let pid = std::process::id() as i64;
    let a = register(&hub, Harness::Opencode, "a", Some(pid));
    let b = register(&hub, Harness::Opencode, "b", Some(pid));
    hub.set_state(&b.id, &StateRequest { status: SessionStatus::Active, last_reply: None }).unwrap();
    hub.send_from_user(&a.id, "for a").unwrap();
    hub.send_from_user(&b.id, "for busy b").unwrap();
    let pending = hub.pending_for_pid(pid, Duration::from_millis(100)).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].external_id, "a");
}

#[test]
fn unread_counts_agent_messages_until_read() {
    let hub = hub();
    let s = register(&hub, Harness::ClaudeCode, "s1", None);
    say(&hub, &s.id, MessageKind::Text, "one");
    say(&hub, &s.id, MessageKind::Notify, "two");
    assert_eq!(hub.session(&s.id).unwrap().unread, 2);
    hub.mark_read(&s.id).unwrap();
    assert_eq!(hub.session(&s.id).unwrap().unread, 0);
    let last = hub.messages(&s.id, None, 10).unwrap().pop().unwrap();
    assert_eq!(last.direction, Direction::FromAgent);
    assert!(last.read_at.is_some());
}

#[test]
fn coordinators_listen_from_the_start() {
    let hub = hub();
    let req = |ext: &str, role: Option<&str>| RegisterRequest {
        harness: Harness::ClaudeCode,
        external_id: ext.into(),
        cwd: Some("/tmp/project".into()),
        title: None,
        pid: None,
        transcript_path: None,
        source: Some("startup".into()),
        role: role.map(str::to_owned),
        parent: None,
    };
    let coordinator = hub.register(&req("coord", Some(jgt_core::COORDINATOR))).unwrap();
    assert!(coordinator.is_coordinator());
    assert!(coordinator.listen, "the Coordinator only hears the user through the app");

    let plain = hub.register(&req("plain", None)).unwrap();
    assert_eq!(plain.role, None);
    assert!(!plain.listen);

    // A resume registers again without the env var; the role stays.
    let again = hub.register(&req("coord", None)).unwrap();
    assert!(again.is_coordinator());
}

#[test]
    fn empty_registration_keeps_role_coordinator() {
        let hub = hub();
        let req = |role| RegisterRequest {
            harness: Harness::ClaudeCode,
            external_id: "coord-session".into(),
            cwd: Some("/tmp/folder-x".into()),
            title: Some("coord · folder-x".into()),
            pid: None,
            transcript_path: None,
            source: Some("startup".into()),
            role,
            parent: None,
        };
        let s = hub.register(&req(Some(jgt_core::COORDINATOR.into()))).unwrap();
        assert_eq!(s.role.as_deref(), Some(jgt_core::COORDINATOR));

        // Re-register with empty role:
        let s2 = hub.register(&req(None)).unwrap();
        assert_eq!(s2.role.as_deref(), Some(jgt_core::COORDINATOR));
    }

    #[test]
    fn claim_coordinator_sets_role_and_refuses_second_live_coordinator() {
        let hub = hub();
        let make_sess = |ext: &str, cwd: &str, role: Option<&str>| {
            hub.register(&RegisterRequest {
                harness: Harness::ClaudeCode,
                external_id: ext.into(),
                cwd: Some(cwd.into()),
                title: Some(format!("coord · {ext}")),
                pid: None,
                transcript_path: None,
                source: Some("startup".into()),
                role: role.map(str::to_owned),
                parent: None,
            })
            .unwrap()
        };

        let folder_a = "/tmp/test-folder-a";
        let folder_b = "/tmp/test-folder-b";

        // Session 1 in folder A with no role claims coordinator:
        let s1 = make_sess("s1", folder_a, None);
        assert_eq!(s1.role, None);
        let s1_claimed = hub.claim_coordinator(&s1.id).unwrap();
        assert_eq!(s1_claimed.role.as_deref(), Some(jgt_core::COORDINATOR));
        assert!(s1_claimed.listen);

        // Re-claiming coordinator on an existing Coordinator session is allowed:
        let s1_reclaimed = hub.claim_coordinator(&s1.id).unwrap();
        assert_eq!(s1_reclaimed.role.as_deref(), Some(jgt_core::COORDINATOR));

        // Session 2 in folder A with no role tries to claim coordinator while s1 is live (idle/listening):
        let s2 = make_sess("s2", folder_a, None);
        let err = hub.claim_coordinator(&s2.id).unwrap_err();
        match err {
            HubError::Forbidden(msg) => {
                assert!(msg.contains("another Coordinator is already running for /tmp/test-folder-a"));
            }
            other => panic!("expected Forbidden, got {other:?}"),
        }

        // Session 3 in folder B (different folder) succeeds:
        let s3 = make_sess("s3", folder_b, None);
        let s3_claimed = hub.claim_coordinator(&s3.id).unwrap();
        assert_eq!(s3_claimed.role.as_deref(), Some(jgt_core::COORDINATOR));

        // If Session 1 in folder A ends, Session 2 can now claim coordinator:
        hub.end(&s1.id, &EndRequest { reason: "ended".into(), gone: false }).unwrap();
        let s2_claimed = hub.claim_coordinator(&s2.id).unwrap();
        assert_eq!(s2_claimed.role.as_deref(), Some(jgt_core::COORDINATOR));

        // A worker session (e.g. coder) cannot claim coordinator even in an uncoordinated folder:
        let w = make_sess("w1", "/tmp/test-folder-c", Some("coder"));
        assert!(w.is_worker());
        let err = hub.claim_coordinator(&w.id).unwrap_err();
        match err {
            HubError::Forbidden(msg) => {
                assert!(msg.contains("coder worker, not a Coordinator"), "unexpected message: {msg}");
            }
            other => panic!("expected Forbidden, got {other:?}"),
        }

        // A reviewer worker is also refused:
        let w2 = make_sess("w2", "/tmp/test-folder-d", Some("reviewer"));
        assert!(w2.is_worker());
        let err2 = hub.claim_coordinator(&w2.id).unwrap_err();
        match err2 {
            HubError::Forbidden(msg) => {
                assert!(msg.contains("reviewer worker, not a Coordinator"), "unexpected message: {msg}");
            }
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

/// The sessions schema before version 7 added what a worker runs on (`model`, `effort`, tokens).
fn schema_6() -> String {
    ["model           TEXT", "effort          TEXT", "tokens_in       INTEGER", "tokens_out      INTEGER"]
        .iter()
        .fold(crate::db::SCHEMA.to_string(), |s, col| s.replace(&format!("    {col},\n"), ""))
}

#[test]
fn schema_1_databases_are_upgraded() {
    let dir = std::env::temp_dir().join(format!("jgt-migrate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("old.db");
    let _ = std::fs::remove_file(&path);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        let v1 = schema_6().replace("    role            TEXT,\n", "").replace("    parent_id       TEXT,\n", "")
            .replace("    cost_usd        REAL,\n", "")
            .replace("    run_secs        INTEGER,\n", "");
        conn.execute_batch(&v1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, harness, external_id, status, created_at, updated_at, last_seen_at)
             VALUES ('s1', 'claude-code', 'ext', 'idle', 0, 0, 0)",
            [],
        )
        .unwrap();
    }
    let db = Db::open(&path).unwrap();
    let session = db.session("s1").unwrap().unwrap();
    assert_eq!(session.role, None);
    assert_eq!(session.parent_id, None);
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, crate::db::SCHEMA_VERSION);
    assert_eq!((session.cost_usd, session.run_secs), (None, None));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workers_link_to_their_coordinator_and_relay_results() {
    let hub = hub();
    let req = |ext: &str, role: &str, parent: Option<String>| RegisterRequest {
        harness: Harness::ClaudeCode,
        external_id: ext.into(),
        cwd: Some("/tmp/project".into()),
        title: None,
        pid: None,
        transcript_path: None,
        source: Some("startup".into()),
        role: Some(role.into()),
        parent,
    };
    let coordinator = hub.register(&req("coord", jgt_core::COORDINATOR, None)).unwrap();
    let worker = hub.register(&req("w1", "coder", Some(coordinator.id.clone()))).unwrap();
    assert!(worker.is_worker());
    assert!(!worker.listen, "workers never listen");
    assert_eq!(worker.parent_id.as_deref(), Some(coordinator.id.as_str()));

    say(&hub, &coordinator.id, MessageKind::Text, "unread greeting");
    let relayed = hub
        .relay(&coordinator.id, &RelayRequest { from_session: Some(worker.id.clone()), text: "Worker finished".into() })
        .unwrap();
    assert_eq!(relayed.direction, Direction::ToAgent);
    assert_eq!(relayed.meta_str("relay_from"), Some(worker.id.as_str()));
    assert_eq!(hub.session(&coordinator.id).unwrap().unread, 1, "a relay does not mark the chat read");

    let taken = hub.take(&coordinator.id).unwrap();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].body, "Worker finished");
    assert!(hub.relay(&coordinator.id, &RelayRequest { from_session: None, text: "  ".into() }).is_err());
}

#[test]
fn a_reset_retires_the_old_coordinator() {
    let hub = hub();
    let old = hub
        .register(&RegisterRequest {
            harness: Harness::ClaudeCode,
            external_id: "old".into(),
            cwd: Some("/tmp/project".into()),
            title: None,
            pid: None,
            transcript_path: None,
            source: Some("startup".into()),
            role: Some(jgt_core::COORDINATOR.into()),
            parent: None,
        })
        .unwrap();
    assert!(!old.is_retired());
    let retired = hub
        .end(&old.id, &EndRequest { reason: format!("{}: a fresh Coordinator took over", jgt_core::RETIRED), gone: true })
        .unwrap();
    assert!(retired.is_retired());
    assert_eq!(retired.status, SessionStatus::Ended);
    // A plain end is not a retirement.
    let other = register(&hub, Harness::ClaudeCode, "plain", None);
    let ended = hub.end(&other.id, &EndRequest { reason: "transcript deleted".into(), gone: true }).unwrap();
    assert!(!ended.is_retired());
}

#[test]
fn a_finished_run_records_cost_and_duration() {
    let hub = hub();
    let worker = register(&hub, Harness::ClaudeCode, "w", None);
    let patched = hub
        .patch(&worker.id, &PatchSessionRequest { run_secs: Some(95), cost_usd: Some(0.42), ..Default::default() }, false)
        .unwrap();
    assert_eq!((patched.cost_usd, patched.run_secs), (Some(0.42), Some(95)));
    // Other patches leave them alone.
    let patched = hub.patch(&worker.id, &PatchSessionRequest { archived: Some(true), ..Default::default() }, false).unwrap();
    assert_eq!((patched.cost_usd, patched.run_secs, patched.archived), (Some(0.42), Some(95), true));
}

#[test]
fn schema_4_databases_are_upgraded_without_data_loss() {
    let dir = std::env::temp_dir().join(format!("jgt-migrate4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("v4.db");
    let _ = std::fs::remove_file(&path);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&schema_6()).unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, harness, external_id, cwd, status, created_at, updated_at, last_seen_at, role)
                VALUES ('s1', 'claude-code', 'ext', '/work/app', 'idle', 1, 2, 3, 'coordinator');
             INSERT INTO messages (id, session_id, direction, kind, body, delivery, created_at)
                VALUES (7, 's1', 'from_agent', 'question', 'Deploy?', 'delivered', 4);
             INSERT INTO questions (id, session_id, message_id, kind, prompt, status, created_at)
                VALUES ('q1', 's1', 7, 'question', 'Deploy?', 'pending', 4);",
        )
        .unwrap();
    }
    let before = |conn: &rusqlite::Connection| -> Vec<String> {
        ["SELECT id || harness || external_id || cwd || status || created_at || role FROM sessions",
         "SELECT id || session_id || direction || kind || body || delivery || created_at FROM messages",
         "SELECT id || session_id || message_id || kind || prompt || status || created_at FROM questions"]
        .iter()
        .map(|q| conn.query_row(q, [], |r| r.get::<_, String>(0)).unwrap())
        .collect()
    };
    let rows = before(&rusqlite::Connection::open(&path).unwrap());
    for _ in 0..2 {
        // The second open is a no-op.
        let db = Db::open(&path).unwrap();
        let hub = Hub::new(db, Config::default());
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0)).unwrap(), crate::db::SCHEMA_VERSION);
        assert_eq!(before(&conn), rows);
        assert_eq!(hub.session("s1").unwrap().role.as_deref(), Some("coordinator"));
        assert_eq!(hub.db.question("q1").unwrap().unwrap().prompt, "Deploy?");
        let task = hub
            .create_task(&CreateTaskRequest {
                actor: ActorRequest::User,
                workspace: "/work/app".into(),
                title: "After the upgrade".into(),
                description: String::new(),
                priority: None,
                status: None,
                parent_id: None,
            })
            .unwrap();
        assert!(task.key.starts_with("JGT-"));
    }
    let hub = Hub::new(Db::open(&path).unwrap(), Config::default());
    assert_eq!(hub.tasks("/work/app", false).unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn round_limit_tasks_move_to_review_on_upgrade() {
    let dir = std::env::temp_dir().join(format!("jgt-upgrade6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hub.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&schema_6()).unwrap();
        conn.execute_batch(crate::db::TASKS_SCHEMA).unwrap();
        let waiting = r#"{"kind":"round_limit","reason":"1 critical finding(s) after round 3","since":1}"#;
        conn.execute(
            "INSERT INTO tasks (id, workspace, number, title, status, position, waiting, review_rounds, created_at, updated_at)
             VALUES ('t1', '/w', 1, 'stuck', 'review', 0, ?1, 2, 1, 1),
                    ('t2', '/w', 2, 'asking', 'working', 0, '{\"kind\":\"question\",\"reason\":\"which?\",\"since\":1}', 0, 1, 1)",
            [waiting],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
    }
    let hub = Hub::new(Db::open(&path).unwrap(), Config::default());
    let stuck = hub.task("t1", None).unwrap();
    assert_eq!((stuck.status, stuck.waiting), (jgt_core::TaskStatus::Review, None));
    let asking = hub.task("t2", None).unwrap();
    assert_eq!(asking.waiting.map(|w| w.kind), Some(jgt_core::WaitingKind::Question));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn schema_7_orchestrator_review_tasks_move_to_review_on_upgrade() {
    let dir = std::env::temp_dir().join(format!("jgt-upgrade8-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hub.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn.execute_batch(crate::db::TASKS_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO tasks (id, workspace, number, title, status, position, created_at, updated_at)
             VALUES ('t1', '/w', 1, 'in_orch_review', 'orchestrator_review', 0, 1, 1)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 7).unwrap();
    }
    let hub = Hub::new(Db::open(&path).unwrap(), Config::default());
    let task = hub.task("t1", None).unwrap();
    assert_eq!(task.status, jgt_core::TaskStatus::Review);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, crate::db::SCHEMA_VERSION);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn schema_8_database_with_review_task_opens_at_version_9_with_task_unchanged() {
    let dir = std::env::temp_dir().join(format!("jgt-upgrade9-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hub.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn.execute_batch(crate::db::TASKS_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO tasks (id, workspace, number, title, status, position, created_at, updated_at)
             VALUES ('t1', '/w', 1, 'in_review_task', 'review', 0, 1, 1)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 8).unwrap();
    }
    let hub = Hub::new(Db::open(&path).unwrap(), Config::default());
    let task = hub.task("t1", None).unwrap();
    assert_eq!(task.status, jgt_core::TaskStatus::Review);
    assert_eq!(task.title, "in_review_task");
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, 9);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── board ───────────────────────────────────────────────────────────────────

mod board {
    use super::*;
    use jgt_core::{
        Actor, Event, FindingCategory, ROUND_LIMIT, ReviewDecision, Session, Task, TaskEvent, TaskEventKind, TaskPriority, TaskStatus,
        TestStatus, WaitingKind,
    };
    use TaskStatus::*;

    const WS: &str = "/tmp/jgt-board-test/app";

    fn session(hub: &Hub, ext: &str, role: &str, cwd: &str, parent: Option<&str>) -> Session {
        hub.register(&RegisterRequest {
            harness: Harness::ClaudeCode,
            external_id: ext.into(),
            cwd: Some(cwd.into()),
            title: None,
            pid: None,
            transcript_path: None,
            source: Some("startup".into()),
            role: Some(role.into()),
            parent: parent.map(str::to_owned),
        })
        .unwrap()
    }

    fn session_with_id(
        hub: &Hub,
        id: &str,
        ext: &str,
        role: &str,
        cwd: &str,
        parent: Option<&str>,
    ) -> Session {
        let s = hub
            .db
            .insert_session_with_id(id, Harness::ClaudeCode, ext, Some(role), Some(cwd), parent)
            .unwrap();
        hub.emit_session(id);
        s
    }

    fn as_session(s: &Session) -> ActorRequest {
        ActorRequest::Session { session: s.id.clone() }
    }

    /// A hub with a Coordinator for `WS`, its coder and its reviewer.
    struct Team {
        hub: SharedHub,
        coord: Session,
        coder: Session,
        reviewer: Session,
    }

    fn team() -> Team {
        let hub = hub();
        let coord = session(&hub, "coord", jgt_core::COORDINATOR, WS, None);
        let coder = session(&hub, "coder", "coder", WS, Some(&coord.id));
        let reviewer = session(&hub, "reviewer", "reviewer", WS, Some(&coord.id));
        Team { hub, coord, coder, reviewer }
    }

    fn finding(critical: Option<bool>, category: Option<FindingCategory>) -> FindingInput {
        FindingInput {
            critical,
            category,
            severity: None,
            location: "src/lib.rs:1".into(),
            problem: "a problem".into(),
            required_action: "fix it".into(),
        }
    }

    fn minor() -> FindingInput {
        finding(Some(false), None)
    }

    fn critical() -> FindingInput {
        finding(Some(true), Some(FindingCategory::Security))
    }

    /// A high finding: not critical, but it starts the fix round (PRD E3, Q13).
    fn high() -> FindingInput {
        FindingInput { severity: Some("high".into()), ..finding(Some(false), Some(FindingCategory::Behavior)) }
    }

    fn medium_finding() -> FindingInput {
        FindingInput { severity: Some("medium".into()), ..finding(Some(false), None) }
    }

    fn low_finding() -> FindingInput {
        FindingInput { severity: Some("low".into()), ..finding(Some(false), None) }
    }

    fn conflict<T: std::fmt::Debug>(r: Result<T, HubError>) -> String {
        match r {
            Err(HubError::Conflict(why)) => why,
            other => panic!("expected 409, got {other:?}"),
        }
    }

    fn forbidden<T: std::fmt::Debug>(r: Result<T, HubError>) {
        assert!(matches!(r, Err(HubError::Forbidden(_))), "expected 403, got {r:?}");
    }

    fn bad_request<T: std::fmt::Debug>(r: Result<T, HubError>) {
        assert!(matches!(r, Err(HubError::BadRequest(_))), "expected 400, got {r:?}");
    }

    impl Team {
        fn coordinator(&self) -> ActorRequest {
            as_session(&self.coord)
        }

        fn create(&self, actor: ActorRequest, title: &str, status: Option<TaskStatus>) -> Result<Task, HubError> {
            self.hub.create_task(&CreateTaskRequest {
                actor,
                workspace: WS.into(),
                title: title.into(),
                description: "the user's words".into(),
                priority: None,
                status,
                parent_id: None,
            })
        }

        /// A task the user created and queued.
        fn queued(&self, title: &str) -> Task {
            let t = self.create(ActorRequest::User, title, None).unwrap();
            self.user_move(&t, OnQueue);
            self.get(&t)
        }

        fn get(&self, t: &Task) -> Task {
            self.hub.task(&t.id, None).unwrap()
        }

        fn events(&self, t: &Task) -> Vec<TaskEvent> {
            self.hub.task_events(&t.id, None).unwrap()
        }

        fn move_as(&self, actor: ActorRequest, t: &Task, to: TaskStatus) -> Result<Task, HubError> {
            self.hub
                .move_task(&t.id, None, &MoveTaskRequest { actor, to, position: None, reason: None })
                .map(|r| r.task)
        }

        fn mv(&self, t: &Task, to: TaskStatus) -> Result<Task, HubError> {
            self.move_as(self.coordinator(), t, to)
        }

        fn mv_full(&self, t: &Task, to: TaskStatus) -> Result<MoveResponse, HubError> {
            self.hub.move_task(&t.id, None, &MoveTaskRequest {
                actor: self.coordinator(),
                to,
                position: None,
                reason: None,
            })
        }

        fn mv_with_reason(&self, t: &Task, to: TaskStatus, reason: &str) -> Result<MoveResponse, HubError> {
            self.hub.move_task(&t.id, None, &MoveTaskRequest {
                actor: self.coordinator(),
                to,
                position: None,
                reason: Some(reason.into()),
            })
        }

        fn user_move(&self, t: &Task, to: TaskStatus) -> Task {
            self.move_as(ActorRequest::User, t, to).unwrap()
        }

        fn user_mv_full(&self, t: &Task, to: TaskStatus) -> Result<MoveResponse, HubError> {
            self.hub.move_task(&t.id, None, &MoveTaskRequest {
                actor: ActorRequest::User,
                to,
                position: None,
                reason: None,
            })
        }

        fn set_git_switches(&self, auto_commit: bool, auto_merge: bool, auto_push: bool) {
            let mut config = self.hub.config();
            config.git.auto_commit = auto_commit;
            config.git.auto_merge = auto_merge;
            config.git.auto_push = auto_push;
            *self.hub.config.write().unwrap() = config;
        }

        fn test(&self, t: &Task, status: TestStatus) -> Result<Task, HubError> {
            self.hub.record_results(&t.id, None, &ResultRequest {
                actor: self.coordinator(),
                test: Some(TestResultInput {
                    status,
                    worker: self.coder.id.clone(),
                    summary: format!("tests {status}"),
                    failures: vec![],
                }),
                review: None,
            })
        }

        fn review(&self, t: &Task, decision: ReviewDecision, findings: Vec<FindingInput>) -> Result<Task, HubError> {
            self.hub.record_results(&t.id, None, &ResultRequest {
                actor: self.coordinator(),
                test: None,
                review: Some(ReviewResultInput { decision, worker: self.reviewer.id.clone(), findings }),
            })
        }

        /// From Working: a green test, then the review.
        fn pass(&self, t: &Task, decision: ReviewDecision, findings: Vec<FindingInput>) -> Task {
            self.mv(t, Testing).unwrap();
            self.test(t, TestStatus::Pass).unwrap();
            self.mv(t, Review).unwrap();
            self.review(t, decision, findings).unwrap()
        }

        /// A queued task the Coordinator took to Working.
        fn working(&self, title: &str) -> Task {
            let t = self.queued(title);
            self.mv(&t, Working).unwrap()
        }
    }

    #[test]
    fn task_keys_count_per_workspace_and_are_never_reused() {
        let t = team();
        let one = t.create(ActorRequest::User, "one", None).unwrap();
        let two = t.create(ActorRequest::User, "two", None).unwrap();
        assert_eq!((one.key.as_str(), two.key.as_str()), ("JGT-1", "JGT-2"));
        let other = t
            .hub
            .create_task(&CreateTaskRequest {
                actor: ActorRequest::User,
                workspace: "/tmp/jgt-board-test/other/".into(),
                title: "elsewhere".into(),
                description: String::new(),
                priority: None,
                status: None,
                parent_id: None,
            })
            .unwrap();
        assert_eq!((other.key.as_str(), other.workspace.as_str()), ("JGT-1", "/tmp/jgt-board-test/other"));
        t.hub.delete_task(&two.id, None, &ActorRequest::User).unwrap();
        let three = t.create(ActorRequest::User, "three", None).unwrap();
        assert_eq!(three.key, "JGT-3", "a deleted key is never handed out again");
        assert_eq!(t.hub.task("jgt-3", Some(&format!("{WS}/"))).unwrap().id, three.id);
        assert!(matches!(t.hub.task("JGT-2", Some(WS)), Err(HubError::NotFound(_))));
    }

    #[test]
    fn tasks_list_in_column_and_position_order() {
        let t = team();
        let a = t.create(ActorRequest::User, "a", None).unwrap();
        let b = t.create(ActorRequest::User, "b", Some(OnQueue)).unwrap();
        let c = t.create(ActorRequest::User, "c", None).unwrap();
        let d = t.create(ActorRequest::User, "d", Some(OnQueue)).unwrap();
        let order: Vec<_> = t.hub.tasks(WS, false).unwrap().into_iter().map(|x| (x.title, x.status, x.position)).collect();
        assert_eq!(order, [
            ("c".to_string(), Backlog, 0),
            ("a".to_string(), Backlog, 1),
            ("d".to_string(), OnQueue, 0),
            ("b".to_string(), OnQueue, 1),
        ]);
        let _ = (a, b, c, d);
    }

    #[test]
    fn new_task_lands_at_top_and_renumbers() {
        let t = team();
        let first = t.create(ActorRequest::User, "first", None).unwrap();
        assert_eq!(first.position, 0);

        let second = t.create(ActorRequest::User, "second", None).unwrap();
        assert_eq!(second.position, 0);
        assert_eq!(t.get(&first).position, 1);

        let third = t.create(ActorRequest::User, "third", None).unwrap();
        assert_eq!(third.position, 0);
        assert_eq!(t.get(&second).position, 1);
        assert_eq!(t.get(&first).position, 2);

        let backlog: Vec<(String, i64)> =
            t.hub.tasks(WS, false).unwrap().into_iter().filter(|x| x.status == Backlog).map(|x| (x.title, x.position)).collect();
        assert_eq!(backlog, [
            ("third".to_string(), 0),
            ("second".to_string(), 1),
            ("first".to_string(), 2),
        ]);
    }

    #[test]
    fn moving_renumbers_positions() {
        let t = team();
        let a = t.create(ActorRequest::User, "a", None).unwrap();
        let b = t.create(ActorRequest::User, "b", None).unwrap();
        let c = t.create(ActorRequest::User, "c", Some(OnQueue)).unwrap();
        // Into On Queue, before c.
        t.hub
            .move_task(&a.id, None, &MoveTaskRequest { actor: ActorRequest::User, to: OnQueue, position: Some(0), reason: None })
            .unwrap();
        // Reorder inside On Queue: c to the top.
        t.hub
            .move_task(&c.id, None, &MoveTaskRequest { actor: ActorRequest::User, to: OnQueue, position: Some(0), reason: None })
            .unwrap();
        let titles = |status| -> Vec<(String, i64)> {
            t.hub.tasks(WS, false).unwrap().into_iter().filter(|x| x.status == status).map(|x| (x.title, x.position)).collect()
        };
        assert_eq!(titles(OnQueue), [("c".to_string(), 0), ("a".to_string(), 1)]);
        assert_eq!(titles(Backlog), [("b".to_string(), 0)]);
        let _ = b;
    }

    #[test]
    fn leaving_a_column_keeps_its_positions_contiguous() {
        let t = team();
        let [a, b, c, d, e] = ["a", "b", "c", "d", "e"].map(|x| t.create(ActorRequest::User, x, None).unwrap());
        let column = |status| -> Vec<(String, i64)> {
            t.hub.tasks(WS, false).unwrap().into_iter().filter(|x| x.status == status).map(|x| (x.title, x.position)).collect()
        };
        let expect = |titles: &[&str]| -> Vec<(String, i64)> {
            titles.iter().enumerate().map(|(i, x)| (x.to_string(), i as i64)).collect()
        };
        // A move out of Backlog.
        t.user_move(&b, OnQueue);
        assert_eq!(column(Backlog), expect(&["e", "d", "c", "a"]));
        // An edit of c keeps its place, as the app shows it.
        let mut req = PatchTaskRequest::new(ActorRequest::User);
        req.priority = Some(jgt_core::TaskPriority::High);
        t.hub.patch_task(&c.id, None, &req).unwrap();
        assert_eq!(column(Backlog), expect(&["e", "d", "c", "a"]));
        // A delete.
        t.hub.delete_task(&a.id, None, &ActorRequest::User).unwrap();
        assert_eq!(column(Backlog), expect(&["e", "d", "c"]));
        // Archived tasks take no position: new tasks and moves number the others from 0.
        t.hub.db.board::<_, rusqlite::Error>(|bd| bd.set_columns(&d.id, &[("archived", &true as &dyn rusqlite::ToSql)])).unwrap();
        t.hub
            .move_task(&e.id, None, &MoveTaskRequest { actor: ActorRequest::User, to: Backlog, position: Some(0), reason: None })
            .unwrap();
        assert_eq!(column(Backlog), expect(&["e", "c"]));
        t.user_move(&e, OnQueue);
        assert_eq!(column(Backlog), expect(&["c"]));
        let f = t.create(ActorRequest::User, "f", None).unwrap();
        assert_eq!(f.position, 0);
        assert_eq!(column(Backlog), expect(&["f", "c"]));
        assert_eq!(column(OnQueue), expect(&["b", "e"]));
    }

    #[test]
    fn agents_cannot_edit_the_description() {
        let t = team();
        let task = t.create(ActorRequest::User, "mine", None).unwrap();
        let mut req = PatchTaskRequest::new(t.coordinator());
        req.description = Some("rewritten".into());
        forbidden(t.hub.patch_task(&task.id, None, &req));
        let mut req = PatchTaskRequest::new(t.coordinator());
        req.title = Some("renamed".into());
        forbidden(t.hub.patch_task(&task.id, None, &req));

        let mut req = PatchTaskRequest::new(t.coordinator());
        req.details = Some("## Plan\n1. do it".into());
        req.run_id = Some("20261026-1200-mine".into());
        let task = t.hub.patch_task(&task.id, None, &req).unwrap();
        assert_eq!(task.description, "the user's words");
        assert_eq!(task.details.as_deref(), Some("## Plan\n1. do it"));
        assert_eq!(task.run_id.as_deref(), Some("20261026-1200-mine"));

        let mut req = PatchTaskRequest::new(ActorRequest::User);
        req.description = Some("my new words".into());
        assert_eq!(t.hub.patch_task(&task.id, None, &req).unwrap().description, "my new words");
        let kinds: Vec<_> = t.events(&task).iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [TaskEventKind::Created, TaskEventKind::Details, TaskEventKind::Link, TaskEventKind::Edited]);
    }

    #[test]
    fn workers_are_read_only() {
        let t = team();
        let task = t.create(ActorRequest::User, "task", Some(OnQueue)).unwrap();
        let w = as_session(&t.coder);
        forbidden(t.create(w.clone(), "mine", None));
        let mut req = PatchTaskRequest::new(w.clone());
        req.details = Some("x".into());
        forbidden(t.hub.patch_task(&task.id, None, &req));
        forbidden(t.move_as(w.clone(), &task, Working));
        forbidden(t.hub.delete_task(&task.id, None, &w));
        forbidden(t.hub.add_task_event(&task.id, None, &TaskEventRequest {
            actor: w.clone(),
            kind: TaskEventKind::Comment,
            body: "hi".into(),
            meta: serde_json::Value::Null,
        }));
        // Reading is fine, and so is reporting its own start.
        assert_eq!(t.hub.task(&task.key, Some(WS)).unwrap().id, task.id);
        t.hub
            .add_task_event(&task.id, None, &TaskEventRequest {
                actor: w,
                kind: TaskEventKind::WorkerStarted,
                body: String::new(),
                meta: serde_json::json!({ "worker": "coder" }),
            })
            .unwrap();
        assert_eq!(t.get(&task).current_worker.as_deref(), Some("coder"));
        // Only the user deletes.
        forbidden(t.hub.delete_task(&task.id, None, &t.coordinator()));
    }

    #[test]
    fn only_the_workspace_coordinator_acts_as_coordinator() {
        let t = team();
        let elsewhere = session(&t.hub, "coord2", jgt_core::COORDINATOR, "/tmp/jgt-board-test/other", None);
        forbidden(t.create(as_session(&elsewhere), "x", None));

        let no_role = t
            .hub
            .register(&RegisterRequest {
                harness: Harness::ClaudeCode,
                external_id: "no-role-sess".into(),
                cwd: Some(WS.into()),
                title: Some("no-role-sess".into()),
                pid: None,
                transcript_path: None,
                source: Some("startup".into()),
                role: None,
                parent: None,
            })
            .unwrap();
        let err = t.create(as_session(&no_role), "x", None).unwrap_err();
        match err {
            HubError::Forbidden(msg) => {
                assert_eq!(
                    msg,
                    "this Claude Code session has no jg-team-dev role (JGT_ROLE was not set when it started). Run jg-team-dev claim-coordinator, or restart it with jg-team-dev start."
                );
            }
            other => panic!("expected Forbidden, got {other:?}"),
        }
        // A trailing slash in the Coordinator's folder still matches.
        let hub = hub();
        let coord = session(&hub, "c", jgt_core::COORDINATOR, &format!("{WS}/"), None);
        let req = CreateTaskRequest {
            actor: as_session(&coord),
            workspace: WS.into(),
            title: "ok".into(),
            description: String::new(),
            priority: None,
            status: None,
            parent_id: None,
        };
        hub.create_task(&req).unwrap();
        // Coordinator without trailing slash matching workspace with trailing slash.
        let coord2 = session(&hub, "c2", jgt_core::COORDINATOR, "/tmp/jgt-board-test/no-slash", None);
        let req2 = CreateTaskRequest {
            actor: as_session(&coord2),
            workspace: "/tmp/jgt-board-test/no-slash/".into(),
            title: "ok2".into(),
            description: String::new(),
            priority: None,
            status: None,
            parent_id: None,
        };
        hub.create_task(&req2).unwrap();
        assert_eq!(hub.db.current_coordinator("/tmp/jgt-board-test/no-slash/").unwrap().unwrap().id, coord2.id);
        assert_eq!(hub.db.current_coordinator("/tmp/jgt-board-test/no-slash").unwrap().unwrap().id, coord2.id);
        // Retired (ended) Coordinators are refused.
        hub.end(&coord.id, &EndRequest { reason: format!("{}: reset", jgt_core::RETIRED), gone: true }).unwrap();
        forbidden(hub.create_task(&req));
    }

    #[test]
    fn coordinator_creates_only_in_backlog() {
        let t = team();
        forbidden(t.create(t.coordinator(), "queued", Some(OnQueue)));
        let task = t.create(t.coordinator(), "from the planner", None).unwrap();
        assert_eq!(task.status, Backlog);
        assert_eq!(t.events(&task)[0].actor, Actor::Coordinator { session: t.coord.id.clone() });
        bad_request(t.create(ActorRequest::User, "straight to work", Some(Working)));
    }

    #[test]
    fn coordinator_queues_only_subtasks_of_user_queued_tasks() {
        let t = team();
        // A task the user wrote but did not queue.
        let own = t.create(ActorRequest::User, "the user's", None).unwrap();
        conflict(t.mv(&own, OnQueue));

        let subtask = |parent: &Task| {
            t.hub
                .create_task(&CreateTaskRequest {
                    actor: t.coordinator(),
                    workspace: WS.into(),
                    title: "part".into(),
                    description: String::new(),
                    priority: None,
                    status: None,
                    parent_id: Some(parent.key.clone()),
                })
                .unwrap()
        };
        // Parent not queued by the user: refused.
        let sub = subtask(&own);
        assert_eq!(sub.parent_id.as_deref(), Some(own.id.as_str()));
        conflict(t.mv(&sub, OnQueue));
        // Parent queued by the user and in progress: allowed.
        let big = t.working("big");
        let sub = subtask(&big);
        assert_eq!(t.mv(&sub, OnQueue).unwrap().status, OnQueue);
        // A parent created straight into On Queue by the user counts too.
        let queued = t.create(ActorRequest::User, "queued at once", Some(OnQueue)).unwrap();
        let sub = subtask(&queued);
        t.mv(&sub, OnQueue).unwrap();
    }

    #[test]
    fn wip_limit_blocks_a_second_task() {
        let t = team();
        let first = t.working("first");
        assert_eq!(first.status, Working);
        let second = t.queued("second");
        let why = conflict(t.mv(&second, Working));
        assert!(why.contains("WIP"), "{why}");
        // Once the first is done, the second may start.
        t.pass(&first, ReviewDecision::Approved, vec![]);
        t.mv(&first, Done).unwrap();
        t.mv(&second, Working).unwrap();
        // A higher limit lets two run at once.
        let mut config = Config::default();
        config.notifications.enabled = false;
        config.git.auto_commit = true;
        config.git.auto_merge = true;
        config.git.auto_push = true;
        config.team.wip_limit = 2;
        *t.hub.config.write().unwrap() = config;
        let third = t.queued("third");
        t.mv(&third, Working).unwrap();
        conflict(t.mv(&t.queued("fourth"), Working));
    }

    #[test]
    fn testing_to_review_needs_a_green_test() {
        let t = team();
        let task = t.working("x");
        t.mv(&task, Testing).unwrap();
        conflict(t.mv(&task, Review));
        conflict(t.mv(&task, Working));
        t.test(&task, TestStatus::Pass).unwrap();
        conflict(t.mv(&task, Working));
        assert_eq!(t.mv(&task, Review).unwrap().status, Review);
        // Results only go to the matching column.
        conflict(t.test(&task, TestStatus::Pass));
    }

    #[test]
    fn a_red_test_is_the_fix_round() {
        let t = team();
        let task = t.working("x");
        t.mv(&task, Testing).unwrap();
        t.test(&task, TestStatus::Fail).unwrap();
        conflict(t.mv(&task, Review));
        conflict(t.mv(&task, Done));
        let back = t.mv(&task, Working).unwrap();
        assert_eq!(back.review_rounds, 1, "a red test starts the one automatic fix round");
        assert_eq!(back.test_result.map(|r| r.status), Some(TestStatus::Fail), "the coder sees the failures");
        let task = t.mv(&task, Testing).unwrap();
        assert_eq!(task.test_result, None, "a new pass starts without a test");
        // Red again on the second verification: no second fix, the user decides.
        t.test(&task, TestStatus::Fail).unwrap();
        assert!(conflict(t.mv(&task, Working)).contains("no further automatic fix"));
        conflict(t.mv(&task, Review));
        conflict(t.mv(&task, Done));
    }

    #[test]
    fn done_needs_green_test_and_approved_review() {
        let t = team();
        let task = t.working("x");
        conflict(t.mv(&task, Done));
        t.mv(&task, Testing).unwrap();
        t.test(&task, TestStatus::Pass).unwrap();
        conflict(t.mv(&task, Done));
        t.mv(&task, Review).unwrap();
        // No review yet.
        assert!(conflict(t.mv(&task, Done)).contains("K6"));
        // Changes requested in round 1.
        t.review(&task, ReviewDecision::Changes, vec![high()]).unwrap();
        assert!(conflict(t.mv(&task, Done)).contains("the fix round is still left"));
        let task = t.mv(&task, Working).unwrap();
        assert_eq!(task.review_rounds, 1);
        assert_eq!(task.test_result, None, "the green test of the old pass is gone");
        t.mv(&task, Testing).unwrap();
        t.mv(&task, Working).unwrap_err();
        t.test(&task, TestStatus::Pass).unwrap();
        let task = t.mv(&task, Review).unwrap();
        assert_eq!(task.review_result, None, "the old review is gone");
        let task = t.review(&task, ReviewDecision::Approved, vec![minor()]).unwrap();
        assert_eq!(task.review_result.as_ref().map(|r| r.round), Some(2));
        // Approved at round 2 is accepted directly.
        let done = t.mv(&task, Done).unwrap();
        assert_eq!(done.status, Done);
        assert!(done.done_at.is_some());
        // Done is final for the team.
        conflict(t.mv(&done, OnQueue));
        // The approved review's minor note goes to a follow-up.
        assert_eq!(t.hub.tasks(WS, false).unwrap().len(), 2);
    }

    #[test]
    fn an_approved_review_with_notes_leaves_a_follow_up() {
        let t = team();
        let task = t.working("feature");
        let task = t.pass(&task, ReviewDecision::Approved, vec![minor()]);
        t.mv(&task, Done).unwrap();
        let follow = t.hub.tasks(WS, false).unwrap().into_iter().find(|x| x.id != task.id).unwrap();
        assert_eq!((follow.status, follow.parent_id.as_deref()), (Backlog, Some(task.id.as_str())));
        assert!(follow.description.contains("review round 1"), "{}", follow.description);
        assert!(follow.description.contains("a problem — fix it"));
        // A clean approval leaves none.
        let clean = t.working("clean");
        let clean = t.pass(&clean, ReviewDecision::Approved, vec![]);
        t.mv(&clean, Done).unwrap();
        assert_eq!(t.hub.tasks(WS, false).unwrap().len(), 3);
    }

    #[test]
    fn a_patch_keeps_what_a_move_wrote() {
        let t = team();
        let task = t.working("feature");
        let task = t.pass(&task, ReviewDecision::Changes, vec![high()]);
        // A PATCH that read the task in Review writes after the Review → Working move: only the
        // columns it changes are written, so the move's rounds and cleared test survive.
        let mut stale = t.get(&task);
        stale.details = Some("plan".into());
        let moved = t.mv(&task, Working).unwrap();
        assert_eq!((moved.review_rounds, moved.test_result.is_none()), (1, true));
        t.hub.db.board::<_, rusqlite::Error>(|b| b.set_columns(&stale.id, &[("details", &stale.details)])).unwrap();
        let task = t.get(&task);
        assert_eq!(task.details.as_deref(), Some("plan"));
        assert_eq!((task.review_rounds, task.test_result.is_none(), task.status), (1, true, Working));
        assert_eq!(task.review_result, moved.review_result);

        let mut req = PatchTaskRequest::new(t.coordinator());
        req.details = Some("plan 2".into());
        req.run_id = Some("run".into());
        let task = t.hub.patch_task(&task.id, None, &req).unwrap();
        assert_eq!((task.details.as_deref(), task.run_id.as_deref()), (Some("plan 2"), Some("run")));
        assert_eq!((task.review_rounds, task.round_limit, task.test_result.is_none()), (1, 2, true));
        assert_eq!(task.review_result, moved.review_result);
    }

    #[test]
    fn a_green_test_does_not_survive_a_return_to_working() {
        let t = team();
        let task = t.working("x");
        t.mv(&task, Testing).unwrap();
        t.test(&task, TestStatus::Pass).unwrap();
        // The user sends it back and on to Review, skipping the test.
        let back = t.user_move(&task, Working);
        assert_eq!(back.test_result, None);
        t.user_move(&task, Review);
        t.review(&task, ReviewDecision::Approved, vec![]).unwrap();
        assert!(conflict(t.mv(&task, Done)).contains("green test"));
        // An approved review does not survive either.
        let back = t.user_move(&task, Working);
        assert_eq!((back.test_result, back.review_result), (None, None));
    }

    #[test]
    fn a_green_test_does_not_survive_a_move_to_backlog() {
        let t = team();
        let task = t.working("feature");
        t.mv(&task, Testing).unwrap();
        t.test(&task, TestStatus::Pass).unwrap();
        let task = t.mv(&task, Review).unwrap();
        assert_eq!(task.test_result.as_ref().map(|r| r.status), Some(TestStatus::Pass));
        let task = t.user_move(&task, Backlog);
        // The user sends it straight to Review: the old green test is not this pass's test.
        let review = t.user_move(&task, Review);
        assert_eq!(review.test_result, None);
        t.review(&task, ReviewDecision::Approved, vec![]).unwrap();
        assert!(conflict(t.mv(&task, Done)).contains("green test"));
    }

    #[test]
    fn a_patch_and_a_move_at_the_same_time_keep_both() {
        for _ in 0..10 {
            let t = team();
            let task = t.working("feature");
            let task = t.pass(&task, ReviewDecision::Changes, vec![high()]);
            let barrier = std::sync::Barrier::new(2);
            let (moved, patched) = std::thread::scope(|s| {
                let mv = s.spawn(|| {
                    barrier.wait();
                    t.mv(&task, Working)
                });
                let patch = s.spawn(|| {
                    barrier.wait();
                    let mut req = PatchTaskRequest::new(t.coordinator());
                    req.details = Some("plan".into());
                    req.run_id = Some("run".into());
                    t.hub.patch_task(&task.id, None, &req)
                });
                (mv.join().unwrap(), patch.join().unwrap())
            });
            let moved = moved.unwrap();
            patched.unwrap();
            let task = t.get(&task);
            assert_eq!((task.status, task.review_rounds, task.round_limit), (Working, 1, 2));
            assert_eq!(task.test_result, None);
            assert_eq!(task.review_result, moved.review_result);
            assert_eq!(task.review_result.as_ref().map(|r| r.decision), Some(ReviewDecision::Changes));
            assert_eq!((task.details.as_deref(), task.run_id.as_deref()), (Some("plan"), Some("run")));
        }
    }

    #[test]
    fn review_findings_need_critical_flag_and_category() {
        let t = team();
        let task = t.working("x");
        t.mv(&task, Testing).unwrap();
        t.test(&task, TestStatus::Pass).unwrap();
        t.mv(&task, Review).unwrap();
        bad_request(t.review(&task, ReviewDecision::Changes, vec![finding(None, None)]));
        bad_request(t.review(&task, ReviewDecision::Changes, vec![finding(Some(true), None)]));
        bad_request(t.review(&task, ReviewDecision::Approved, vec![critical()]));
        bad_request(t.review(&task, ReviewDecision::Approved, vec![high()]));
        bad_request(t.review(&task, ReviewDecision::Changes, vec![]));
        // Findings that are neither critical nor high never start a fix round (E4).
        bad_request(t.review(&task, ReviewDecision::Changes, vec![minor()]));
        let task = t.review(&task, ReviewDecision::Changes, vec![critical(), high(), minor()]).unwrap();
        let review = task.review_result.unwrap();
        assert_eq!(review.round, 1);
        assert_eq!(review.findings[0].category, Some(FindingCategory::Security));
        assert!(review.has_critical());
        // Each finding is stored with one of the three severities.
        let levels: Vec<&str> = review.findings.iter().map(|f| f.severity.as_deref().unwrap()).collect();
        assert_eq!(levels, ["critical", "high", "other"]);
    }

    #[test]
    fn review_result_worker_must_be_a_reviewer_of_this_coordinator() {
        let t = team();
        let task = t.working("x");
        t.mv(&task, Testing).unwrap();
        // The reviewer is not a coder.
        bad_request(t.hub.record_results(&task.id, None, &ResultRequest {
            actor: t.coordinator(),
            test: Some(TestResultInput {
                status: TestStatus::Pass,
                worker: t.reviewer.id.clone(),
                summary: String::new(),
                failures: vec![],
            }),
            review: None,
        }));
        t.test(&task, TestStatus::Pass).unwrap();
        t.mv(&task, Review).unwrap();
        let other = session(&t.hub, "coord2", jgt_core::COORDINATOR, "/tmp/jgt-board-test/other", None);
        let stranger = session(&t.hub, "r2", "reviewer", WS, Some(&other.id));
        for worker in [&t.coder, &stranger] {
            bad_request(t.hub.record_results(&task.id, None, &ResultRequest {
                actor: t.coordinator(),
                test: None,
                review: Some(ReviewResultInput {
                    decision: ReviewDecision::Approved,
                    worker: worker.id.clone(),
                    findings: vec![],
                }),
            }));
        }
        t.review(&task, ReviewDecision::Approved, vec![]).unwrap();
    }

    #[test]
    fn worker_id_exact_and_prefix_lookups() {
        let hub = hub();
        let coord = session_with_id(&hub, "01ARZ3NDEK0000000000000001", "coord", jgt_core::COORDINATOR, WS, None);
        let coder = session_with_id(&hub, "01BRZ3NDEK0000000000000002", "coder", "coder", WS, Some(&coord.id));
        let reviewer = session_with_id(&hub, "01CRZ3NDEK0000000000000003", "reviewer", "reviewer", WS, Some(&coord.id));
        let t = Team { hub, coord, coder, reviewer };
        let task = t.working("task1");
        t.mv(&task, Testing).unwrap();

        // 1. Exact id works for test result (and stores full id)
        let res = t.hub.record_results(&task.id, None, &ResultRequest {
            actor: t.coordinator(),
            test: Some(TestResultInput {
                status: TestStatus::Pass,
                worker: t.coder.id.clone(),
                summary: "tests pass".into(),
                failures: vec![],
            }),
            review: None,
        }).unwrap();
        assert_eq!(res.test_result.as_ref().unwrap().worker, t.coder.id);

        // Move to Review
        t.mv(&task, Review).unwrap();

        // 1. Exact id works for review result (and stores full id)
        let res = t.hub.record_results(&task.id, None, &ResultRequest {
            actor: t.coordinator(),
            test: None,
            review: Some(ReviewResultInput {
                decision: ReviewDecision::Approved,
                worker: t.reviewer.id.clone(),
                findings: vec![],
            }),
        }).unwrap();
        assert_eq!(res.review_result.as_ref().unwrap().worker, t.reviewer.id);
        t.mv(&task, Done).unwrap();

        // 2. 10-char prefix works for test result, and stores full id
        let task2 = t.working("task2");
        t.mv(&task2, Testing).unwrap();
        let coder_prefix = &t.coder.id[..10];
        let res = t.hub.record_results(&task2.id, None, &ResultRequest {
            actor: t.coordinator(),
            test: Some(TestResultInput {
                status: TestStatus::Pass,
                worker: coder_prefix.to_string(),
                summary: "tests pass prefix".into(),
                failures: vec![],
            }),
            review: None,
        }).unwrap();
        assert_eq!(res.test_result.as_ref().unwrap().worker, t.coder.id);

        // Verify task events also store the resolved full id
        let events = t.events(&task2);
        let test_event = events.iter().find(|e| e.kind == TaskEventKind::TestResult).unwrap();
        assert_eq!(test_event.meta["worker"], t.coder.id);

        // 2. 10-char prefix works for review result, and stores full id
        t.mv(&task2, Review).unwrap();
        let reviewer_prefix = &t.reviewer.id[..10];
        let res = t.hub.record_results(&task2.id, None, &ResultRequest {
            actor: t.coordinator(),
            test: None,
            review: Some(ReviewResultInput {
                decision: ReviewDecision::Approved,
                worker: reviewer_prefix.to_string(),
                findings: vec![],
            }),
        }).unwrap();
        assert_eq!(res.review_result.as_ref().unwrap().worker, t.reviewer.id);

        let events = t.events(&task2);
        let review_event = events.iter().find(|e| e.kind == TaskEventKind::ReviewResult).unwrap();
        assert_eq!(review_event.meta["worker"], t.reviewer.id);
        t.mv(&task2, Done).unwrap();

        // 3. Ambiguous prefix rejected (two sessions sharing a prefix)
        let shared_prefix = &t.reviewer.id[..10];
        let fake_reviewer_id = format!("{shared_prefix}9999999999999999");
        session_with_id(&t.hub, &fake_reviewer_id, "rev2", "reviewer", WS, Some(&t.coord.id));

        let task3 = t.working("task3");
        t.mv(&task3, Testing).unwrap();
        t.test(&task3, TestStatus::Pass).unwrap();
        t.mv(&task3, Review).unwrap();

        let err = t.hub.record_results(&task3.id, None, &ResultRequest {
            actor: t.coordinator(),
            test: None,
            review: Some(ReviewResultInput {
                decision: ReviewDecision::Approved,
                worker: shared_prefix.to_string(),
                findings: vec![],
            }),
        }).unwrap_err();
        match err {
            HubError::BadRequest(msg) => {
                assert!(msg.contains("2 sessions match"), "unexpected message: {msg}");
                assert!(msg.contains(shared_prefix), "unexpected message: {msg}");
                assert!(msg.contains(&fake_reviewer_id), "unexpected message: {msg}");
                assert!(msg.contains(&t.reviewer.id), "unexpected message: {msg}");
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }

        // 4. Unknown id gives the hint
        let err = t.hub.record_results(&task3.id, None, &ResultRequest {
            actor: t.coordinator(),
            test: None,
            review: Some(ReviewResultInput {
                decision: ReviewDecision::Approved,
                worker: "01UNKNOWN000000000".to_string(),
                findings: vec![],
            }),
        }).unwrap_err();
        match err {
            HubError::BadRequest(msg) => {
                assert!(
                    msg.contains("use the full session id (26 chars, `jg-team-dev sessions`)"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }

        // Short unknown id (< 8 chars) also gives the hint
        let err_short = t.hub.record_results(&task3.id, None, &ResultRequest {
            actor: t.coordinator(),
            test: None,
            review: Some(ReviewResultInput {
                decision: ReviewDecision::Approved,
                worker: "short".to_string(),
                findings: vec![],
            }),
        }).unwrap_err();
        match err_short {
            HubError::BadRequest(msg) => {
                assert!(
                    msg.contains("use the full session id (26 chars, `jg-team-dev sessions`)"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }

        // 5. A prefix of a session started by ANOTHER Coordinator is still rejected
        let other_coord = session_with_id(&t.hub, "01DRZ3NDEK0000000000000004", "coord_other", jgt_core::COORDINATOR, "/tmp/jgt-board-test/other_ws", None);
        let stranger = session_with_id(&t.hub, "01ERZ3NDEK0000000000000005", "stranger_rev", "reviewer", WS, Some(&other_coord.id));
        let stranger_prefix = &stranger.id[..10];

        let err_stranger = t.hub.record_results(&task3.id, None, &ResultRequest {
            actor: t.coordinator(),
            test: None,
            review: Some(ReviewResultInput {
                decision: ReviewDecision::Approved,
                worker: stranger_prefix.to_string(),
                findings: vec![],
            }),
        }).unwrap_err();
        match err_stranger {
            HubError::BadRequest(msg) => {
                assert!(
                    msg.contains("was not started by this Coordinator"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_first_verification_goes_straight_to_done() {
        let t = team();
        let task = t.working("feature");
        let task = t.pass(&task, ReviewDecision::Approved, vec![minor(), minor()]);
        let done = t.mv(&task, Done).unwrap();
        assert_eq!(done.status, Done);
        let follow = t.hub.tasks(WS, false).unwrap().into_iter().find(|x| x.id != task.id).unwrap();
        assert_eq!(follow.status, Backlog);
        assert_eq!(follow.parent_id.as_deref(), Some(task.id.as_str()));
        assert_eq!(follow.title, "Follow-up: feature");
        assert!(follow.description.contains("a problem — fix it"));
        assert!(t.events(&task).iter().any(|e| e.kind == TaskEventKind::FollowUp && e.body.contains(&follow.key)));
        assert!(t.events(&follow).iter().any(|e| e.kind == TaskEventKind::FollowUp && e.actor == Actor::Hub));
    }

    #[test]
    fn round_2_review_with_critical_finding_moves_to_done_with_high_follow_up() {
        let t = team();
        let task = t.working("feature");
        let first = t.pass(&task, ReviewDecision::Changes, vec![critical(), minor()]);
        assert!(first.fix_round_left());
        t.mv(&task, Working).unwrap();
        let second = t.pass(&task, ReviewDecision::Changes, vec![critical()]);
        assert_eq!(second.review_result.as_ref().map(|r| r.round), Some(2));
        assert!(!second.fix_round_left());
        // No further automatic fix round.
        assert!(conflict(t.mv(&task, Working)).contains("no further automatic fix"));
        // Accepted for Done in round 2 even with changes requested.
        let done = t.mv(&task, Done).unwrap();
        assert_eq!(done.status, Done);
        assert!(done.done_at.is_some());
        let follow = t.hub.tasks(WS, false).unwrap().into_iter().find(|x| x.id != task.id).unwrap();
        assert_eq!(follow.status, Backlog);
        assert_eq!(follow.priority, TaskPriority::High);
        assert_eq!(follow.parent_id.as_deref(), Some(task.id.as_str()));
        assert!(follow.description.contains("a problem"));
        assert!(t.events(&task).iter().any(|e| e.kind == TaskEventKind::FollowUp && e.body.contains(&follow.key)));
    }

    #[test]
    fn follow_up_priority_high_with_one_high_three_medium_one_low() {
        let t = team();
        let task = t.working("feature");
        // Round 1: high finding requests changes
        t.pass(&task, ReviewDecision::Changes, vec![high()]);
        t.mv(&task, Working).unwrap();
        // Round 2: 1 high + 3 medium + 1 low findings
        let findings = vec![
            high(),
            medium_finding(),
            medium_finding(),
            medium_finding(),
            low_finding(),
        ];
        t.pass(&task, ReviewDecision::Changes, findings);
        let done = t.mv(&task, Done).unwrap();
        assert_eq!(done.status, Done);
        let follow = t.hub.tasks(WS, false).unwrap().into_iter().find(|x| x.id != task.id).unwrap();
        assert_eq!(follow.priority, TaskPriority::High);
        assert_eq!(follow.status, Backlog);
        assert_eq!(follow.parent_id.as_deref(), Some(task.id.as_str()));
    }

    #[test]
    fn follow_up_priority_normal_with_one_medium_ten_low() {
        let t = team();
        let task = t.working("feature");
        let mut findings = vec![medium_finding()];
        for _ in 0..10 {
            findings.push(low_finding());
        }
        t.pass(&task, ReviewDecision::Approved, findings);
        let done = t.mv(&task, Done).unwrap();
        assert_eq!(done.status, Done);
        let follow = t.hub.tasks(WS, false).unwrap().into_iter().find(|x| x.id != task.id).unwrap();
        assert_eq!(follow.priority, TaskPriority::Normal);
        assert_eq!(follow.status, Backlog);
        assert_eq!(follow.parent_id.as_deref(), Some(task.id.as_str()));
    }

    #[test]
    fn follow_up_priority_normal_with_medium_and_low_findings() {
        let t = team();
        let task = t.working("feature");
        let task = t.pass(&task, ReviewDecision::Approved, vec![medium_finding(), low_finding()]);
        let done = t.mv(&task, Done).unwrap();
        assert_eq!(done.status, Done);
        let follow = t.hub.tasks(WS, false).unwrap().into_iter().find(|x| x.id != task.id).unwrap();
        assert_eq!(follow.priority, TaskPriority::Normal);
        assert_eq!(follow.status, Backlog);
        assert_eq!(follow.parent_id.as_deref(), Some(task.id.as_str()));
    }

    #[test]
    fn follow_up_priority_low_with_only_low_findings() {
        let t = team();
        let task = t.working("feature");
        let task = t.pass(&task, ReviewDecision::Approved, vec![low_finding(), low_finding()]);
        let done = t.mv(&task, Done).unwrap();
        assert_eq!(done.status, Done);
        let follow = t.hub.tasks(WS, false).unwrap().into_iter().find(|x| x.id != task.id).unwrap();
        assert_eq!(follow.priority, TaskPriority::Low);
        assert_eq!(follow.status, Backlog);
        assert_eq!(follow.parent_id.as_deref(), Some(task.id.as_str()));
    }

    #[test]
    fn round_1_changes_with_critical_finding_refuses_done() {
        let t = team();
        let task = t.working("feature");
        let first = t.pass(&task, ReviewDecision::Changes, vec![critical()]);
        assert!(first.fix_round_left());
        let err = conflict(t.mv(&task, Done));
        assert!(err.contains("the fix round is still left"), "expected fix round left refusal, got {err}");
    }

    #[test]
    fn approved_at_round_2_moves_to_done() {
        let t = team();
        let task = t.working("feature");
        t.pass(&task, ReviewDecision::Changes, vec![high()]);
        t.mv(&task, Working).unwrap();
        let second = t.pass(&task, ReviewDecision::Approved, vec![]);
        assert_eq!(second.review_result.as_ref().map(|r| r.round), Some(2));
        let done = t.mv(&task, Done).unwrap();
        assert_eq!(done.status, Done);
        assert!(done.done_at.is_some());
    }

    #[test]
    fn done_without_green_test_or_without_review_is_refused() {
        let t = team();
        // Case 1: In Review without a green test.
        let task = t.working("no-test");
        t.user_move(&task, Review);
        t.review(&task, ReviewDecision::Approved, vec![]).unwrap();
        let err = conflict(t.mv(&task, Done));
        assert!(err.contains("green test"), "expected green test error, got {err}");
        t.user_move(&task, Backlog);

        // Case 2: In Review with a green test, but without a review.
        let task2 = t.working("no-review");
        t.mv(&task2, Testing).unwrap();
        t.test(&task2, TestStatus::Pass).unwrap();
        t.mv(&task2, Review).unwrap();
        let err2 = conflict(t.mv(&task2, Done));
        assert!(err2.contains("review of this pass"), "expected review error, got {err2}");
    }

    #[test]
    fn done_notification_mentions_follow_up() {
        let t = team();
        let task = t.working("feature");
        t.pass(&task, ReviewDecision::Changes, vec![high()]);
        t.mv(&task, Working).unwrap();
        // Round 2 changes with critical finding -> follow-up priority High
        t.pass(&task, ReviewDecision::Changes, vec![critical()]);
        let task = t.mv(&task, Done).unwrap();

        let n = notices(&t.hub, &t.coord);
        let note = n.iter().find(|m| m.meta_str("title") == Some("JGT-1 is Done")).expect("done notice");
        assert_eq!(note.meta_str("level"), Some("success"));
        assert!(
            note.body.contains("A high-priority follow-up JGT-2 is in Backlog."),
            "body was: {}",
            note.body
        );

        // Reopening the task and reaching Done again without findings must not mention the old follow-up
        let reopened = t.user_move(&task, OnQueue);
        let task = t.mv(&reopened, Working).unwrap();
        let task = t.pass(&task, ReviewDecision::Approved, vec![]);
        t.mv(&task, Done).unwrap();

        let n = notices(&t.hub, &t.coord);
        let note = n.iter().filter(|m| m.meta_str("title") == Some("JGT-1 is Done")).last().expect("second done notice");
        assert!(!note.body.contains("follow-up"), "reopened done notice should not mention old follow-up: {}", note.body);
    }

    #[test]
    fn verifier_and_tester_are_refused() {
        let t = team();
        let task = t.working("feature");
        let tester = session(&t.hub, "tester", "tester", WS, Some(&t.coord.id));
        let verifier = session(&t.hub, "verifier", "verifier", WS, Some(&t.coord.id));
        t.mv(&task, Testing).unwrap();
        for worker in [&tester, &verifier] {
            bad_request(t.hub.record_results(&task.id, None, &ResultRequest {
                actor: t.coordinator(),
                test: Some(TestResultInput {
                    status: TestStatus::Pass,
                    worker: worker.id.clone(),
                    summary: "build and tests green".into(),
                    failures: vec![],
                }),
                review: None,
            }));
        }
        t.test(&task, TestStatus::Pass).unwrap();
        t.mv(&task, Review).unwrap();
        for worker in [&tester, &verifier] {
            bad_request(t.hub.record_results(&task.id, None, &ResultRequest {
                actor: t.coordinator(),
                test: None,
                review: Some(ReviewResultInput {
                    decision: ReviewDecision::Approved,
                    worker: worker.id.clone(),
                    findings: vec![],
                }),
            }));
        }
    }

    #[test]
    fn user_moves_skip_gates_and_are_logged() {
        let t = team();
        let task = t.create(ActorRequest::User, "x", None).unwrap();
        let done = t.user_move(&task, Done);
        assert_eq!(done.status, Done);
        let moved = t.events(&task).into_iter().rfind(|e| e.kind == TaskEventKind::Moved).unwrap();
        assert_eq!(moved.actor, Actor::User);
        assert_eq!(moved.body, "moved by you");
        assert!(moved.meta["gate_skipped"].as_str().unwrap().contains("Backlog"));
        assert_eq!(moved.meta["from"], "backlog");
        assert_eq!(moved.meta["to"], "done");
        // A move the team could make is logged without a skipped gate.
        let task = t.create(ActorRequest::User, "y", None).unwrap();
        t.user_move(&task, OnQueue);
        let moved = t.events(&task).into_iter().rfind(|e| e.kind == TaskEventKind::Moved).unwrap();
        assert!(moved.meta.get("gate_skipped").is_none());
    }

    #[test]
    fn reopening_resets_rounds() {
        let t = team();
        let task = t.working("feature");
        t.pass(&task, ReviewDecision::Changes, vec![high()]);
        t.mv(&task, Working).unwrap();
        let task = t.pass(&task, ReviewDecision::Approved, vec![minor()]);
        let done = t.mv(&task, Done).unwrap();
        assert_eq!(done.review_rounds, 1);
        let reopened = t.user_move(&done, OnQueue);
        assert_eq!(reopened.status, OnQueue);
        assert_eq!((reopened.review_rounds, reopened.round_limit), (0, 2));
        assert_eq!((reopened.test_result, reopened.review_result, reopened.done_at), (None, None, None));
    }

    #[test]
    fn task_writes_emit_task_events() {
        let t = team();
        let mut rx = t.hub.subscribe();
        let mut drain = || {
            let mut got = vec![];
            while let Ok(e) = rx.try_recv() {
                got.push(e);
            }
            got
        };
        let task = t.create(ActorRequest::User, "x", None).unwrap();
        let got = drain();
        assert!(got.iter().any(|e| matches!(e, Event::Task { task: x } if x.id == task.id)));
        assert!(got.iter().any(|e| matches!(e, Event::TaskEvent { event } if event.kind == TaskEventKind::Created)));

        t.user_move(&task, OnQueue);
        let got = drain();
        assert!(got.iter().any(|e| matches!(e, Event::Task { task: x } if x.status == OnQueue)));
        assert!(got.iter().any(|e| matches!(e, Event::TaskEvent { event } if event.kind == TaskEventKind::Moved)));

        let mut req = PatchTaskRequest::new(t.coordinator());
        req.waiting = Some(Some(WaitingInput { reason: "which database?".into(), question_id: None }));
        let task = t.hub.patch_task(&task.id, None, &req).unwrap();
        assert_eq!(task.waiting.map(|w| w.kind), Some(WaitingKind::Question));
        let got = drain();
        assert!(got.iter().any(|e| matches!(e, Event::TaskEvent { event } if event.kind == TaskEventKind::Waiting)));

        t.hub
            .add_task_event(&task.id, None, &TaskEventRequest {
                actor: ActorRequest::User,
                kind: TaskEventKind::Comment,
                body: "use sqlite".into(),
                meta: serde_json::Value::Null,
            })
            .unwrap();
        assert!(drain().iter().any(|e| matches!(e, Event::TaskEvent { event } if event.body == "use sqlite")));

        t.hub.delete_task(&task.id, None, &ActorRequest::User).unwrap();
        assert!(drain().iter().any(|e| matches!(e, Event::TaskRemoved { id, workspace } if *id == task.id && workspace == WS)));
    }

    // ── K-3: the team on the board ──────────────────────────────────────────

    /// The messages queued for `s` (to the agent).
    fn inbox(hub: &Hub, s: &Session) -> Vec<jgt_core::Message> {
        hub.messages(&s.id, None, 100).unwrap().into_iter().filter(|m| m.direction == Direction::ToAgent).collect()
    }

    /// The notify messages in `s`'s chat.
    fn notices(hub: &Hub, s: &Session) -> Vec<jgt_core::Message> {
        hub.messages(&s.id, None, 100).unwrap().into_iter().filter(|m| m.kind == MessageKind::Notify).collect()
    }

    #[test]
    fn a_task_goes_from_queue_to_done_with_one_change_request() {
        let t = team();
        let coder = session(&t.hub, "coder", "coder", WS, Some(&t.coord.id));
        let task = t.create(ActorRequest::User, "fix navbar", None).unwrap();

        // The user queues it: the Coordinator gets the queue message, the caller learns whom to wake.
        let r = t
            .hub
            .move_task(&task.id, None, &MoveTaskRequest { actor: ActorRequest::User, to: OnQueue, position: None, reason: None })
            .unwrap();
        assert!(r.queued);
        assert_eq!(r.coordinator.as_ref().map(|c| c.id.as_str()), Some(t.coord.id.as_str()));
        let queued = inbox(&t.hub, &t.coord);
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].body, "[jg-team-dev] Task JGT-1 \"fix navbar\" is on the queue. Read it with task_get.");
        assert_eq!(queued[0].meta["task_id"], task.id.as_str());
        assert_eq!(queued[0].delivery, Delivery::Queued);

        // The Coordinator takes it, writes the Details, starts a coder.
        t.mv(&task, Working).unwrap();
        let mut req = PatchTaskRequest::new(t.coordinator());
        req.details = Some("## Plan\nChange the CSS.".into());
        req.run_id = Some("20261027-1200-jgt-1-fix-navbar".into());
        req.worktree = Some("jgt-1".into());
        t.hub.patch_task(&task.id, None, &req).unwrap();
        let worker_event = |kind, actor: ActorRequest| {
            t.hub
                .add_task_event(&task.id, None, &TaskEventRequest {
                    actor,
                    kind,
                    body: String::new(),
                    meta: serde_json::json!({ "worker": "coder-uuid", "skill": "coder" }),
                })
                .unwrap();
        };
        worker_event(TaskEventKind::WorkerStarted, t.coordinator());
        assert_eq!(t.get(&task).current_worker.as_deref(), Some("coder-uuid"));
        worker_event(TaskEventKind::WorkerFinished, as_session(&coder));
        assert_eq!(t.get(&task).current_worker, None);

        // Round 1: a change request with one high finding.
        let after = t.pass(&task, ReviewDecision::Changes, vec![high()]);
        assert_eq!(after.review_result.as_ref().unwrap().round, 1);
        let back = t.mv(&task, Working).unwrap();
        assert_eq!(back.review_rounds, 1);
        // Round 2: approved; the coordinator moves it to Done.
        let approved = t.pass(&task, ReviewDecision::Approved, vec![]);
        assert_eq!(approved.review_result.as_ref().unwrap().round, 2);
        let done = t.mv(&task, Done).unwrap();
        assert!(done.done_at.is_some());
        assert_eq!(done.description, "the user's words", "the description is never touched");

        // Every step is in the log, with the right actors; no follow-up task.
        let coordinator = Actor::Coordinator { session: t.coord.id.clone() };
        let moves: Vec<(String, Actor)> = t
            .events(&task)
            .into_iter()
            .filter(|e| e.kind == TaskEventKind::Moved)
            .map(|e| (e.meta["to"].as_str().unwrap_or_default().to_string(), e.actor))
            .collect();
        let expect = |to: &str, actor: &Actor| (to.to_string(), actor.clone());
        assert_eq!(moves, [
            expect("on_queue", &Actor::User),
            expect("working", &coordinator),
            expect("testing", &coordinator),
            expect("review", &coordinator),
            expect("working", &coordinator),
            expect("testing", &coordinator),
            expect("review", &coordinator),
            expect("done", &coordinator),
        ]);
        let kinds: Vec<TaskEventKind> = t.events(&task).into_iter().map(|e| e.kind).collect();
        for kind in [TaskEventKind::Details, TaskEventKind::Link, TaskEventKind::TestResult, TaskEventKind::ReviewResult] {
            assert!(kinds.contains(&kind), "{kind} logged");
        }
        assert_eq!(t.hub.tasks(WS, true).unwrap().len(), 1, "no follow-up task");
        // Done by the team is announced in the Coordinator's chat.
        let titles: Vec<String> =
            notices(&t.hub, &t.coord).iter().filter_map(|m| m.meta_str("title").map(str::to_owned)).collect();
        assert_eq!(titles, ["JGT-1 is Done"]);
    }

    #[test]
    fn queue_without_a_coordinator_is_logged() {
        let hub = hub();
        let task = hub
            .create_task(&CreateTaskRequest {
                actor: ActorRequest::User,
                workspace: WS.into(),
                title: "alone".into(),
                description: String::new(),
                priority: Some(jgt_core::TaskPriority::High),
                status: None,
                parent_id: None,
            })
            .unwrap();
        let r = hub
            .move_task(&task.id, None, &MoveTaskRequest { actor: ActorRequest::User, to: OnQueue, position: None, reason: None })
            .unwrap();
        assert!(r.queued && r.coordinator.is_none());
        let log = hub.task_events(&task.id, None).unwrap();
        let note = log.iter().find(|e| e.kind == TaskEventKind::Note).expect("a note");
        assert_eq!((note.body.as_str(), &note.actor), (crate::state::board::NO_COORDINATOR, &Actor::Hub));
        // A reorder inside On Queue is not a new request.
        let r = hub
            .move_task(&task.id, None, &MoveTaskRequest { actor: ActorRequest::User, to: OnQueue, position: Some(0), reason: None })
            .unwrap();
        assert!(!r.queued);
    }

    #[test]
    fn retired_coordinator_is_not_notified() {
        let t = team();
        t.hub.end(&t.coord.id, &EndRequest { reason: format!("{}: reset", jgt_core::RETIRED), gone: true }).unwrap();
        let fresh = session(&t.hub, "coord-2", jgt_core::COORDINATOR, &format!("{WS}/"), None);
        let task = t.create(ActorRequest::User, "after the reset", None).unwrap();
        let r = t
            .hub
            .move_task(&task.id, None, &MoveTaskRequest { actor: ActorRequest::User, to: OnQueue, position: None, reason: None })
            .unwrap();
        assert_eq!(r.coordinator.map(|c| c.id), Some(fresh.id.clone()));
        assert_eq!(inbox(&t.hub, &fresh).len(), 1);
        assert!(inbox(&t.hub, &t.coord).is_empty());
        // The priority is in the message when it is not normal.
        let mut req = PatchTaskRequest::new(ActorRequest::User);
        req.priority = Some(jgt_core::TaskPriority::High);
        let other = t.create(ActorRequest::User, "urgent", None).unwrap();
        t.hub.patch_task(&other.id, None, &req).unwrap();
        t.user_move(&other, OnQueue);
        assert!(inbox(&t.hub, &fresh).iter().any(|m| m.body.contains("\"urgent\" is on the queue (priority high)")));
    }

    #[test]
    fn moving_to_backlog_keeps_positions_contiguous() {
        let t = team();
        let [a, b] = ["a", "b"].map(|x| t.create(ActorRequest::User, x, None).unwrap());
        let task = t.working("stopped");
        t.user_move(&task, Backlog);
        let backlog: Vec<(String, i64)> =
            t.hub.tasks(WS, false).unwrap().into_iter().filter(|x| x.status == Backlog).map(|x| (x.title, x.position)).collect();
        assert_eq!(backlog, [("b".to_string(), 0), ("a".to_string(), 1), ("stopped".to_string(), 2)]);
        let _ = (a, b);
    }

    // ── Queue reminder (JGT-24) ─────────────────────────────────────────────

    #[test]
    fn queued_task_and_idle_coordinator_with_zero_in_progress_triggers_reminder() {
        let t = team();
        let task = t.create(ActorRequest::User, "fix navbar", Some(OnQueue)).unwrap();

        t.hub
            .set_state(&t.coord.id, &StateRequest { status: SessionStatus::Idle, last_reply: None })
            .unwrap();

        let queued = inbox(&t.hub, &t.coord);
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].body,
            "[jg-team-dev] 1 task(s) are on the queue: JGT-1. Take the next one (task_list status=on_queue)."
        );
        assert_eq!(queued[0].meta["task_id"], task.id.as_str());
        assert_eq!(queued[0].delivery, Delivery::Queued);
    }

    #[test]
    fn same_stop_again_within_window_gives_no_second_reminder() {
        let t = team();
        let _task = t.create(ActorRequest::User, "fix navbar", Some(OnQueue)).unwrap();

        t.hub
            .set_state(&t.coord.id, &StateRequest { status: SessionStatus::Idle, last_reply: None })
            .unwrap();
        let msgs = inbox(&t.hub, &t.coord);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].delivery, Delivery::Queued);

        // Take messages (simulating hook take)
        let taken = t.hub.take(&t.coord.id).unwrap();
        assert_eq!(taken.len(), 1);

        // Stop again within the window
        t.hub
            .set_state(&t.coord.id, &StateRequest { status: SessionStatus::Idle, last_reply: None })
            .unwrap();
        // Still only the 1 previous delivered message; no second reminder was queued.
        assert_eq!(inbox(&t.hub, &t.coord).len(), 1);
        assert!(t.hub.take(&t.coord.id).unwrap().is_empty());
    }

    #[test]
    fn task_in_working_with_wip_limit_1_gives_no_reminder() {
        let t = team();
        let task1 = t.create(ActorRequest::User, "in progress task", Some(OnQueue)).unwrap();
        t.mv(&task1, Working).unwrap();
        t.hub.take(&t.coord.id).unwrap();

        let _task2 = t.create(ActorRequest::User, "queued task", Some(OnQueue)).unwrap();

        t.hub
            .set_state(&t.coord.id, &StateRequest { status: SessionStatus::Idle, last_reply: None })
            .unwrap();
        assert!(inbox(&t.hub, &t.coord).is_empty());
    }

    #[test]
    fn empty_queue_gives_no_reminder() {
        let t = team();
        t.hub
            .set_state(&t.coord.id, &StateRequest { status: SessionStatus::Idle, last_reply: None })
            .unwrap();
        assert!(inbox(&t.hub, &t.coord).is_empty());
    }

    #[test]
    fn non_coordinator_worker_idle_gives_no_reminder() {
        let t = team();
        let _task = t.create(ActorRequest::User, "task", Some(OnQueue)).unwrap();

        t.hub
            .set_state(&t.coder.id, &StateRequest { status: SessionStatus::Idle, last_reply: None })
            .unwrap();
        assert!(inbox(&t.hub, &t.coder).is_empty());
        assert!(inbox(&t.hub, &t.coord).is_empty());
    }

    #[test]
    fn queue_reminder_caps_at_five_keys() {
        let t = team();
        for i in 1..=6 {
            t.create(ActorRequest::User, &format!("task {i}"), Some(OnQueue)).unwrap();
        }

        t.hub
            .set_state(&t.coord.id, &StateRequest { status: SessionStatus::Idle, last_reply: None })
            .unwrap();

        let queued = inbox(&t.hub, &t.coord);
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].body,
            "[jg-team-dev] 6 task(s) are on the queue: JGT-6, JGT-5, JGT-4, JGT-3, JGT-2, .... Take the next one (task_list status=on_queue)."
        );
    }

    // ── E7: the usage limit ─────────────────────────────────────────────────

    #[test]
    fn a_usage_limit_death_waits_and_the_hub_resumes_the_task() {
        let t = team();
        let coder = session(&t.hub, "coder", "coder", WS, Some(&t.coord.id));
        let task = t.working("feature");
        let finished = |until: i64| TaskEventRequest {
            actor: as_session(&coder),
            kind: TaskEventKind::WorkerFinished,
            body: "coder stopped by the usage limit; resets 17:00".into(),
            meta: serde_json::json!({
                "worker": "coder-uuid", "skill": "coder", "ok": false,
                "usage_limit": { "resets": "17:00", "until": until, "resume_brief": "/p/.team/runs/r/briefs/resume-coder-1.md" },
            }),
        };
        // Resets in an hour: the task waits, the user is told, nothing resumes yet.
        let later = jgt_core::now_ms() + 60 * 60 * 1000;
        t.hub.add_task_event(&task.id, None, &finished(later)).unwrap();
        let waiting = t.get(&task).waiting.unwrap();
        assert_eq!((waiting.kind, waiting.until, waiting.reason.as_str()), (WaitingKind::UsageLimit, Some(later), "usage limit, resets 17:00"));
        assert!(notices(&t.hub, &t.coord).iter().any(|m| m.body.contains("usage limit, resets 17:00")));
        assert!(t.hub.resume_usage_limits().unwrap().is_empty());
        // The team cannot go on, nor clear the flag itself.
        conflict(t.mv(&task, Testing));
        let mut req = PatchTaskRequest::new(t.coordinator());
        req.waiting = Some(None);
        assert!(conflict(t.hub.patch_task(&task.id, None, &req)).contains("usage limit"));

        // Reset 3 minutes ago (more than the 2-minute grace): cleared, and the Coordinator is told.
        let past = jgt_core::now_ms() - 3 * 60 * 1000;
        t.hub.add_task_event(&task.id, None, &finished(past)).unwrap();
        let before = inbox(&t.hub, &t.coord).len();
        let wake = t.hub.resume_usage_limits().unwrap();
        assert_eq!(wake.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [t.coord.id.as_str()]);
        assert_eq!(t.get(&task).waiting, None);
        let got = inbox(&t.hub, &t.coord);
        assert_eq!(got.len(), before + 1);
        let text = &got.last().unwrap().body;
        assert!(text.starts_with("[jg-team-dev] Usage limit reset: resume JGT-1 \"feature\""), "{text}");
        assert!(text.contains("Resume brief: /p/.team/runs/r/briefs/resume-coder-1.md"), "{text}");
        // Once only.
        assert!(t.hub.resume_usage_limits().unwrap().is_empty());
        t.mv(&task, Testing).unwrap();
    }

    #[test]
    fn a_usage_limit_does_not_replace_an_open_question() {
        let t = team();
        let coder = session(&t.hub, "coder", "coder", WS, Some(&t.coord.id));
        let task = t.working("feature");
        let mut req = PatchTaskRequest::new(t.coordinator());
        req.waiting = Some(Some(WaitingInput { reason: "which color?".into(), question_id: None }));
        t.hub.patch_task(&task.id, None, &req).unwrap();
        t.hub
            .add_task_event(&task.id, None, &TaskEventRequest {
                actor: as_session(&coder),
                kind: TaskEventKind::WorkerFinished,
                body: String::new(),
                meta: serde_json::json!({ "usage_limit": { "resets": "17:00", "until": 1 } }),
            })
            .unwrap();
        assert_eq!(t.get(&task).waiting.map(|w| w.kind), Some(WaitingKind::Question));
    }

    // ── K-4: comments, notifications, archive ───────────────────────────────

    #[test]
    fn a_comment_reaches_the_coordinator() {
        let t = team();
        let task = t.create(ActorRequest::User, "navbar", None).unwrap();
        let comment = |actor| TaskEventRequest { actor, kind: TaskEventKind::Comment, body: " make it blue ".into(), meta: serde_json::Value::Null };
        t.hub.add_task_event(&task.id, None, &comment(ActorRequest::User)).unwrap();
        let got = inbox(&t.hub, &t.coord);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "[jg-team-dev] Comment on JGT-1 \"navbar\": make it blue");
        assert_eq!(got[0].meta["task_id"], task.id.as_str());
        // The Coordinator's own comments are not echoed back to it.
        t.hub.add_task_event(&task.id, None, &comment(t.coordinator())).unwrap();
        assert_eq!(inbox(&t.hub, &t.coord).len(), 1);
    }

    #[test]
    fn done_and_waiting_post_a_notify_message() {
        let t = team();
        // Waiting set by the Coordinator (an open question).
        let task = t.working("question");
        let mut req = PatchTaskRequest::new(t.coordinator());
        req.waiting = Some(Some(WaitingInput { reason: "which color?".into(), question_id: None }));
        t.hub.patch_task(&task.id, None, &req).unwrap();
        let n = notices(&t.hub, &t.coord);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].meta_str("title"), Some("JGT-1 is waiting for you"));
        assert_eq!(n[0].meta_str("level"), Some("warning"));
        assert_eq!(n[0].meta["task_id"], task.id.as_str());
        assert!(n[0].body.contains("which color?"));
        // Clearing it is not news.
        let mut req = PatchTaskRequest::new(t.coordinator());
        req.waiting = Some(None);
        t.hub.patch_task(&task.id, None, &req).unwrap();
        assert_eq!(notices(&t.hub, &t.coord).len(), 1);
        t.user_move(&task, Backlog);

        // Done by the team is announced; the user's own move to Done is not.
        let done = t.working("done");
        t.pass(&done, ReviewDecision::Approved, vec![]);
        t.mv(&done, Done).unwrap();
        assert!(notices(&t.hub, &t.coord).iter().any(|m| m.meta_str("title") == Some("JGT-2 is Done")));
        let mine = t.create(ActorRequest::User, "mine", None).unwrap();
        t.user_move(&mine, Done);
        assert!(!notices(&t.hub, &t.coord).iter().any(|m| m.meta_str("title") == Some("JGT-3 is Done")));
    }

    fn done_task(t: &Team, title: &str, days_ago: i64) -> Task {
        let task = t.create(ActorRequest::User, title, None).unwrap();
        t.user_move(&task, Done);
        let at = jgt_core::now_ms() - days_ago * 24 * 60 * 60 * 1000;
        t.hub.db.board::<_, rusqlite::Error>(|b| b.set_columns(&task.id, &[("done_at", &at as &dyn rusqlite::ToSql)])).unwrap();
        t.get(&task)
    }

    #[test]
    fn done_tasks_are_archived_after_seven_days() {
        let t = team();
        let old = done_task(&t, "old", 8);
        let recent = done_task(&t, "recent", 6);
        let mut rx = t.hub.subscribe();
        assert_eq!(t.hub.archive_done_tasks().unwrap(), 1);
        assert!(t.get(&old).archived);
        assert!(!t.get(&recent).archived);
        assert_eq!(t.get(&recent).position, 0, "Done is numbered again without the archived task");
        assert!(t.events(&old).iter().any(|e| e.kind == TaskEventKind::Archived && e.actor == Actor::Hub));
        let mut events = vec![];
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert!(events.iter().any(|e| matches!(e, Event::Task { task } if task.id == old.id && task.archived)));
        let shown: Vec<String> = t.hub.tasks(WS, false).unwrap().into_iter().map(|x| x.title).collect();
        assert_eq!(shown, ["recent"]);
        assert_eq!(t.hub.tasks(WS, true).unwrap().len(), 2, "archived tasks are kept");
        assert_eq!(t.hub.archive_done_tasks().unwrap(), 0, "once");
    }

    #[test]
    fn moving_an_archived_task_unarchives_it() {
        let t = team();
        let old = done_task(&t, "old", 8);
        t.hub.archive_done_tasks().unwrap();
        let reopened = t.user_move(&old, OnQueue);
        assert!(!reopened.archived);
        assert_eq!(reopened.done_at, None);
        assert_eq!(t.hub.tasks(WS, false).unwrap().len(), 1);
    }

    #[test]
    fn delete_workspace_removes_sessions_and_tasks_but_keeps_counters() {
        let hub = hub();
        let folder_a = "/tmp/test/folder-a";
        let folder_b = "/tmp/test/folder-b";

        let coord_a = session(&hub, "coord_a", jgt_core::COORDINATOR, folder_a, None);
        let worker_a = session(&hub, "worker_a", "coder", folder_a, Some(&coord_a.id));
        let archived_a = session(&hub, "archived_a", "reviewer", folder_a, Some(&coord_a.id));
        hub.patch(&archived_a.id, &PatchSessionRequest { archived: Some(true), ..Default::default() }, true).unwrap();

        let coord_b = session(&hub, "coord_b", jgt_core::COORDINATOR, folder_b, None);
        let worker_b = session(&hub, "worker_b", "coder", folder_b, Some(&coord_b.id));

        let task_a1 = hub.create_task(&CreateTaskRequest {
            actor: ActorRequest::User,
            workspace: folder_a.into(),
            title: "task a1".into(),
            description: String::new(),
            priority: None,
            status: None,
            parent_id: None,
        }).unwrap();
        let task_a2 = hub.create_task(&CreateTaskRequest {
            actor: ActorRequest::User,
            workspace: folder_a.into(),
            title: "task a2".into(),
            description: String::new(),
            priority: None,
            status: None,
            parent_id: None,
        }).unwrap();
        assert_eq!((task_a1.key.as_str(), task_a2.key.as_str()), ("JGT-1", "JGT-2"));

        let task_b1 = hub.create_task(&CreateTaskRequest {
            actor: ActorRequest::User,
            workspace: folder_b.into(),
            title: "task b1".into(),
            description: String::new(),
            priority: None,
            status: None,
            parent_id: None,
        }).unwrap();
        assert_eq!(task_b1.key.as_str(), "JGT-1");

        let all_sessions = hub.db.sessions(true).unwrap();
        assert!(all_sessions.iter().any(|s| s.id == coord_a.id));
        assert!(all_sessions.iter().any(|s| s.id == worker_a.id));
        assert!(all_sessions.iter().any(|s| s.id == archived_a.id));
        assert!(all_sessions.iter().any(|s| s.id == coord_b.id));
        assert!(all_sessions.iter().any(|s| s.id == worker_b.id));

        assert_eq!(hub.tasks(folder_a, true).unwrap().len(), 2);
        assert_eq!(hub.tasks(folder_b, true).unwrap().len(), 1);

        hub.delete_workspace(&format!("{folder_a}/")).unwrap();

        let remaining_sessions = hub.db.sessions(true).unwrap();
        assert!(!remaining_sessions.iter().any(|s| s.id == coord_a.id));
        assert!(!remaining_sessions.iter().any(|s| s.id == worker_a.id));
        assert!(!remaining_sessions.iter().any(|s| s.id == archived_a.id));

        assert!(hub.tasks(folder_a, true).unwrap().is_empty());

        assert!(remaining_sessions.iter().any(|s| s.id == coord_b.id));
        assert!(remaining_sessions.iter().any(|s| s.id == worker_b.id));
        assert_eq!(hub.tasks(folder_b, true).unwrap().len(), 1);
        assert_eq!(hub.task(&task_b1.id, Some(folder_b)).unwrap().id, task_b1.id);

        let task_a3 = hub.create_task(&CreateTaskRequest {
            actor: ActorRequest::User,
            workspace: folder_a.into(),
            title: "task a3".into(),
            description: String::new(),
            priority: None,
            status: None,
            parent_id: None,
        }).unwrap();
        assert_eq!(task_a3.key, "JGT-3");
    }

    // ── M4: git column routing, refusals, user moves, wip, follow-up, notify ──

    #[test]
    fn routing_all_switches_off_routes_to_to_commit() {
        let t = team();
        t.set_git_switches(false, false, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let resp = t.mv_full(&reviewed, Done).unwrap();
        assert_eq!(resp.task.status, ToCommit);
        assert_eq!(resp.git_steps, Vec::<String>::new());
    }

    #[test]
    fn routing_auto_commit_on_routes_to_to_merge() {
        let t = team();
        t.set_git_switches(true, false, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let resp = t.mv_full(&reviewed, Done).unwrap();
        assert_eq!(resp.task.status, ToMerge);
        assert_eq!(resp.git_steps, vec!["commit".to_string()]);
    }

    #[test]
    fn routing_commit_and_merge_on_routes_to_to_push() {
        let t = team();
        t.set_git_switches(true, true, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let resp = t.mv_full(&reviewed, Done).unwrap();
        assert_eq!(resp.task.status, ToPush);
        assert_eq!(resp.git_steps, vec!["commit".to_string(), "merge".to_string()]);
    }

    #[test]
    fn routing_all_switches_on_routes_to_done() {
        let t = team();
        t.set_git_switches(true, true, true);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let resp = t.mv_full(&reviewed, Done).unwrap();
        assert_eq!(resp.task.status, Done);
        assert_eq!(resp.git_steps, vec!["commit".to_string(), "merge".to_string(), "push".to_string()]);
    }

    #[test]
    fn routing_commit_off_merge_on_routes_to_commit_then_with_reason_to_push() {
        let t = team();
        t.set_git_switches(false, true, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let resp1 = t.mv_full(&reviewed, Done).unwrap();
        assert_eq!(resp1.task.status, ToCommit);
        assert_eq!(resp1.git_steps, Vec::<String>::new());

        let resp2 = t.mv_with_reason(&resp1.task, Done, "user says OK to commit").unwrap();
        assert_eq!(resp2.task.status, ToPush);
        assert_eq!(resp2.git_steps, vec!["commit".to_string(), "merge".to_string()]);
    }

    #[test]
    fn team_refused_git_column_to_working_testing_review() {
        let t = team();
        t.set_git_switches(false, false, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let in_git = t.mv(&reviewed, Done).unwrap();
        assert_eq!(in_git.status, ToCommit);

        for dest in [Backlog, OnQueue, Working, Testing, Review] {
            let err = conflict(t.mv(&in_git, dest));
            assert!(err.contains(&format!("is past review; it does not go back to {}", dest.label())), "was: {err}");
        }
    }

    #[test]
    fn team_refused_jumping_over_column_whose_switch_is_off() {
        let t = team();
        // commit is ON, merge is OFF, push is ON
        t.set_git_switches(true, false, true);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let in_git = t.mv(&reviewed, Done).unwrap();
        assert_eq!(in_git.status, ToMerge);

        // Move task into ToCommit as user to test jumping over ToMerge
        let in_commit = t.user_move(&in_git, ToCommit);
        assert_eq!(in_commit.status, ToCommit);

        // Try team move to ToPush jumping over ToMerge whose switch is OFF
        let err = conflict(t.mv_with_reason(&in_commit, ToPush, "user ok"));
        assert!(err.contains("waits in To merge for the user's OK to merge"), "was: {err}");
    }

    #[test]
    fn team_refused_leaving_column_whose_switch_is_off_without_reason() {
        let t = team();
        t.set_git_switches(false, false, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let in_commit = t.mv(&reviewed, Done).unwrap();
        assert_eq!(in_commit.status, ToCommit);

        let err = conflict(t.mv(&in_commit, ToMerge));
        assert!(err.contains("leaving To commit needs the user's OK: quote it in reason"), "was: {err}");

        let err = conflict(t.mv_with_reason(&in_commit, ToMerge, "   "));
        assert!(err.contains("leaving To commit needs the user's OK: quote it in reason"), "was: {err}");
    }

    #[test]
    fn team_refused_review_to_git_column_without_green_test_or_review() {
        let t = team();
        t.set_git_switches(false, false, false);
        let task = t.working("feature");
        // User moves directly to Review without test
        let in_review = t.user_move(&task, Review);
        let err = conflict(t.mv(&in_review, ToCommit));
        assert!(err.contains("To commit needs a green test of this pass (K6)"), "was: {err}");

        // Now add a green test, but no review
        t.user_move(&in_review, Testing);
        t.test(&in_review, TestStatus::Pass).unwrap();
        t.mv(&in_review, Review).unwrap();
        let err = conflict(t.mv(&in_review, ToCommit));
        assert!(err.contains("To commit needs a review of this pass (K6)"), "was: {err}");
    }

    #[test]
    fn team_move_review_to_git_column_refuses_when_switches_between_are_off() {
        let t = team();
        // 1. commit OFF: Review -> ToMerge refused (409)
        t.set_git_switches(false, true, true);
        let task = t.working("feature1");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let err = conflict(t.mv(&reviewed, ToMerge));
        assert!(err.contains("waits in To commit for the user's OK to commit"), "was: {err}");

        // 2. commit ON: Review -> ToMerge accepted with git_steps [commit]
        t.set_git_switches(true, false, false);
        let resp = t.mv_full(&reviewed, ToMerge).unwrap();
        assert_eq!(resp.task.status, ToMerge);
        assert_eq!(resp.git_steps, vec!["commit".to_string()]);

        // 3. merge OFF: Review -> ToPush refused
        // task is now in ToMerge (not in progress), so we can create feature2
        t.set_git_switches(true, false, true);
        let task2 = t.working("feature2");
        let reviewed2 = t.pass(&task2, ReviewDecision::Approved, vec![]);
        let err2 = conflict(t.mv(&reviewed2, ToPush));
        assert!(err2.contains("waits in To merge for the user's OK to merge"), "was: {err2}");
    }

    #[test]
    fn user_drag_review_to_git_column_or_done_notifies_coordinator() {
        let t = team();
        t.set_git_switches(false, false, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);

        // Drag Review -> ToMerge tells Coordinator with git_steps [commit]
        let resp = t.user_mv_full(&reviewed, ToMerge).unwrap();
        assert_eq!(resp.task.status, ToMerge);
        assert_eq!(resp.git_steps, vec!["commit".to_string()]);
        assert!(resp.coordinator.is_some());

        let msgs = inbox(&t.hub, &t.coord);
        let coord_msg = msgs.iter().find(|m| m.body.contains("their OK to commit")).expect("coordinator message");
        let expected = format!(
            "[jg-team-dev] The user moved {} from Review to To merge: their OK to commit. Run them with the git-coder now.",
            resp.task.key
        );
        assert_eq!(coord_msg.body, expected);

        // Drag Review -> Done tells Coordinator with git_steps [commit, merge, push]
        let task2 = t.working("feature2");
        let reviewed2 = t.pass(&task2, ReviewDecision::Approved, vec![]);
        let resp2 = t.user_mv_full(&reviewed2, Done).unwrap();
        assert_eq!(resp2.task.status, Done);
        assert_eq!(resp2.git_steps, vec!["commit".to_string(), "merge".to_string(), "push".to_string()]);
        assert!(resp2.coordinator.is_some());

        let msgs2 = inbox(&t.hub, &t.coord);
        let coord_msg2 = msgs2.iter().find(|m| m.body.contains("their OK to commit, merge, push")).expect("coordinator message");
        let expected2 = format!(
            "[jg-team-dev] The user moved {} from Review to Done: their OK to commit, merge, push. Run them with the git-coder now.",
            resp2.task.key
        );
        assert_eq!(coord_msg2.body, expected2);
    }

    #[test]
    fn routed_body_appends_user_reason_when_leaving_off_column() {
        let t = team();
        // commit OFF, merge OFF, push ON
        t.set_git_switches(false, false, true);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let in_commit = t.mv(&reviewed, Done).unwrap();
        assert_eq!(in_commit.status, ToCommit);

        // Card leaves ToCommit (auto_commit is OFF) with reason; routes to ToMerge (auto_merge is OFF)
        let resp = t.mv_with_reason(&in_commit, Done, "user agreed in chat").unwrap();
        assert_eq!(resp.task.status, ToMerge);

        let events = t.events(&resp.task);
        let move_event = events.iter().rfind(|e| e.kind == TaskEventKind::Moved).unwrap();
        assert_eq!(move_event.body, "routed to To merge: auto-merge OFF; user agreed in chat");
    }

    #[test]
    fn user_drag_to_commit_to_done_no_gate_skipped_and_notifies_coordinator() {
        let t = team();
        t.set_git_switches(false, false, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let in_commit = t.mv(&reviewed, Done).unwrap();
        assert_eq!(in_commit.status, ToCommit);

        let resp = t.user_mv_full(&in_commit, Done).unwrap();
        assert_eq!(resp.task.status, Done);
        assert_eq!(resp.git_steps, vec!["commit".to_string(), "merge".to_string(), "push".to_string()]);
        assert!(resp.coordinator.is_some());

        let events = t.events(&resp.task);
        let move_event = events.iter().rfind(|e| e.kind == TaskEventKind::Moved).unwrap();
        assert!(move_event.meta.get("gate_skipped").is_none() || move_event.meta["gate_skipped"].is_null());

        let msgs = inbox(&t.hub, &t.coord);
        let coord_msg = msgs.iter().find(|m| m.body.contains("their OK to commit, merge, push")).expect("coordinator message");
        let expected = format!(
            "[jg-team-dev] The user moved {} from To commit to Done: their OK to commit, merge, push. Run them with the git-coder now.",
            resp.task.key
        );
        assert_eq!(coord_msg.body, expected);
    }

    #[test]
    fn user_drag_git_column_to_working_is_allowed_and_logged_with_gate_skipped() {
        let t = team();
        t.set_git_switches(false, false, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let in_commit = t.mv(&reviewed, Done).unwrap();
        assert_eq!(in_commit.status, ToCommit);

        let moved = t.user_move(&in_commit, Working);
        assert_eq!(moved.status, Working);

        let events = t.events(&moved);
        let move_event = events.iter().rfind(|e| e.kind == TaskEventKind::Moved).unwrap();
        let skipped = move_event.meta["gate_skipped"].as_str().expect("gate_skipped logged");
        assert!(skipped.contains("is past review; it does not go back to Working"));
    }

    #[test]
    fn user_drag_to_commit_to_on_queue_resets_rounds() {
        let t = team();
        t.set_git_switches(false, false, false);
        let task = t.working("feature");
        t.pass(&task, ReviewDecision::Changes, vec![high()]);
        let task = t.mv(&task, Working).unwrap();
        assert_eq!(task.review_rounds, 1);
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let in_commit = t.mv(&reviewed, Done).unwrap();
        assert_eq!(in_commit.status, ToCommit);
        assert_eq!(in_commit.review_rounds, 1);

        let reopened = t.user_move(&in_commit, OnQueue);
        assert_eq!(reopened.status, OnQueue);
        assert_eq!(reopened.review_rounds, 0);
        assert_eq!(reopened.round_limit, ROUND_LIMIT);
        assert!(reopened.test_result.is_none());
        assert!(reopened.review_result.is_none());
    }

    #[test]
    fn card_in_to_commit_does_not_block_working_at_wip_limit_1() {
        let t = team();
        t.set_git_switches(false, false, false);
        let t1 = t.working("t1");
        let reviewed = t.pass(&t1, ReviewDecision::Approved, vec![]);
        let in_commit = t.mv(&reviewed, Done).unwrap();
        assert_eq!(in_commit.status, ToCommit);

        let t2 = t.queued("t2");
        let working = t.mv(&t2, Working).unwrap();
        assert_eq!(working.status, Working);
    }

    #[test]
    fn follow_up_filed_once_on_review_to_to_commit_not_again_on_reaching_done() {
        let t = team();
        t.set_git_switches(false, false, false);
        let task = t.working("feature");
        t.pass(&task, ReviewDecision::Changes, vec![critical()]);
        t.mv(&task, Working).unwrap();
        t.pass(&task, ReviewDecision::Changes, vec![critical()]);
        let in_commit = t.mv(&task, Done).unwrap();
        assert_eq!(in_commit.status, ToCommit);

        let tasks_before = t.hub.tasks(WS, false).unwrap();
        let follow_ups: Vec<_> = tasks_before.iter().filter(|x| x.parent_id.as_deref() == Some(&in_commit.id)).collect();
        assert_eq!(follow_ups.len(), 1, "one follow-up created on entering To commit");

        t.set_git_switches(true, true, true);
        let in_done = t.mv(&in_commit, Done).unwrap();
        assert_eq!(in_done.status, Done);

        let tasks_after = t.hub.tasks(WS, false).unwrap();
        let follow_ups_after: Vec<_> = tasks_after.iter().filter(|x| x.parent_id.as_deref() == Some(&in_commit.id)).collect();
        assert_eq!(follow_ups_after.len(), 1, "no second follow-up created on reaching Done");
    }

    #[test]
    fn warning_notify_for_card_entering_to_merge() {
        let t = team();
        t.set_git_switches(true, false, false);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);

        let initial_count = notices(&t.hub, &t.coord).len();
        let in_merge = t.mv(&reviewed, Done).unwrap();
        assert_eq!(in_merge.status, ToMerge);

        let n = notices(&t.hub, &t.coord);
        let new_notices: Vec<_> = n.into_iter().skip(initial_count).collect();
        assert_eq!(new_notices.len(), 1);
        let note = &new_notices[0];
        assert_eq!(note.meta_str("level"), Some("warning"));
        let expected_title = format!("{} waits for your OK to merge", in_merge.key);
        assert_eq!(note.meta_str("title"), Some(expected_title.as_str()));
        assert_eq!(
            note.body,
            format!("\"{}\" is tested and reviewed. Move the card (or answer in chat) to OK the merge.", in_merge.title)
        );
    }

    #[test]
    fn done_notification_lists_steps_per_git_policy() {
        let t = team();
        t.set_git_switches(true, true, true);
        let task = t.working("feature");
        let reviewed = t.pass(&task, ReviewDecision::Approved, vec![]);
        let initial_count = notices(&t.hub, &t.coord).len();
        let done = t.mv(&reviewed, Done).unwrap();
        assert_eq!(done.status, Done);

        let n = notices(&t.hub, &t.coord);
        let new_notices: Vec<_> = n.into_iter().skip(initial_count).collect();
        let done_title = format!("{} is Done", done.key);
        let done_notice = new_notices.iter().find(|m| m.meta_str("title") == Some(done_title.as_str())).expect("done notice");
        assert_eq!(done_notice.meta_str("level"), Some("success"));
        assert!(done_notice.body.contains("Done: tested, reviewed; commit, merge, push per git policy."));
    }
}
