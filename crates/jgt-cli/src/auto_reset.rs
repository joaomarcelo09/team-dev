//! Automatic Coordinator reset (JGT-65). The Coordinator's Stop hook measures the session's
//! context from the tail of its transcript; past `reset_window` it tells the Coordinator to reset
//! to a fresh one at the next safe point (`jg-team-dev reset --summary <file>`).
//!
//! Everything here is best effort: a missing or malformed transcript, or a hub that cannot be
//! asked, means no notice, never a failed hook.

use jgt_core::{Session, Task, paths};
use serde_json::Value;
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// How much of the transcript's end is read: enough for the last assistant entry even after a
/// large tool result, without reading a days-long transcript whole.
const TAIL_BYTES: u64 = 512 * 1024;

/// The context size of the session's last assistant turn, from the tail of its transcript
/// (`input_tokens + cache_creation_input_tokens + cache_read_input_tokens`). `None` when the file
/// is missing or unreadable or its tail holds no usable usage.
pub fn context_tokens(transcript: &Path) -> Option<u64> {
    let mut file = std::fs::File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // Reading from the middle of the file cuts the first line: drop it.
    let text = if start > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
    last_usage(text)
}

/// The context size of the last main-thread assistant entry with a non-zero usage in `jsonl`.
/// Lines that are not JSON, other entry types and sidechain entries are skipped.
pub fn last_usage(jsonl: &str) -> Option<u64> {
    jsonl.lines().rev().find_map(|line| {
        let entry: Value = serde_json::from_str(line.trim()).ok()?;
        if entry.get("type").and_then(Value::as_str) != Some("assistant")
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            return None;
        }
        let usage = entry.get("message")?.get("usage")?;
        let field = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        // Synthetic entries (e.g. an API error) carry a zero usage: not a measurement.
        Some(field("input_tokens") + field("cache_creation_input_tokens") + field("cache_read_input_tokens"))
            .filter(|t| *t > 0)
    })
}

/// The context size when it has reached `limit`; `None` when under it, unmeasured, or the
/// automatic reset is off (`limit` is `None`).
pub fn over_limit(tokens: Option<u64>, limit: Option<u64>) -> Option<u64> {
    let (tokens, limit) = (tokens?, limit?);
    (limit > 0 && tokens >= limit).then_some(tokens)
}

/// What stops the Coordinator from resetting now, readably; empty at a safe point. Detectable
/// from the hub: a card in Working, Testing or Review (never reset mid-card), a card in a git
/// column (a git step not finished), a card waiting for the user (an open decision), and a
/// running worker of this Coordinator. A question the Coordinator asks only in its chat, or a git
/// command it is about to run, is not visible here: the Coordinator skill enforces those.
pub fn safe_point_blockers(coordinator_id: &str, tasks: &[Task], sessions: &[Session]) -> Vec<String> {
    let mut out = Vec::new();
    for t in tasks.iter().filter(|t| !t.archived) {
        if t.status.in_progress() || t.status.is_git_stage() {
            out.push(format!("{} is in {}", t.key, t.status.label()));
        } else if t.waiting.is_some() {
            out.push(format!("{} waits for the user", t.key));
        }
    }
    for s in sessions {
        if s.parent_id.as_deref() == Some(coordinator_id) && s.status.is_connected() {
            out.push(format!("worker {} is running", s.display_name()));
        }
    }
    out
}

/// What the last notice of a session said, kept in its marker file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Told to reset once the blockers are done.
    Blocked,
    /// Told to reset now.
    Safe,
}

impl Notice {
    fn as_str(self) -> &'static str {
        match self {
            Notice::Blocked => "blocked",
            Notice::Safe => "safe",
        }
    }
}

/// Whether to tell the Coordinator again. At most two notices per session: the first time it is
/// over the limit (saying what to finish first, if anything), and once more when a safe point is
/// first reached after a "blocked" notice. Every notice ends the Coordinator's turn with a Stop
/// hook block, so repeating it on every turn would loop.
pub fn should_notify(last: Option<Notice>, now: Notice) -> bool {
    match last {
        None => true,
        Some(Notice::Blocked) => now == Notice::Safe,
        Some(Notice::Safe) => false,
    }
}

/// `<runtime dir>/reset-notice/<hub session id>`.
pub fn marker_path(session_id: &str) -> PathBuf {
    paths::runtime_dir().join("reset-notice").join(session_id)
}

pub fn read_marker(path: &Path) -> Option<Notice> {
    match std::fs::read_to_string(path).ok()?.trim() {
        "blocked" => Some(Notice::Blocked),
        "safe" => Some(Notice::Safe),
        _ => None,
    }
}

pub fn write_marker(path: &Path, notice: Notice) -> std::io::Result<()> {
    jgt_core::board::write_atomic(path, notice.as_str())
}

/// The instruction added to the Stop hook output.
pub fn notice_text(tokens: u64, limit: u64, blockers: &[String], exe: &str) -> String {
    let head = format!(
        "[jg-team-dev] Automatic reset: your context is {tokens} tokens, past the reset limit of {limit}."
    );
    let how = format!(
        "write a short summary into `.team/tasks/task-<YYYYMMDD-HHMM>-<slug>.md` (goal, decisions, Board state, \
         open decisions, next queued work), tell the user in one line, run `{exe} reset --summary <that file>` \
         and end your turn."
    );
    if blockers.is_empty() {
        format!(
            "{head} This is a safe point on the Board. Unless a question to the user or a git step is still open in \
             this chat (finish it first), {how}"
        )
    } else {
        format!(
            "{head} Reset at the next safe point, never mid-card or mid-git-step. First finish: {}. Do not start a new \
             card meanwhile. Then {how}",
            blockers.join("; ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jgt_core::{TaskStatus, Waiting, WaitingKind};

    fn assistant(input: u64, created: u64, read: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "usage": {
                "input_tokens": input,
                "cache_creation_input_tokens": created,
                "cache_read_input_tokens": read,
                "output_tokens": 999,
            }},
        })
        .to_string()
    }

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("jgt-auto-reset-{name}-{}", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn the_last_assistant_usage_is_the_context_size() {
        let jsonl = [
            assistant(10, 20, 30),
            r#"{"type":"user","message":{"content":"hi"}}"#.into(),
            assistant(1, 2_000, 80_000),
            r#"{"type":"system","subtype":"stop_hook_summary"}"#.into(),
        ]
        .join("\n");
        assert_eq!(last_usage(&jsonl), Some(82_001));
    }

    #[test]
    fn malformed_lines_sidechains_and_zero_usage_are_skipped() {
        let sidechain = r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":5}}}"#;
        let synthetic = r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{"input_tokens":0}}}"#;
        let jsonl = [assistant(100, 0, 0).as_str(), "{not json", sidechain, synthetic, r#"{"type":"assistant"}"#, ""]
            .join("\n");
        assert_eq!(last_usage(&jsonl), Some(100));
        assert_eq!(last_usage("garbage\n{\"type\":\"user\"}\n"), None);
        assert_eq!(last_usage(""), None);
    }

    #[test]
    fn a_missing_transcript_measures_nothing() {
        assert_eq!(context_tokens(Path::new("/nonexistent/jgt-transcript.jsonl")), None);
    }

    #[test]
    fn only_the_tail_of_a_large_transcript_is_read() {
        // An old, larger usage beyond the tail, a cut line, then the last turn.
        let filler = format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{}\"}}}}", "x".repeat(TAIL_BYTES as usize));
        let content = [assistant(500_000, 0, 0), filler, assistant(1_000, 0, 90_000)].join("\n");
        let path = temp_file("tail", &content);
        assert_eq!(context_tokens(&path), Some(91_000));
        // A tail that holds no assistant entry measures nothing.
        let content = [assistant(500_000, 0, 0), format!("{{\"type\":\"user\",\"x\":\"{}\"}}", "y".repeat(TAIL_BYTES as usize))]
            .join("\n");
        std::fs::write(&path, content).unwrap();
        assert_eq!(context_tokens(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_limit_check() {
        assert_eq!(over_limit(Some(79_999), Some(80_000)), None, "under");
        assert_eq!(over_limit(Some(80_000), Some(80_000)), Some(80_000), "at");
        assert_eq!(over_limit(Some(95_000), Some(80_000)), Some(95_000), "over");
        assert_eq!(over_limit(Some(95_000), None), None, "disabled");
        assert_eq!(over_limit(Some(95_000), Some(0)), None, "disabled with 0");
        assert_eq!(over_limit(None, Some(80_000)), None, "unmeasured");
        // End to end from a malformed transcript.
        let path = temp_file("malformed", "{{{\nnot json\n");
        assert_eq!(over_limit(context_tokens(&path), Some(1)), None);
        let _ = std::fs::remove_file(&path);
    }

    fn session(id: &str, parent: Option<&str>, status: &str) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": id, "harness": "claude-code", "external_id": format!("ext-{id}"), "status": status,
            "listen": false, "mirror": false, "archived": false, "awaiting_reply": false,
            "created_at": 0, "updated_at": 0, "last_seen_at": 0, "title": format!("coder {id}"),
            "role": if parent.is_some() { "coder" } else { "coordinator" }, "parent_id": parent,
        }))
        .unwrap()
    }

    #[test]
    fn a_quiet_board_is_a_safe_point() {
        let tasks = [
            Task::new("/w", 1, "a", TaskStatus::Backlog),
            Task::new("/w", 2, "b", TaskStatus::OnQueue),
            Task::new("/w", 3, "c", TaskStatus::Done),
        ];
        let sessions = [
            session("c1", None, "idle"),
            session("w1", Some("c1"), "disconnected"),
            // Another Coordinator's worker does not count.
            session("w2", Some("other"), "active"),
        ];
        assert!(safe_point_blockers("c1", &tasks, &sessions).is_empty());
        let mut archived = Task::new("/w", 4, "d", TaskStatus::Working);
        archived.archived = true;
        assert!(safe_point_blockers("c1", &[archived], &[]).is_empty());
    }

    #[test]
    fn each_blocker_prevents_the_reset() {
        for status in [TaskStatus::Working, TaskStatus::Testing, TaskStatus::Review] {
            let b = safe_point_blockers("c1", &[Task::new("/w", 7, "t", status)], &[]);
            assert_eq!(b, [format!("JGT-7 is in {}", status.label())], "mid-card");
        }
        for status in TaskStatus::GIT_STAGES {
            let b = safe_point_blockers("c1", &[Task::new("/w", 8, "t", status)], &[]);
            assert_eq!(b, [format!("JGT-8 is in {}", status.label())], "git step");
        }
        let mut waiting = Task::new("/w", 9, "t", TaskStatus::OnQueue);
        waiting.waiting = Some(Waiting {
            kind: WaitingKind::Question,
            reason: "pick a design".into(),
            question_id: Some("q1".into()),
            since: 0,
            until: None,
        });
        assert_eq!(safe_point_blockers("c1", &[waiting], &[]), ["JGT-9 waits for the user"], "open decision");
        for status in ["active", "idle", "listening"] {
            let b = safe_point_blockers("c1", &[], &[session("w1", Some("c1"), status)]);
            assert_eq!(b, ["worker coder w1 is running"], "worker {status}");
        }
    }

    #[test]
    fn the_notice_is_given_at_most_twice() {
        assert!(should_notify(None, Notice::Blocked));
        assert!(should_notify(None, Notice::Safe));
        assert!(!should_notify(Some(Notice::Blocked), Notice::Blocked));
        assert!(should_notify(Some(Notice::Blocked), Notice::Safe));
        assert!(!should_notify(Some(Notice::Safe), Notice::Safe));
        assert!(!should_notify(Some(Notice::Safe), Notice::Blocked));
    }

    #[test]
    fn the_marker_round_trips() {
        let path = std::env::temp_dir().join(format!("jgt-auto-reset-marker-{}", std::process::id())).join("s1");
        assert_eq!(read_marker(&path), None);
        write_marker(&path, Notice::Blocked).unwrap();
        assert_eq!(read_marker(&path), Some(Notice::Blocked));
        write_marker(&path, Notice::Safe).unwrap();
        assert_eq!(read_marker(&path), Some(Notice::Safe));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_notice_says_what_to_finish_first() {
        let now = notice_text(90_000, 80_000, &[], "/usr/bin/jg-team-dev");
        assert!(now.contains("90000 tokens") && now.contains("safe point"));
        assert!(now.contains("`/usr/bin/jg-team-dev reset --summary <that file>`") && now.contains(".team/tasks/"));
        let later = notice_text(90_000, 80_000, &["JGT-3 is in Review".into()], "jg-team-dev");
        assert!(later.contains("First finish: JGT-3 is in Review.") && later.contains("never mid-card"));
    }
}
