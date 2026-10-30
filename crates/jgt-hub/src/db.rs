//! SQLite storage. All access goes through one connection behind a mutex; every query is
//! small, and a single-user desktop hub never needs more.

use anyhow::Result;
use jgt_core::{
    Actor, Delivery, Direction, Harness, Message, MessageKind, Question, Session, SessionStatus, Task, TaskEvent,
    TaskEventKind, TaskPriority, TaskStatus, now_ms, task_key,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::{path::Path, sync::Mutex};

pub(crate) const SCHEMA_VERSION: i64 = 7;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE sessions (
    id              TEXT PRIMARY KEY,
    harness         TEXT NOT NULL,
    external_id     TEXT NOT NULL,
    title           TEXT,
    title_locked    INTEGER NOT NULL DEFAULT 0,
    cwd             TEXT,
    transcript_path TEXT,
    pid             INTEGER,
    pid_start       INTEGER,
    status          TEXT NOT NULL,
    status_text     TEXT,
    listen          INTEGER NOT NULL DEFAULT 0,
    mirror          INTEGER NOT NULL DEFAULT 0,
    archived        INTEGER NOT NULL DEFAULT 0,
    awaiting_reply  INTEGER NOT NULL DEFAULT 0,
    ended_reason    TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL,
    role            TEXT,
    parent_id       TEXT,
    cost_usd        REAL,
    run_secs        INTEGER,
    model           TEXT,
    effort          TEXT,
    tokens_in       INTEGER,
    tokens_out      INTEGER,
    UNIQUE (harness, external_id)
);
CREATE INDEX sessions_pid ON sessions (pid);

CREATE TABLE messages (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id   TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    direction    TEXT NOT NULL,
    kind         TEXT NOT NULL,
    body         TEXT NOT NULL,
    meta         TEXT NOT NULL DEFAULT '{}',
    delivery     TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    delivered_at INTEGER,
    read_at      INTEGER
);
CREATE INDEX messages_session ON messages (session_id, id);
CREATE INDEX messages_delivery ON messages (session_id, direction, delivery);

CREATE TABLE questions (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    message_id  INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,
    prompt      TEXT NOT NULL,
    options     TEXT NOT NULL DEFAULT '[]',
    answer      TEXT,
    status      TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    answered_at INTEGER,
    expires_at  INTEGER
);
CREATE INDEX questions_message ON questions (message_id);
"#;

/// The Board (schema 5): tasks, their activity log and the per-workspace key counters. Kept out
/// of `SCHEMA` so older schemas can still be derived from it; `IF NOT EXISTS` makes it safe to
/// run again after a crash, since migrations are not transactional.
pub(crate) const TASKS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id            TEXT PRIMARY KEY,
    workspace     TEXT NOT NULL,
    number        INTEGER NOT NULL,
    title         TEXT NOT NULL,
    description   TEXT NOT NULL DEFAULT '',
    details       TEXT,
    status        TEXT NOT NULL,
    position      INTEGER NOT NULL,
    priority      TEXT NOT NULL DEFAULT 'normal',
    waiting       TEXT,
    review_rounds INTEGER NOT NULL DEFAULT 0,
    round_limit   INTEGER NOT NULL DEFAULT 3,
    test_result   TEXT,
    review_result TEXT,
    run_id        TEXT,
    worktree      TEXT,
    parent_id     TEXT REFERENCES tasks (id) ON DELETE SET NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    done_at       INTEGER,
    archived      INTEGER NOT NULL DEFAULT 0,
    UNIQUE (workspace, number)
);
CREATE INDEX IF NOT EXISTS tasks_board ON tasks (workspace, status, position);

CREATE TABLE IF NOT EXISTS task_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    actor      TEXT NOT NULL,
    body       TEXT NOT NULL DEFAULT '',
    meta       TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS task_events_task ON task_events (task_id, id);

-- Keys are never reused, even after a delete: branches, worktrees and runs are named after them.
CREATE TABLE IF NOT EXISTS task_counters (
    workspace TEXT PRIMARY KEY,
    next      INTEGER NOT NULL
);
"#;

/// Upgrades from each older schema version, applied in order.
const MIGRATIONS: &[(i64, &str)] = &[
    (2, "ALTER TABLE sessions ADD COLUMN role TEXT;"),
    (3, "ALTER TABLE sessions ADD COLUMN parent_id TEXT;"),
    (4, "ALTER TABLE sessions ADD COLUMN cost_usd REAL; ALTER TABLE sessions ADD COLUMN run_secs INTEGER;"),
    (5, TASKS_SCHEMA),
    // The Orchestrator review column replaces the round-limit flag (PRD D4).
    (6, "UPDATE tasks SET status = 'orchestrator_review', waiting = NULL
            WHERE json_extract(waiting, '$.kind') = 'round_limit';"),
    // What a worker runs on, and the tokens it used (PRD H7, H8).
    (7, "ALTER TABLE sessions ADD COLUMN model TEXT; ALTER TABLE sessions ADD COLUMN effort TEXT;
         ALTER TABLE sessions ADD COLUMN tokens_in INTEGER; ALTER TABLE sessions ADD COLUMN tokens_out INTEGER;"),
];

/// Columns selected for a `Session`, including the computed counters.
const SESSION_COLUMNS: &str = r#"
    s.id, s.harness, s.external_id, s.title, s.cwd, s.transcript_path, s.pid, s.pid_start,
    s.status, s.status_text, s.listen, s.mirror, s.archived, s.awaiting_reply, s.ended_reason,
    s.created_at, s.updated_at, s.last_seen_at,
    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id AND m.direction = 'from_agent'
        AND m.kind != 'event' AND m.read_at IS NULL) AS unread,
    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id AND m.direction = 'to_agent'
        AND m.delivery = 'queued') AS queued,
    (SELECT m.body FROM messages m WHERE m.session_id = s.id AND m.kind != 'event'
        ORDER BY m.id DESC LIMIT 1) AS last_message,
    (SELECT m.created_at FROM messages m WHERE m.session_id = s.id ORDER BY m.id DESC LIMIT 1) AS last_message_at,
    s.role, s.parent_id, s.cost_usd, s.run_secs, s.model, s.effort, s.tokens_in, s.tokens_out
"#;

const MESSAGE_COLUMNS: &str =
    "id, session_id, direction, kind, body, meta, delivery, created_at, delivered_at, read_at";

const QUESTION_COLUMNS: &str =
    "id, session_id, message_id, kind, prompt, options, answer, status, created_at, answered_at, expires_at";

/// Columns selected for a `Task`; `current_worker` is computed in the same query (K14).
const TASK_COLUMNS: &str = r#"
    t.id, t.workspace, t.number, t.title, t.description, t.details, t.status, t.position, t.priority,
    t.waiting, t.review_rounds, t.round_limit, t.test_result, t.review_result, t.run_id, t.worktree,
    t.parent_id, t.created_at, t.updated_at, t.done_at, t.archived,
    (SELECT json_extract(e.meta, '$.worker') FROM task_events e WHERE e.task_id = t.id
        AND e.kind = 'worker_started' AND NOT EXISTS (SELECT 1 FROM task_events f
            WHERE f.task_id = t.id AND f.kind = 'worker_finished' AND f.id > e.id
            AND json_extract(f.meta, '$.worker') = json_extract(e.meta, '$.worker'))
        ORDER BY e.id DESC LIMIT 1) AS current_worker
"#;

const TASK_EVENT_COLUMNS: &str = "id, task_id, kind, actor, body, meta, created_at";

pub struct Db {
    conn: Mutex<Connection>,
}

/// Fields of a session the harness reports when it (re)connects.
pub struct Registration<'a> {
    pub harness: Harness,
    pub external_id: &'a str,
    pub cwd: Option<&'a str>,
    pub title: Option<&'a str>,
    pub pid: Option<i64>,
    pub pid_start: Option<i64>,
    pub transcript_path: Option<&'a str>,
    pub status: SessionStatus,
    pub role: Option<&'a str>,
    pub parent_id: Option<&'a str>,
}

fn parse<T: std::str::FromStr<Err = String>>(idx: usize, value: String) -> rusqlite::Result<T> {
    value.parse().map_err(|e: String| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, e.into())
    })
}

fn session_from_row(row: &Row) -> rusqlite::Result<Session> {
    let last_message: Option<String> = row.get(20)?;
    Ok(Session {
        id: row.get(0)?,
        harness: parse(1, row.get(1)?)?,
        external_id: row.get(2)?,
        title: row.get(3)?,
        cwd: row.get(4)?,
        transcript_path: row.get(5)?,
        pid: row.get(6)?,
        pid_start: row.get(7)?,
        status: parse(8, row.get(8)?)?,
        status_text: row.get(9)?,
        listen: row.get(10)?,
        mirror: row.get(11)?,
        archived: row.get(12)?,
        awaiting_reply: row.get(13)?,
        ended_reason: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
        last_seen_at: row.get(17)?,
        unread: row.get(18)?,
        queued: row.get(19)?,
        last_message: last_message.map(|m| preview(&m)),
        last_message_at: row.get(21)?,
        role: row.get(22)?,
        parent_id: row.get(23)?,
        cost_usd: row.get(24)?,
        run_secs: row.get(25)?,
        model: row.get(26)?,
        effort: row.get(27)?,
        tokens_in: row.get(28)?,
        tokens_out: row.get(29)?,
    })
}

/// Reads a JSON column; a value that does not parse is a conversion error, not silently dropped.
fn json<T: serde::de::DeserializeOwned>(idx: usize, value: Option<String>) -> rusqlite::Result<Option<T>> {
    value
        .map(|v| {
            serde_json::from_str(&v).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, e.into())
            })
        })
        .transpose()
}

pub(crate) fn to_json<T: serde::Serialize>(value: &Option<T>) -> Option<String> {
    value.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default())
}

fn task_from_row(row: &Row) -> rusqlite::Result<Task> {
    let number: i64 = row.get(2)?;
    Ok(Task {
        id: row.get(0)?,
        workspace: row.get(1)?,
        number,
        key: task_key(number),
        title: row.get(3)?,
        description: row.get(4)?,
        details: row.get(5)?,
        status: parse(6, row.get(6)?)?,
        position: row.get(7)?,
        priority: parse(8, row.get(8)?)?,
        waiting: json(9, row.get(9)?)?,
        review_rounds: row.get(10)?,
        round_limit: row.get(11)?,
        test_result: json(12, row.get(12)?)?,
        review_result: json(13, row.get(13)?)?,
        run_id: row.get(14)?,
        worktree: row.get(15)?,
        parent_id: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
        done_at: row.get(19)?,
        archived: row.get(20)?,
        current_worker: row.get(21)?,
    })
}

fn task_event_from_row(row: &Row) -> rusqlite::Result<TaskEvent> {
    let meta: String = row.get(5)?;
    Ok(TaskEvent {
        id: row.get(0)?,
        task_id: row.get(1)?,
        kind: parse(2, row.get(2)?)?,
        actor: json(3, Some(row.get(3)?))?.unwrap_or(Actor::Hub),
        body: row.get(4)?,
        meta: serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null),
        created_at: row.get(6)?,
    })
}

/// `ORDER BY` expression putting tasks in Board column order.
fn column_order() -> String {
    let cases: String =
        TaskStatus::ALL.iter().enumerate().map(|(i, s)| format!(" WHEN '{}' THEN {i}", s.as_str())).collect();
    format!("CASE t.status{cases} END")
}

fn message_from_row(row: &Row) -> rusqlite::Result<Message> {
    let meta: String = row.get(5)?;
    Ok(Message {
        id: row.get(0)?,
        session_id: row.get(1)?,
        direction: parse(2, row.get(2)?)?,
        kind: parse(3, row.get(3)?)?,
        body: row.get(4)?,
        meta: serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null),
        delivery: parse(6, row.get(6)?)?,
        created_at: row.get(7)?,
        delivered_at: row.get(8)?,
        read_at: row.get(9)?,
        question: None,
    })
}

fn question_from_row(row: &Row) -> rusqlite::Result<Question> {
    let options: String = row.get(5)?;
    Ok(Question {
        id: row.get(0)?,
        session_id: row.get(1)?,
        message_id: row.get(2)?,
        kind: parse(3, row.get(3)?)?,
        prompt: row.get(4)?,
        options: serde_json::from_str(&options).unwrap_or_default(),
        answer: row.get(6)?,
        status: parse(7, row.get(7)?)?,
        created_at: row.get(8)?,
        answered_at: row.get(9)?,
        expires_at: row.get(10)?,
    })
}

/// A one-line preview of a message body.
fn preview(body: &str) -> String {
    let line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 140 { format!("{}…", line.chars().take(140).collect::<String>()) } else { line }
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(SCHEMA)?;
            conn.execute_batch(TASKS_SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version < SCHEMA_VERSION {
            for (to, sql) in MIGRATIONS.iter().filter(|(to, _)| *to > version) {
                conn.execute_batch(sql)?;
                conn.pragma_update(None, "user_version", to)?;
            }
        } else if version > SCHEMA_VERSION {
            anyhow::bail!("database {} was created by a newer jg-team-dev (schema {version})", path.display());
        }
        Ok(Db { conn: Mutex::new(conn) })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&conn)?)
    }

    /// Runs Board queries in one transaction, committed only when `f` succeeds. `f` must not
    /// call other `Db` methods: the connection is locked while it runs.
    pub fn board<T, E: From<rusqlite::Error>>(&self, f: impl FnOnce(&Board) -> Result<T, E>) -> Result<T, E> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.unchecked_transaction()?;
        let out = f(&Board { c: &tx })?;
        tx.commit()?;
        Ok(out)
    }

    // ── sessions ────────────────────────────────────────────────────────────

    /// The Coordinator of a workspace: the newest coordinator session with that folder that has
    /// not ended (a reset retires, i.e. ends, the previous one).
    pub fn current_coordinator(&self, workspace: &str) -> Result<Option<Session>> {
        let ws_norm = jgt_core::team::normalize_folder(workspace);
        self.with(|c| {
            c.query_row(
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.role = 'coordinator' AND s.status != 'ended'
                     AND (s.cwd = ?1 OR s.cwd = ?1 || '/' OR (s.cwd IS NOT NULL AND rtrim(s.cwd, '/') = ?1))
                     ORDER BY s.created_at DESC LIMIT 1"
                ),
                [ws_norm],
                session_from_row,
            )
            .optional()
        })
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        self.with(|c| {
            c.query_row(&format!("SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.id = ?1"), [id], session_from_row)
                .optional()
        })
    }

    pub fn session_by_external(&self, harness: Harness, external_id: &str) -> Result<Option<Session>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.harness = ?1 AND s.external_id = ?2"),
                params![harness.as_str(), external_id],
                session_from_row,
            )
            .optional()
        })
    }

    /// The connected session most recently active in the given harness process.
    pub fn session_by_pid(&self, pid: i64) -> Result<Option<Session>> {
        self.with(|c| {
            c.query_row(
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.pid = ?1
                     AND s.status IN ('active', 'idle', 'listening')
                     ORDER BY s.last_seen_at DESC LIMIT 1"
                ),
                [pid],
                session_from_row,
            )
            .optional()
        })
    }

    pub fn sessions(&self, include_archived: bool) -> Result<Vec<Session>> {
        self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions s WHERE (?1 OR s.archived = 0)
                 ORDER BY COALESCE(last_message_at, s.updated_at) DESC"
            ))?;
            stmt.query_map([include_archived], session_from_row)?.collect()
        })
    }

    /// Connected sessions; checked by the liveness monitor.
    pub fn connected_sessions(&self) -> Result<Vec<Session>> {
        self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.status IN ('active', 'idle', 'listening')"
            ))?;
            stmt.query_map([], session_from_row)?.collect()
        })
    }

    pub fn sessions_with_status(&self, status: SessionStatus) -> Result<Vec<Session>> {
        self.with(|c| {
            let mut stmt = c.prepare(&format!("SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.status = ?1"))?;
            stmt.query_map([status.as_str()], session_from_row)?.collect()
        })
    }

    /// Creates the session, or updates it when the harness already registered it before.
    /// Returns the session and its status before the call (None when new).
    pub fn upsert_session(&self, r: &Registration) -> Result<(Session, Option<SessionStatus>)> {
        let now = now_ms();
        let previous = self.session_by_external(r.harness, r.external_id)?;
        let id = match &previous {
            Some(s) => {
                self.with(|c| {
                    c.execute(
                        "UPDATE sessions SET
                            cwd = COALESCE(?2, cwd),
                            title = CASE WHEN title_locked = 0 AND ?3 IS NOT NULL THEN ?3 ELSE title END,
                            pid = COALESCE(?4, pid),
                            pid_start = CASE WHEN ?4 IS NOT NULL THEN ?5 ELSE pid_start END,
                            transcript_path = COALESCE(?6, transcript_path),
                            status = ?7, ended_reason = NULL, updated_at = ?8, last_seen_at = ?8,
                            role = COALESCE(?9, role), parent_id = COALESCE(?10, parent_id)
                         WHERE id = ?1",
                        params![
                            s.id,
                            r.cwd,
                            r.title,
                            r.pid,
                            r.pid_start,
                            r.transcript_path,
                            r.status.as_str(),
                            now,
                            r.role,
                            r.parent_id
                        ],
                    )
                })?;
                s.id.clone()
            }
            None => {
                let id = ulid::Ulid::generate().to_string();
                self.with(|c| {
                    c.execute(
                        "INSERT INTO sessions (id, harness, external_id, title, cwd, transcript_path, pid,
                            pid_start, status, created_at, updated_at, last_seen_at, role, parent_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?10, ?11, ?12)",
                        params![
                            id,
                            r.harness.as_str(),
                            r.external_id,
                            r.title,
                            r.cwd,
                            r.transcript_path,
                            r.pid,
                            r.pid_start,
                            r.status.as_str(),
                            now,
                            r.role,
                            r.parent_id
                        ],
                    )
                })?;
                id
            }
        };
        let session = self.session(&id)?.ok_or_else(|| anyhow::anyhow!("session vanished"))?;
        Ok((session, previous.map(|s| s.status)))
    }

    pub fn set_status(&self, id: &str, status: SessionStatus, ended_reason: Option<&str>) -> Result<()> {
        let now = now_ms();
        self.with(|c| {
            c.execute(
                "UPDATE sessions SET status = ?2, ended_reason = ?3, updated_at = ?4, last_seen_at = ?4 WHERE id = ?1",
                params![id, status.as_str(), ended_reason, now],
            )
        })?;
        Ok(())
    }

    /// Like `set_status`, but only when the session is currently in `from`.
    /// Listening → idle for every session (used at start-up, when no waiter can exist).
    pub fn reset_listening(&self) -> Result<usize> {
        let now = now_ms();
        self.with(|c| {
            c.execute("UPDATE sessions SET status = 'idle', updated_at = ?1 WHERE status = 'listening'", params![now])
        })
    }

    pub fn set_status_if(&self, id: &str, from: SessionStatus, to: SessionStatus) -> Result<bool> {
        let now = now_ms();
        let n = self.with(|c| {
            c.execute(
                "UPDATE sessions SET status = ?3, updated_at = ?4 WHERE id = ?1 AND status = ?2",
                params![id, from.as_str(), to.as_str(), now],
            )
        })?;
        Ok(n > 0)
    }

    pub fn touch(&self, id: &str) -> Result<()> {
        self.with(|c| c.execute("UPDATE sessions SET last_seen_at = ?2 WHERE id = ?1", params![id, now_ms()]))?;
        Ok(())
    }

    pub fn set_awaiting_reply(&self, id: &str, value: bool) -> Result<()> {
        self.with(|c| c.execute("UPDATE sessions SET awaiting_reply = ?2 WHERE id = ?1", params![id, value]))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn patch_session(
        &self,
        id: &str,
        title: Option<&str>,
        title_from_user: bool,
        status_text: Option<&str>,
        listen: Option<bool>,
        mirror: Option<bool>,
        archived: Option<bool>,
        run: Option<(Option<f64>, i64)>,
    ) -> Result<()> {
        let now = now_ms();
        self.with(|c| {
            if let Some(title) = title {
                let title = Some(title.trim()).filter(|t| !t.is_empty());
                if title_from_user {
                    c.execute(
                        "UPDATE sessions SET title = ?2, title_locked = ?3 WHERE id = ?1",
                        params![id, title, title.is_some()],
                    )?;
                } else {
                    c.execute("UPDATE sessions SET title = ?2 WHERE id = ?1 AND title_locked = 0", params![id, title])?;
                }
            }
            if let Some(text) = status_text {
                let text = Some(text.trim()).filter(|t| !t.is_empty());
                c.execute("UPDATE sessions SET status_text = ?2 WHERE id = ?1", params![id, text])?;
            }
            if let Some(v) = listen {
                c.execute("UPDATE sessions SET listen = ?2 WHERE id = ?1", params![id, v])?;
            }
            if let Some(v) = mirror {
                c.execute("UPDATE sessions SET mirror = ?2 WHERE id = ?1", params![id, v])?;
            }
            if let Some(v) = archived {
                c.execute("UPDATE sessions SET archived = ?2 WHERE id = ?1", params![id, v])?;
            }
            if let Some((cost, secs)) = run {
                c.execute("UPDATE sessions SET cost_usd = ?2, run_secs = ?3 WHERE id = ?1", params![id, cost, secs])?;
            }
            c.execute("UPDATE sessions SET updated_at = ?2 WHERE id = ?1", params![id, now])
        })?;
        Ok(())
    }

    /// A worker's model and effort, and the tokens of its run; `None` leaves a field as it is.
    pub fn set_run_info(
        &self,
        id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        tokens: (Option<i64>, Option<i64>),
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE sessions SET model = COALESCE(?2, model), effort = COALESCE(?3, effort),
                    tokens_in = COALESCE(?4, tokens_in), tokens_out = COALESCE(?5, tokens_out) WHERE id = ?1",
                params![id, model, effort, tokens.0, tokens.1],
            )
        })?;
        Ok(())
    }

    pub fn delete_session(&self, id: &str) -> Result<bool> {
        Ok(self.with(|c| c.execute("DELETE FROM sessions WHERE id = ?1", [id]))? > 0)
    }

    /// Sessions of a harness process that are idle and have messages waiting.
    pub fn pending_for_pid(&self, pid: i64) -> Result<Vec<Session>> {
        self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.pid = ?1 AND s.status IN ('idle', 'listening')
                 AND EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id
                     AND m.direction = 'to_agent' AND m.delivery = 'queued')"
            ))?;
            stmt.query_map([pid], session_from_row)?.collect()
        })
    }

    pub fn count_sessions(&self) -> Result<(i64, i64)> {
        self.with(|c| {
            c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(status IN ('active', 'idle', 'listening')), 0) FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
    }

    // ── messages ────────────────────────────────────────────────────────────

    pub fn insert_message(
        &self,
        session_id: &str,
        direction: Direction,
        kind: MessageKind,
        body: &str,
        meta: &serde_json::Value,
        delivery: Delivery,
    ) -> Result<Message> {
        let now = now_ms();
        let meta = if meta.is_null() { "{}".to_string() } else { meta.to_string() };
        let delivered_at = (delivery != Delivery::Queued).then_some(now);
        let id = self.with(|c| {
            c.execute(
                "INSERT INTO messages (session_id, direction, kind, body, meta, delivery, created_at, delivered_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![session_id, direction.as_str(), kind.as_str(), body, meta, delivery.as_str(), now, delivered_at],
            )?;
            c.execute("UPDATE sessions SET updated_at = ?2 WHERE id = ?1", params![session_id, now])?;
            Ok(c.last_insert_rowid())
        })?;
        self.message(id)?.ok_or_else(|| anyhow::anyhow!("message vanished"))
    }

    pub fn message(&self, id: i64) -> Result<Option<Message>> {
        let msg = self.with(|c| {
            c.query_row(&format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = ?1"), [id], message_from_row)
                .optional()
        })?;
        match msg {
            Some(mut m) => {
                m.question = self.question_for_message(m.id)?;
                Ok(Some(m))
            }
            None => Ok(None),
        }
    }

    /// A page of messages, oldest first. `before` pages backwards from a message id.
    pub fn messages(&self, session_id: &str, before: Option<i64>, limit: i64) -> Result<Vec<Message>> {
        let mut messages = self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 AND (?2 IS NULL OR id < ?2)
                 ORDER BY id DESC LIMIT ?3"
            ))?;
            stmt.query_map(params![session_id, before, limit], message_from_row)?.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        messages.reverse();
        let questions = self.with(|c| {
            let mut stmt = c.prepare(&format!("SELECT {QUESTION_COLUMNS} FROM questions WHERE session_id = ?1"))?;
            stmt.query_map([session_id], question_from_row)?.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        for q in questions {
            if let Some(m) = messages.iter_mut().find(|m| m.id == q.message_id) {
                m.question = Some(q);
            }
        }
        Ok(messages)
    }

    /// Marks every queued message for the session delivered and returns them.
    pub fn take_queued(&self, session_id: &str) -> Result<Vec<Message>> {
        let now = now_ms();
        let ids: Vec<i64> = self.with(|c| {
            let tx = c.unchecked_transaction()?;
            let ids = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM messages WHERE session_id = ?1 AND direction = 'to_agent'
                     AND delivery = 'queued' ORDER BY id",
                )?;
                stmt.query_map([session_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?
            };
            for id in &ids {
                tx.execute("UPDATE messages SET delivery = 'delivered', delivered_at = ?2 WHERE id = ?1", params![id, now])?;
            }
            tx.commit()?;
            Ok(ids)
        })?;
        ids.into_iter().filter_map(|id| self.message(id).transpose()).collect()
    }

    pub fn queued(&self, session_id: &str) -> Result<Vec<Message>> {
        self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 AND direction = 'to_agent'
                 AND delivery = 'queued' ORDER BY id"
            ))?;
            stmt.query_map([session_id], message_from_row)?.collect()
        })
    }

    /// Marks queued messages undeliverable (the session's context is gone).
    pub fn fail_queued(&self, session_id: &str) -> Result<Vec<Message>> {
        let queued = self.queued(session_id)?;
        self.with(|c| {
            c.execute(
                "UPDATE messages SET delivery = 'undeliverable' WHERE session_id = ?1 AND direction = 'to_agent'
                 AND delivery = 'queued'",
                [session_id],
            )
        })?;
        queued.iter().filter_map(|m| self.message(m.id).transpose()).collect()
    }

    /// Marks the agent's messages read; returns how many changed.
    pub fn mark_read(&self, session_id: &str) -> Result<usize> {
        let now = now_ms();
        self.with(|c| {
            c.execute(
                "UPDATE messages SET read_at = ?2, delivery = 'read' WHERE session_id = ?1
                 AND direction = 'from_agent' AND read_at IS NULL",
                params![session_id, now],
            )
        })
    }

    /// Whether the agent wrote to the user itself since the last message it received.
    pub fn agent_spoke_since_last_delivery(&self, session_id: &str) -> Result<bool> {
        self.with(|c| {
            c.query_row(
                "SELECT EXISTS (SELECT 1 FROM messages WHERE session_id = ?1 AND direction = 'from_agent'
                    AND kind IN ('text', 'reply', 'notify', 'question')
                    AND id > COALESCE((SELECT MAX(id) FROM messages WHERE session_id = ?1
                        AND direction = 'to_agent' AND delivery != 'queued'), 0))",
                [session_id],
                |r| r.get(0),
            )
        })
    }

    // ── questions ───────────────────────────────────────────────────────────

    pub fn insert_question(&self, q: &Question) -> Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO questions ({QUESTION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
                params![
                    q.id,
                    q.session_id,
                    q.message_id,
                    q.kind.as_str(),
                    q.prompt,
                    serde_json::to_string(&q.options).unwrap_or_else(|_| "[]".into()),
                    q.answer,
                    q.status.as_str(),
                    q.created_at,
                    q.answered_at,
                    q.expires_at
                ],
            )
        })?;
        Ok(())
    }

    pub fn question(&self, id: &str) -> Result<Option<Question>> {
        self.with(|c| {
            c.query_row(&format!("SELECT {QUESTION_COLUMNS} FROM questions WHERE id = ?1"), [id], question_from_row)
                .optional()
        })
    }

    fn question_for_message(&self, message_id: i64) -> Result<Option<Question>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {QUESTION_COLUMNS} FROM questions WHERE message_id = ?1"),
                [message_id],
                question_from_row,
            )
            .optional()
        })
    }

    /// Records an answer; returns false when the question is no longer pending.
    pub fn answer_question(&self, id: &str, answer: &str) -> Result<bool> {
        let now = now_ms();
        let n = self.with(|c| {
            c.execute(
                "UPDATE questions SET answer = ?2, status = 'answered', answered_at = ?3
                 WHERE id = ?1 AND status = 'pending'",
                params![id, answer, now],
            )
        })?;
        if n > 0 {
            self.with(|c| {
                c.execute(
                    "UPDATE messages SET read_at = COALESCE(read_at, ?2), delivery = 'read'
                     WHERE id = (SELECT message_id FROM questions WHERE id = ?1)",
                    params![id, now],
                )
            })?;
        }
        Ok(n > 0)
    }

    /// Expires pending questions past their deadline, or all of a session's when given.
    pub fn expire_questions(&self, session_id: Option<&str>) -> Result<Vec<Question>> {
        let now = now_ms();
        let ids: Vec<String> = self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM questions WHERE status = 'pending'
                 AND ((?1 IS NOT NULL AND session_id = ?1) OR (?1 IS NULL AND expires_at IS NOT NULL AND expires_at < ?2))",
            )?;
            stmt.query_map(params![session_id, now], |r| r.get(0))?.collect()
        })?;
        for id in &ids {
            self.with(|c| c.execute("UPDATE questions SET status = 'expired' WHERE id = ?1", [id]))?;
        }
        ids.iter().filter_map(|id| self.question(id).transpose()).collect()
    }
}

/// A new task; the number and position are allocated by `Board::insert_task`.
pub struct NewTask<'a> {
    pub workspace: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub priority: TaskPriority,
    pub status: TaskStatus,
    pub parent_id: Option<&'a str>,
}

/// Board queries inside one transaction (see `Db::board`).
pub struct Board<'a> {
    c: &'a Connection,
}

impl Board<'_> {
    pub fn task(&self, id: &str) -> rusqlite::Result<Option<Task>> {
        self.c.query_row(&format!("SELECT {TASK_COLUMNS} FROM tasks t WHERE t.id = ?1"), [id], task_from_row).optional()
    }

    pub fn task_by_key(&self, workspace: &str, number: i64) -> rusqlite::Result<Option<Task>> {
        self.c
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks t WHERE t.workspace = ?1 AND t.number = ?2"),
                params![workspace, number],
                task_from_row,
            )
            .optional()
    }

    /// A workspace's tasks in Board order: column, then position.
    pub fn tasks(&self, workspace: &str, include_archived: bool) -> rusqlite::Result<Vec<Task>> {
        let mut stmt = self.c.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks t WHERE t.workspace = ?1 AND (?2 OR t.archived = 0)
             ORDER BY {}, t.position, t.number",
            column_order()
        ))?;
        stmt.query_map(params![workspace, include_archived], task_from_row)?.collect()
    }

    /// Creates a task with the workspace's next key, at the end of its column.
    pub fn insert_task(&self, new: &NewTask) -> rusqlite::Result<Task> {
        let now = now_ms();
        self.c.execute(
            "INSERT INTO task_counters (workspace, next) VALUES (?1, 1) ON CONFLICT (workspace) DO NOTHING",
            [new.workspace],
        )?;
        let number: i64 =
            self.c.query_row("SELECT next FROM task_counters WHERE workspace = ?1", [new.workspace], |r| r.get(0))?;
        self.c.execute("UPDATE task_counters SET next = next + 1 WHERE workspace = ?1", [new.workspace])?;
        let id = ulid::Ulid::generate().to_string();
        self.c.execute(
            "INSERT INTO tasks (id, workspace, number, title, description, status, position, priority, parent_id,
                created_at, updated_at, round_limit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                (SELECT COALESCE(MAX(position) + 1, 0) FROM tasks WHERE workspace = ?2 AND status = ?6 AND archived = 0),
                ?7, ?8, ?9, ?9, ?10)",
            params![
                id,
                new.workspace,
                number,
                new.title,
                new.description,
                new.status.as_str(),
                new.priority.as_str(),
                new.parent_id,
                now,
                jgt_core::ROUND_LIMIT
            ],
        )?;
        self.task(&id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
    }

    /// Writes every field of `task` except its column and position (see `place`).
    pub fn save(&self, task: &Task) -> rusqlite::Result<()> {
        self.c.execute(
            "UPDATE tasks SET title = ?2, description = ?3, details = ?4, priority = ?5, waiting = ?6,
                review_rounds = ?7, round_limit = ?8, test_result = ?9, review_result = ?10, run_id = ?11,
                worktree = ?12, parent_id = ?13, done_at = ?14, archived = ?15, updated_at = ?16
             WHERE id = ?1",
            params![
                task.id,
                task.title,
                task.description,
                task.details,
                task.priority.as_str(),
                to_json(&task.waiting),
                task.review_rounds,
                task.round_limit,
                to_json(&task.test_result),
                to_json(&task.review_result),
                task.run_id,
                task.worktree,
                task.parent_id,
                task.done_at,
                task.archived,
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// Writes only the named columns of a task (plus `updated_at`), so an edit cannot overwrite
    /// what other writes changed. Column names come from the hub, never from a request.
    pub fn set_columns(&self, id: &str, columns: &[(&str, &dyn rusqlite::ToSql)]) -> rusqlite::Result<()> {
        let sets: String = columns.iter().enumerate().map(|(i, (name, _))| format!("{name} = ?{}, ", i + 3)).collect();
        let sql = format!("UPDATE tasks SET {sets}updated_at = ?2 WHERE id = ?1");
        let now = now_ms();
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&id, &now];
        values.extend(columns.iter().map(|(_, v)| *v));
        self.c.execute(&sql, values.as_slice())?;
        Ok(())
    }

    /// Puts a task in column `to` at index `position` (the end when absent) and renumbers that
    /// column from 0. A task leaving its column closes the gap it leaves there (see `compact`).
    pub fn place(&self, id: &str, workspace: &str, to: TaskStatus, position: Option<i64>) -> rusqlite::Result<()> {
        let from: String = self.c.query_row("SELECT status FROM tasks WHERE id = ?1", [id], |r| r.get(0))?;
        let mut ids = self.column_ids(workspace, to.as_str(), id)?;
        let at = position.map_or(ids.len(), |p| p.clamp(0, ids.len() as i64) as usize);
        ids.insert(at, id.to_string());
        self.c.execute(
            "UPDATE tasks SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, to.as_str(), now_ms()],
        )?;
        self.renumber(&ids)?;
        if from != to.as_str() {
            self.compact(workspace, &from)?;
        }
        Ok(())
    }

    /// The ids of a column's tasks in order, without archived tasks and `except`.
    fn column_ids(&self, workspace: &str, status: &str, except: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.c.prepare(
            "SELECT id FROM tasks WHERE workspace = ?1 AND status = ?2 AND id != ?3 AND archived = 0
             ORDER BY position, number",
        )?;
        stmt.query_map(params![workspace, status, except], |r| r.get(0))?.collect()
    }

    fn renumber(&self, ids: &[String]) -> rusqlite::Result<()> {
        let mut stmt = self.c.prepare("UPDATE tasks SET position = ?2 WHERE id = ?1")?;
        for (i, id) in ids.iter().enumerate() {
            stmt.execute(params![id, i as i64])?;
        }
        Ok(())
    }

    /// Numbers a column's tasks from 0 again, so positions stay the index the app places cards by.
    fn compact(&self, workspace: &str, status: &str) -> rusqlite::Result<()> {
        self.renumber(&self.column_ids(workspace, status, "")?)
    }

    /// Deletes a task and closes the gap it leaves in its column.
    pub fn delete_task(&self, id: &str) -> rusqlite::Result<bool> {
        let column: Option<(String, String)> = self
            .c
            .query_row("SELECT workspace, status FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((workspace, status)) = column else { return Ok(false) };
        self.c.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        self.compact(&workspace, &status)?;
        Ok(true)
    }

    /// Done tasks finished before `cutoff` (ms) and not archived yet, in every workspace.
    pub fn done_before(&self, cutoff: i64) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.c.prepare(
            "SELECT id FROM tasks WHERE status = 'done' AND archived = 0 AND done_at IS NOT NULL AND done_at < ?1",
        )?;
        stmt.query_map([cutoff], |r| r.get(0))?.collect()
    }

    /// Hides a task from the Board (K15) and closes the gap it leaves in its column.
    pub fn archive(&self, id: &str) -> rusqlite::Result<()> {
        let (workspace, status): (String, String) =
            self.c.query_row("SELECT workspace, status FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        self.set_columns(id, &[("archived", &true as &dyn rusqlite::ToSql)])?;
        self.compact(&workspace, &status)
    }

    /// Tasks of the workspace the team is working on (Working, Testing, Review), except `except`.
    pub fn count_in_progress(&self, workspace: &str, except: &str) -> rusqlite::Result<i64> {
        self.c.query_row(
            "SELECT COUNT(*) FROM tasks WHERE workspace = ?1 AND id != ?2
             AND status IN ('working', 'testing', 'review') AND archived = 0",
            params![workspace, except],
            |r| r.get(0),
        )
    }

    pub fn insert_event(
        &self,
        task_id: &str,
        kind: TaskEventKind,
        actor: &Actor,
        body: &str,
        meta: &serde_json::Value,
    ) -> rusqlite::Result<TaskEvent> {
        let meta = if meta.is_null() { "{}".to_string() } else { meta.to_string() };
        self.c.execute(
            "INSERT INTO task_events (task_id, kind, actor, body, meta, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![task_id, kind.as_str(), serde_json::to_string(actor).unwrap_or_default(), body, meta, now_ms()],
        )?;
        let id = self.c.last_insert_rowid();
        self.c.query_row(
            &format!("SELECT {TASK_EVENT_COLUMNS} FROM task_events WHERE id = ?1"),
            [id],
            task_event_from_row,
        )
    }

    /// A task's activity log, oldest first.
    pub fn events(&self, task_id: &str) -> rusqlite::Result<Vec<TaskEvent>> {
        let mut stmt =
            self.c.prepare(&format!("SELECT {TASK_EVENT_COLUMNS} FROM task_events WHERE task_id = ?1 ORDER BY id"))?;
        stmt.query_map([task_id], task_event_from_row)?.collect()
    }

    /// Whether the user put the task on the queue (created it there, or moved it there).
    pub fn queued_by_user(&self, task_id: &str) -> rusqlite::Result<bool> {
        self.c.query_row(
            "SELECT EXISTS (SELECT 1 FROM task_events WHERE task_id = ?1
                AND json_extract(actor, '$.kind') = 'user'
                AND ((kind = 'moved' AND json_extract(meta, '$.to') = 'on_queue')
                    OR (kind = 'created' AND json_extract(meta, '$.status') = 'on_queue')))",
            [task_id],
            |r| r.get(0),
        )
    }

    /// Every task with a *Waiting* flag, in any workspace.
    pub fn waiting_tasks(&self) -> rusqlite::Result<Vec<Task>> {
        let ids: Vec<String> = self
            .c
            .prepare("SELECT id FROM tasks WHERE waiting IS NOT NULL")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.iter().filter_map(|id| self.task(id).transpose()).collect()
    }

    /// When the task last moved into `status`, from its activity log.
    pub fn entered(&self, task_id: &str, status: TaskStatus) -> rusqlite::Result<Option<i64>> {
        self.c.query_row(
            "SELECT MAX(created_at) FROM task_events WHERE task_id = ?1 AND kind = 'moved'
                AND json_extract(meta, '$.to') = ?2",
            params![task_id, status.as_str()],
            |r| r.get(0),
        )
    }

    /// Whether a K8 decision taking the task away from the team (take over, stop) was made.
    pub fn taken_from_team(&self, task_id: &str) -> rusqlite::Result<bool> {
        self.c.query_row(
            "SELECT EXISTS (SELECT 1 FROM task_events WHERE task_id = ?1 AND kind = 'decision'
                AND json_extract(meta, '$.choice') IN ('take_over', 'stop'))",
            [task_id],
            |r| r.get(0),
        )
    }
}
