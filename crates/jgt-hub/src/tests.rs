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
fn round_limit_tasks_move_to_orchestrator_review_on_upgrade() {
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
    assert_eq!((stuck.status, stuck.waiting), (jgt_core::TaskStatus::OrchestratorReview, None));
    let asking = hub.task("t2", None).unwrap();
    assert_eq!(asking.waiting.map(|w| w.kind), Some(jgt_core::WaitingKind::Question));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── board ───────────────────────────────────────────────────────────────────

mod board {
    use super::*;
    use jgt_core::{
        Actor, Decision, Event, FindingCategory, ReviewDecision, Session, Task, TaskEvent, TaskEventKind, TaskStatus,
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

    fn as_session(s: &Session) -> ActorRequest {
        ActorRequest::Session { session: s.id.clone() }
    }

    /// A hub with a Coordinator for `WS`, its tester, its reviewer and its verifier.
    struct Team {
        hub: SharedHub,
        coord: Session,
        tester: Session,
        reviewer: Session,
        verifier: Session,
    }

    fn team() -> Team {
        let hub = hub();
        let coord = session(&hub, "coord", jgt_core::COORDINATOR, WS, None);
        let tester = session(&hub, "tester", "tester", WS, Some(&coord.id));
        let reviewer = session(&hub, "reviewer", "reviewer", WS, Some(&coord.id));
        let verifier = session(&hub, "verifier", "verifier", WS, Some(&coord.id));
        Team { hub, coord, tester, reviewer, verifier }
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

        fn user_move(&self, t: &Task, to: TaskStatus) -> Task {
            self.move_as(ActorRequest::User, t, to).unwrap()
        }

        fn test(&self, t: &Task, status: TestStatus) -> Result<Task, HubError> {
            self.hub.record_results(&t.id, None, &ResultRequest {
                actor: self.coordinator(),
                test: Some(TestResultInput {
                    status,
                    worker: self.tester.id.clone(),
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

        /// The flow up to the user (PRD D3): a first verification with a high finding, the fix
        /// round, a second verification with `last` (changes when it holds a critical or high
        /// finding, else approved), then Orchestrator review.
        fn to_orchestrator_review(&self, t: &Task, last: Vec<FindingInput>) -> Task {
            self.pass(t, ReviewDecision::Changes, vec![high()]);
            self.mv(t, Working).unwrap();
            let blocking = last.iter().any(|f| f.critical == Some(true) || f.severity.as_deref() == Some("high"));
            let decision = if blocking { ReviewDecision::Changes } else { ReviewDecision::Approved };
            self.pass(t, decision, last);
            self.mv(t, OrchestratorReview).unwrap()
        }

        fn decide(&self, t: &Task, choice: Decision) -> Result<Task, HubError> {
            self.hub.decide(&t.id, None, &DecisionRequest { actor: ActorRequest::User, choice, question_id: None })
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
            ("a".to_string(), Backlog, 0),
            ("c".to_string(), Backlog, 1),
            ("b".to_string(), OnQueue, 0),
            ("d".to_string(), OnQueue, 1),
        ]);
        let _ = (a, b, c, d);
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
        assert_eq!(column(Backlog), expect(&["a", "c", "d", "e"]));
        // An edit of c keeps its place, as the app shows it.
        let mut req = PatchTaskRequest::new(ActorRequest::User);
        req.priority = Some(jgt_core::TaskPriority::High);
        t.hub.patch_task(&c.id, None, &req).unwrap();
        assert_eq!(column(Backlog), expect(&["a", "c", "d", "e"]));
        // A delete.
        t.hub.delete_task(&a.id, None, &ActorRequest::User).unwrap();
        assert_eq!(column(Backlog), expect(&["c", "d", "e"]));
        // Archived tasks take no position: new tasks and moves number the others from 0.
        t.hub.db.board::<_, rusqlite::Error>(|bd| bd.set_columns(&d.id, &[("archived", &true as &dyn rusqlite::ToSql)])).unwrap();
        t.hub
            .move_task(&e.id, None, &MoveTaskRequest { actor: ActorRequest::User, to: Backlog, position: Some(0), reason: None })
            .unwrap();
        assert_eq!(column(Backlog), expect(&["e", "c"]));
        t.user_move(&e, OnQueue);
        assert_eq!(column(Backlog), expect(&["c"]));
        let f = t.create(ActorRequest::User, "f", None).unwrap();
        assert_eq!(f.position, 1);
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
        let w = as_session(&t.tester);
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
                meta: serde_json::json!({ "worker": "tester" }),
            })
            .unwrap();
        assert_eq!(t.get(&task).current_worker.as_deref(), Some("tester"));
        // Only the user deletes.
        forbidden(t.hub.delete_task(&task.id, None, &t.coordinator()));
    }

    #[test]
    fn only_the_workspace_coordinator_acts_as_coordinator() {
        let t = team();
        let elsewhere = session(&t.hub, "coord2", jgt_core::COORDINATOR, "/tmp/jgt-board-test/other", None);
        forbidden(t.create(as_session(&elsewhere), "x", None));
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
        // The first verification is not the last: no Orchestrator review yet.
        conflict(t.mv(&task, OrchestratorReview));
        let back = t.mv(&task, Working).unwrap();
        assert_eq!(back.review_rounds, 1, "a red test starts the one automatic fix round");
        assert_eq!(back.test_result.map(|r| r.status), Some(TestStatus::Fail), "the coder sees the failures");
        let task = t.mv(&task, Testing).unwrap();
        assert_eq!(task.test_result, None, "a new pass starts without a test");
        // Red again on the second verification: no second fix, the user decides.
        t.test(&task, TestStatus::Fail).unwrap();
        assert!(conflict(t.mv(&task, Working)).contains("D3"));
        let task = t.mv(&task, OrchestratorReview).unwrap();
        assert_eq!((task.status, task.round()), (OrchestratorReview, (2, 2)));
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
        conflict(t.mv(&task, Done));
        conflict(t.mv(&task, OrchestratorReview));
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
        // After a fix round even a clean verification goes to the user (D3/D4).
        assert!(conflict(t.mv(&task, Done)).contains("Orchestrator review"));
        let task = t.mv(&task, OrchestratorReview).unwrap();
        let done = t.decide(&task, Decision::Done).unwrap();
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
    fn a_second_decision_is_a_conflict() {
        let t = team();
        let task = t.working("feature");
        let task = t.to_orchestrator_review(&task, vec![critical()]);
        let more = || DecisionRequest { actor: ActorRequest::User, choice: Decision::MoreRounds, question_id: None };
        assert_eq!(t.hub.decide(&task.id, None, &more()).unwrap().round_limit, 3);
        conflict(t.hub.decide(&task.id, None, &more()));
        assert_eq!(t.get(&task).round_limit, 3);

        // Decisions sent at the same time: one wins, the limit is raised once.
        let t = team();
        let task = t.working("feature");
        let task = t.to_orchestrator_review(&task, vec![critical()]);
        let barrier = std::sync::Barrier::new(8);
        let ok = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        t.hub.decide(&task.id, None, &more())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).filter(|r| r.is_ok()).count()
        });
        assert_eq!(ok, 1);
        assert_eq!(t.get(&task).round_limit, 3);
        let decisions = t.events(&task).iter().filter(|e| e.kind == TaskEventKind::Decision).count();
        assert_eq!(decisions, 1);
    }

    #[test]
    fn a_green_test_does_not_survive_a_return_to_working() {
        let t = team();
        let task = t.working("x");
        t.mv(&task, Testing).unwrap();
        t.test(&task, TestStatus::Pass).unwrap();
        // The user sends it back and on to Review, skipping the tester.
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
    fn a_green_test_does_not_survive_a_stop() {
        for choice in [Decision::Stop, Decision::TakeOver] {
            let t = team();
            let task = t.working("feature");
            let task = t.to_orchestrator_review(&task, vec![critical()]);
            assert_eq!(task.test_result.as_ref().map(|r| r.status), Some(TestStatus::Pass));
            t.decide(&task, choice).unwrap();
            // The user sends it straight to Review: the old green test is not this pass's test.
            let review = t.user_move(&task, Review);
            assert_eq!(review.test_result, None);
            t.review(&task, ReviewDecision::Approved, vec![]).unwrap();
            assert!(conflict(t.mv(&task, Done)).contains("green test"), "{choice}");
        }
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
        // The reviewer is not a tester.
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
        for worker in [&t.tester, &stranger] {
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
    fn a_clean_first_verification_goes_straight_to_done() {
        let t = team();
        let task = t.working("feature");
        let task = t.pass(&task, ReviewDecision::Approved, vec![minor(), minor()]);
        // Not through Orchestrator review: that column is for tasks that needed the fix round.
        assert!(conflict(t.mv(&task, OrchestratorReview)).contains("first verification"));
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
    fn the_second_verification_goes_to_orchestrator_review() {
        let t = team();
        let task = t.working("feature");
        let first = t.pass(&task, ReviewDecision::Changes, vec![critical(), minor()]);
        assert!(first.fix_round_left());
        t.mv(&task, Working).unwrap();
        let second = t.pass(&task, ReviewDecision::Changes, vec![critical()]);
        assert_eq!(second.review_result.as_ref().map(|r| r.round), Some(2));
        // One automatic fix round only (K8 as changed by D3), and no Done without the user.
        assert!(conflict(t.mv(&task, Working)).contains("D3"));
        conflict(t.mv(&task, Done));
        let task = t.mv(&task, OrchestratorReview).unwrap();
        assert!(task.waiting.is_none(), "the column replaces the old round-limit flag");
        assert_eq!(task.review_result.as_ref().map(|r| r.round), Some(2), "the card shows the second verification");
        // The team can never move it out; the user decides.
        for to in [Working, Done, Backlog, Review] {
            assert!(conflict(t.mv(&task, to)).contains("only the user decides"), "{to}");
        }
        // It waits for the user: a desktop notification through the Coordinator's chat.
        let n = notices(&t.hub, &t.coord);
        let note = n.iter().find(|m| m.meta_str("title") == Some("JGT-1 waits for your review")).expect("a notice");
        assert!(note.body.contains("verification 2: changes, 1 critical, 0 high, 0 other"), "{}", note.body);
        // Not in progress: it does not hold a WIP slot.
        t.mv(&t.queued("next"), Working).unwrap();
    }

    #[test]
    fn decide_done_accepts_the_task() {
        let t = team();
        let task = t.working("feature");
        let task = t.to_orchestrator_review(&task, vec![high(), minor()]);
        let done = t.decide(&task, Decision::Done).unwrap();
        assert_eq!(done.status, Done);
        assert!(done.done_at.is_some());
        // Whatever is left goes to the follow-up; the decision and the move are logged.
        let follow = t.hub.tasks(WS, false).unwrap().into_iter().find(|x| x.id != task.id).unwrap();
        assert!(follow.description.contains("a problem"));
        let log = t.events(&task);
        assert!(log.iter().any(|e| e.kind == TaskEventKind::Decision && e.meta["choice"] == "done"));
        assert!(log.iter().any(|e| e.kind == TaskEventKind::Moved && e.meta["to"] == "done" && e.actor == Actor::User));
        // Deciding again: nothing to decide.
        conflict(t.decide(&task, Decision::Done));
    }

    #[test]
    fn the_verifier_records_both_results() {
        let t = team();
        let task = t.working("feature");
        t.mv(&task, Testing).unwrap();
        let by_verifier = |test: Option<TestResultInput>, review: Option<ReviewResultInput>| {
            t.hub.record_results(&task.id, None, &ResultRequest { actor: t.coordinator(), test, review })
        };
        by_verifier(
            Some(TestResultInput {
                status: TestStatus::Pass,
                worker: t.verifier.id.clone(),
                summary: "build and tests green".into(),
                failures: vec![],
            }),
            None,
        )
        .unwrap();
        t.mv(&task, Review).unwrap();
        // The verifier's severities alone mark a finding critical (`critical` may be left out).
        let critical_by_severity =
            FindingInput { critical: None, severity: Some("critical".into()), ..finding(None, Some(FindingCategory::Security)) };
        let task = by_verifier(
            None,
            Some(ReviewResultInput {
                decision: ReviewDecision::Changes,
                worker: t.verifier.id.clone(),
                findings: vec![critical_by_severity],
            }),
        )
        .unwrap();
        assert!(task.review_result.as_ref().unwrap().has_critical());
        let log = t.events(&task);
        let test = log.iter().find(|e| e.kind == TaskEventKind::TestResult).unwrap();
        assert!(test.body.starts_with("verified by verifier ") && test.body.ends_with("build and tests green"), "{}", test.body);
        let review = log.iter().find(|e| e.kind == TaskEventKind::ReviewResult).unwrap();
        assert!(review.body.starts_with("verified by verifier ") && review.body.ends_with("round 1: changes"), "{}", review.body);
        assert_eq!(review.meta["role"], "verifier");
    }

    #[test]
    fn more_rounds_needs_the_users_answer() {
        let t = team();
        let task = t.working("feature");
        let ask = |prompt: &str| {
            t.hub
                .ask(&t.coord.id, &QuestionRequest {
                    kind: MessageKind::Question,
                    prompt: prompt.into(),
                    options: vec![],
                    meta: serde_json::Value::Null,
                    timeout_secs: None,
                })
                .unwrap()
        };
        let early = ask("unrelated, asked before");
        t.hub.answer(&early.id, "yes").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let task = t.to_orchestrator_review(&task, vec![critical()]);
        let decide = |question_id: Option<String>| {
            t.hub.decide(&task.id, None, &DecisionRequest {
                actor: t.coordinator(),
                choice: Decision::MoreRounds,
                question_id,
            })
        };
        forbidden(decide(None));
        forbidden(decide(Some(early.id.clone())));
        let pending = ask("Done, one more fix, take over, or stop?");
        forbidden(decide(Some(pending.id.clone())));
        t.hub.answer(&pending.id, "One more fix").unwrap();
        let task = decide(Some(pending.id.clone())).unwrap();
        // One more coder: the task is back in Working for the third verification.
        assert_eq!((task.status, task.review_rounds, task.round_limit), (Working, 2, 3));
        assert_eq!(task.review_result.as_ref().map(|r| r.decision), Some(ReviewDecision::Changes), "the coder sees them");
        let task = t.pass(&task, ReviewDecision::Changes, vec![critical()]);
        assert_eq!(task.review_result.as_ref().map(|r| r.round), Some(3));
        // ... and the card comes back to Orchestrator review, not to another automatic fix.
        conflict(t.mv(&task, Working));
        let task = t.mv(&task, OrchestratorReview).unwrap();
        t.decide(&task, Decision::Stop).unwrap();
        // Not in Orchestrator review any more: deciding again is a conflict.
        conflict(t.hub.decide(&task.id, None, &DecisionRequest {
            actor: ActorRequest::User,
            choice: Decision::Stop,
            question_id: None,
        }));
    }

    #[test]
    fn take_over_and_stop_send_the_task_to_backlog() {
        for choice in [Decision::TakeOver, Decision::Stop] {
            let t = team();
            let task = t.working("feature");
            let task = t.to_orchestrator_review(&task, vec![critical()]);
            let task = t.decide(&task, choice).unwrap();
            assert_eq!(task.status, Backlog);
            assert!(task.waiting.is_none());
            assert!(task.review_result.is_some(), "the findings stay");
            let log = t.events(&task);
            assert!(log.iter().any(|e| e.kind == TaskEventKind::Decision && e.actor == Actor::User));
            // The team leaves it alone, even as a subtask.
            conflict(t.mv(&task, OnQueue));
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
        let task = t.to_orchestrator_review(&task, vec![minor()]);
        let done = t.decide(&task, Decision::Done).unwrap();
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
        // Round 2: approved; the user reviews it and accepts it.
        let approved = t.pass(&task, ReviewDecision::Approved, vec![]);
        assert_eq!(approved.review_result.as_ref().unwrap().round, 2);
        t.mv(&task, OrchestratorReview).unwrap();
        let done = t.decide(&task, Decision::Done).unwrap();
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
            expect("orchestrator_review", &coordinator),
            expect("done", &Actor::User),
        ]);
        let kinds: Vec<TaskEventKind> = t.events(&task).into_iter().map(|e| e.kind).collect();
        for kind in [TaskEventKind::Details, TaskEventKind::Link, TaskEventKind::TestResult, TaskEventKind::ReviewResult] {
            assert!(kinds.contains(&kind), "{kind} logged");
        }
        assert_eq!(t.hub.tasks(WS, true).unwrap().len(), 1, "no follow-up task");
        // Orchestrator review is announced in the Coordinator's chat; the user's own Done is not.
        let titles: Vec<String> =
            notices(&t.hub, &t.coord).iter().filter_map(|m| m.meta_str("title").map(str::to_owned)).collect();
        assert_eq!(titles, ["JGT-1 waits for your review"]);
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
    fn stop_keeps_backlog_positions_contiguous() {
        let t = team();
        let [a, b] = ["a", "b"].map(|x| t.create(ActorRequest::User, x, None).unwrap());
        let task = t.working("stopped");
        let task = t.to_orchestrator_review(&task, vec![critical()]);
        t.decide(&task, Decision::Stop).unwrap();
        let backlog: Vec<(String, i64)> =
            t.hub.tasks(WS, false).unwrap().into_iter().filter(|x| x.status == Backlog).map(|x| (x.title, x.position)).collect();
        assert_eq!(backlog, [("a".to_string(), 0), ("b".to_string(), 1), ("stopped".to_string(), 2)]);
        let _ = (a, b);
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

        // Orchestrator review after the second verification.
        let limit = t.working("limit");
        t.to_orchestrator_review(&limit, vec![critical()]);
        assert!(notices(&t.hub, &t.coord).iter().any(|m| m.meta_str("title") == Some("JGT-2 waits for your review")));
        t.decide(&limit, Decision::Stop).unwrap();

        // Done by the team is announced; the user's own move to Done is not.
        let done = t.working("done");
        t.pass(&done, ReviewDecision::Approved, vec![]);
        t.mv(&done, Done).unwrap();
        assert!(notices(&t.hub, &t.coord).iter().any(|m| m.meta_str("title") == Some("JGT-3 is Done")));
        let mine = t.create(ActorRequest::User, "mine", None).unwrap();
        t.user_move(&mine, Done);
        assert!(!notices(&t.hub, &t.coord).iter().any(|m| m.meta_str("title") == Some("JGT-4 is Done")));
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
}
