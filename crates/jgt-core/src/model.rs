//! Domain model shared by the hub, the CLI and the desktop app.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok($name::$variant),)+
                    other => Err(format!("unknown {}: {other}", stringify!($name))),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum! {
    /// The program an agent session runs in.
    Harness {
        ClaudeCode => "claude-code",
        Opencode => "opencode",
        /// Google Antigravity (`agy`); team workers only.
        Antigravity => "antigravity",
        Custom => "custom",
    }
}

impl Harness {
    pub fn label(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "Claude Code",
            Harness::Opencode => "opencode",
            Harness::Antigravity => "Antigravity",
            Harness::Custom => "Custom",
        }
    }

    /// Whether the hub knows how to start this harness again for a stored session.
    pub fn can_reconnect(self) -> bool {
        matches!(self, Harness::ClaudeCode | Harness::Opencode)
    }
}

string_enum! {
    /// Where a session is in its lifecycle. See PLAN.md §5.
    SessionStatus {
        Active => "active",
        Idle => "idle",
        Listening => "listening",
        Disconnected => "disconnected",
        Ended => "ended",
    }
}

impl SessionStatus {
    /// The harness process is alive and attached to the hub.
    pub fn is_connected(self) -> bool {
        matches!(self, Self::Active | Self::Idle | Self::Listening)
    }

    /// Messages can still be queued for this session.
    pub fn accepts_messages(self) -> bool {
        self != Self::Ended
    }
}

string_enum! {
    Direction {
        ToAgent => "to_agent",
        FromAgent => "from_agent",
        System => "system",
    }
}

string_enum! {
    MessageKind {
        /// Plain chat text.
        Text => "text",
        /// The agent's final answer for a turn, mirrored automatically.
        Reply => "reply",
        /// A highlighted notification.
        Notify => "notify",
        /// A question waiting for an answer.
        Question => "question",
        /// A permission request waiting for Allow/Deny.
        Permission => "permission",
        /// A lifecycle event (connected, disconnected, …).
        Event => "event",
    }
}

string_enum! {
    Delivery {
        Queued => "queued",
        Delivered => "delivered",
        Read => "read",
        Undeliverable => "undeliverable",
    }
}

string_enum! {
    QuestionStatus {
        Pending => "pending",
        Answered => "answered",
        Expired => "expired",
    }
}

string_enum! {
    NotifyLevel {
        Info => "info",
        Success => "success",
        Warning => "warning",
        Error => "error",
    }
}

string_enum! {
    ReconnectMode {
        Background => "background",
        Terminal => "terminal",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub harness: Harness,
    pub external_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    pub pid: Option<i64>,
    pub pid_start: Option<i64>,
    pub status: SessionStatus,
    pub status_text: Option<String>,
    /// Keep the agent parked waiting for messages when its turn ends.
    pub listen: bool,
    /// Mirror every final reply into the chat, not only replies to app messages.
    pub mirror: bool,
    pub archived: bool,
    /// A message from the app was delivered and the next final reply should be mirrored.
    pub awaiting_reply: bool,
    pub ended_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seen_at: i64,
    /// Agent messages the user has not read yet.
    #[serde(default)]
    pub unread: i64,
    /// User messages waiting for delivery.
    #[serde(default)]
    pub queued: i64,
    #[serde(default)]
    pub last_message: Option<String>,
    #[serde(default)]
    pub last_message_at: Option<i64>,
    /// The team role a session was started with (`coordinator`, or a worker skill), if any.
    #[serde(default)]
    pub role: Option<String>,
    /// For a worker: the hub id of the Coordinator that started it.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// For a finished worker: the cost of its run in USD, when Claude Code reported it.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// For a finished worker: how long its run took.
    #[serde(default)]
    pub run_secs: Option<i64>,
    /// For a worker: the model and effort it runs with (PRD H7), shown with its harness.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// For a finished worker: the tokens its harness reported (PRD H8).
    #[serde(default)]
    pub tokens_in: Option<i64>,
    #[serde(default)]
    pub tokens_out: Option<i64>,
}

/// Role of the session that talks to the user and runs the team.
pub const COORDINATOR: &str = "coordinator";

/// `ended_reason` prefix of a Coordinator replaced by `jg-team-dev reset`. Its process may still
/// run for a moment; hooks must not register it again.
pub const RETIRED: &str = "retired";

impl Session {
    pub fn is_coordinator(&self) -> bool {
        self.role.as_deref() == Some(COORDINATOR)
    }

    /// A clear error message when `!self.is_coordinator()`, distinguishing sessions with no role
    /// from sessions that are workers (PRD R4).
    pub fn not_coordinator_error(&self) -> String {
        match self.role.as_deref().filter(|r| !r.is_empty()) {
            Some(role) => format!("session {} is a {role} worker, not a Coordinator.", self.display_name()),
            None => "this Claude Code session has no jg-team-dev role (JGT_ROLE was not set when it started). Run jg-team-dev claim-coordinator, or restart it with jg-team-dev start.".to_string(),
        }
    }

    /// Verifies that this session is a Coordinator, returning an error message if not.
    pub fn require_coordinator(&self) -> Result<(), String> {
        if self.is_coordinator() {
            Ok(())
        } else {
            Err(self.not_coordinator_error())
        }
    }

    /// A Coordinator that handed over to a fresh one with `jg-team-dev reset`.
    pub fn is_retired(&self) -> bool {
        self.status == SessionStatus::Ended && self.ended_reason.as_deref().is_some_and(|r| r.starts_with(RETIRED))
    }

    /// A session started by a Coordinator with one skill.
    pub fn is_worker(&self) -> bool {
        self.role.as_deref().is_some_and(|r| !r.is_empty() && r != COORDINATOR)
    }

    /// A human-friendly name: the title, else the project folder, else the id.
    pub fn display_name(&self) -> String {
        if let Some(title) = self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            return title.trim().to_string();
        }
        if let Some(project) = self.project() {
            return project;
        }
        format!("{} {}", self.harness.label(), short_id(&self.external_id))
    }

    /// The last path component of the working directory.
    pub fn project(&self) -> Option<String> {
        let cwd = self.cwd.as_deref()?;
        std::path::Path::new(cwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| Some(cwd.to_string()))
    }
}

pub fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub session_id: String,
    pub direction: Direction,
    pub kind: MessageKind,
    pub body: String,
    #[serde(default)]
    pub meta: serde_json::Value,
    pub delivery: Delivery,
    pub created_at: i64,
    pub delivered_at: Option<i64>,
    pub read_at: Option<i64>,
    /// The question attached to `question`/`permission` messages.
    #[serde(default)]
    pub question: Option<Question>,
}

impl Message {
    pub fn meta_str(&self, key: &str) -> Option<&str> {
        self.meta.get(key).and_then(|v| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub session_id: String,
    pub message_id: i64,
    pub kind: MessageKind,
    pub prompt: String,
    pub options: Vec<String>,
    pub answer: Option<String>,
    pub status: QuestionStatus,
    pub created_at: i64,
    pub answered_at: Option<i64>,
    pub expires_at: Option<i64>,
}

/// Streamed by `GET /v1/events`, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Sent once when the stream opens.
    Hello { version: String },
    Session { session: Session },
    SessionRemoved { id: String },
    Message { message: Message },
    Question { question: Question },
    /// Ask a running UI to come to the front (e.g. notification clicked).
    Focus { session_id: Option<String> },
    /// A task was created or changed.
    Task { task: Task },
    TaskRemoved { id: String, workspace: String },
    /// A new row in a task's activity log.
    TaskEvent { event: TaskEvent },
}

// ── board ───────────────────────────────────────────────────────────────────

string_enum! {
    /// A Board column. `ALL` is the column order, left to right.
    TaskStatus {
        Backlog => "backlog",
        OnQueue => "on_queue",
        Working => "working",
        Testing => "testing",
        Review => "review",
        ToCommit => "to_commit",
        ToMerge => "to_merge",
        ToPush => "to_push",
        Done => "done",
    }
}

impl TaskStatus {
    /// Column header text.
    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Backlog => "Backlog",
            TaskStatus::OnQueue => "On Queue",
            TaskStatus::Working => "Working",
            TaskStatus::Testing => "Testing",
            TaskStatus::Review => "Review",
            TaskStatus::ToCommit => "To commit",
            TaskStatus::ToMerge => "To merge",
            TaskStatus::ToPush => "To push",
            TaskStatus::Done => "Done",
        }
    }

    /// Position in the column order (Backlog = 0).
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    /// The column to the right, if any.
    pub fn next(self) -> Option<TaskStatus> {
        Self::ALL.get(self.index() + 1).copied()
    }

    /// The column to the left, if any.
    pub fn prev(self) -> Option<TaskStatus> {
        self.index().checked_sub(1).map(|i| Self::ALL[i])
    }

    /// The team is working on a task in this column (counts toward the WIP limit).
    pub fn in_progress(self) -> bool {
        matches!(self, TaskStatus::Working | TaskStatus::Testing | TaskStatus::Review)
    }

    /// True for the three git columns: To commit, To merge, To push.
    pub fn is_git_stage(self) -> bool {
        matches!(self, TaskStatus::ToCommit | TaskStatus::ToMerge | TaskStatus::ToPush)
    }

    /// The git step the column waits for: "commit", "merge" or "push".
    pub fn git_step(self) -> Option<&'static str> {
        match self {
            TaskStatus::ToCommit => Some("commit"),
            TaskStatus::ToMerge => Some("merge"),
            TaskStatus::ToPush => Some("push"),
            _ => None,
        }
    }

    /// A git column or Done.
    pub fn is_past_review(self) -> bool {
        self.is_git_stage() || self == TaskStatus::Done
    }
}

string_enum! {
    TaskPriority {
        Low => "low",
        Normal => "normal",
        High => "high",
    }
}

string_enum! {
    /// Why a task waits for the user.
    WaitingKind {
        /// A decision is open: a `chat_ask` question.
        Question => "question",
        /// Legacy: the old 3-round limit.
        RoundLimit => "round_limit",
        /// A worker on the task stopped at the harness's usage limit; `until` is the reset. Set and
        /// cleared by the hub, which then wakes the Coordinator to resume (PRD E7).
        UsageLimit => "usage_limit",
    }
}

string_enum! {
    /// What a row of a task's activity log records.
    TaskEventKind {
        Created => "created",
        Edited => "edited",
        Moved => "moved",
        Details => "details",
        Comment => "comment",
        Question => "question",
        Answer => "answer",
        TestResult => "test_result",
        ReviewResult => "review_result",
        Waiting => "waiting",
        Decision => "decision",
        Link => "link",
        WorkerStarted => "worker_started",
        WorkerFinished => "worker_finished",
        FollowUp => "follow_up",
        Archived => "archived",
        Note => "note",
    }
}

string_enum! {
    /// How bad a finding is (PRD E3): critical and high ones start the fix round; the others go
    /// to the follow-up task.
    Severity {
        Critical => "critical",
        High => "high",
        Other => "other",
    }
}

impl Severity {
    /// A reviewer's own label as one of the three: `blocking` and `high` are high, `medium`,
    /// `low` and anything else is other.
    pub fn from_label(label: &str) -> Severity {
        match label.trim().to_ascii_lowercase().as_str() {
            "critical" => Severity::Critical,
            "high" | "blocking" => Severity::High,
            _ => Severity::Other,
        }
    }
}

string_enum! {
    /// The kinds of critical finding (PRD "Critical findings"). Critical findings must carry one
    /// of these categories (K17).
    FindingCategory {
        Security => "security",
        Secret => "secret",
        Performance => "performance",
        Patterns => "patterns",
        Docs => "docs",
        Behavior => "behavior",
    }
}

string_enum! {
    TestStatus {
        Pass => "pass",
        Fail => "fail",
    }
}

string_enum! {
    ReviewDecision {
        Approved => "approved",
        /// Changes requested.
        Changes => "changes",
    }
}

/// Verifications a task gets before moving to Done: the first one, and the one after
/// the single automatic fix round.
pub const ROUND_LIMIT: i64 = 2;

/// Prefix of every task key (`JGT-12`).
pub const TASK_KEY_PREFIX: &str = "JGT-";

/// The short key of task number `n` in its workspace: `JGT-<n>`.
pub fn task_key(number: i64) -> String {
    format!("{TASK_KEY_PREFIX}{number}")
}

/// The number in a task key, case-insensitive: `jgt-12` → 12.
pub fn parse_task_key(key: &str) -> Option<i64> {
    let key = key.trim();
    let prefix = key.get(..TASK_KEY_PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(TASK_KEY_PREFIX) {
        return None;
    }
    key[TASK_KEY_PREFIX.len()..].parse().ok().filter(|n| *n > 0)
}

/// A card on a workspace's Board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// The Coordinator's folder (its `cwd`, without a trailing slash).
    pub workspace: String,
    /// Counted per workspace and never reused.
    pub number: i64,
    /// `JGT-<number>`.
    pub key: String,
    pub title: String,
    /// The user's text; agents never edit it.
    pub description: String,
    /// Plan, files and acceptance criteria written by the Coordinator (markdown).
    #[serde(default)]
    pub details: Option<String>,
    pub status: TaskStatus,
    /// Order inside its column, from 0.
    pub position: i64,
    pub priority: TaskPriority,
    #[serde(default)]
    pub waiting: Option<Waiting>,
    /// Fix rounds so far: Review → Working and Testing → Working returns.
    pub review_rounds: i64,
    /// Verifications allowed before moving to Done (2).
    pub round_limit: i64,
    /// The test of the current pass (cleared when the task enters Testing).
    #[serde(default)]
    pub test_result: Option<TestResult>,
    /// The review of the current pass (cleared when the task enters Review).
    #[serde(default)]
    pub review_result: Option<ReviewResult>,
    /// `.team/runs/<run-id>/`.
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    /// The task this one is a subtask or a review follow-up of.
    #[serde(default)]
    pub parent_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub done_at: Option<i64>,
    #[serde(default)]
    pub archived: bool,
    /// The worker on the card right now: `meta.worker` of the newest `worker_started` event
    /// without a matching `worker_finished`.
    #[serde(default)]
    pub current_worker: Option<String>,
}

impl Task {
    /// The verification cycle the task is in, `(round, limit)`: `review_rounds` counts fix rounds,
    /// so the current cycle is one more (the round the next or latest review gets). After the
    /// second verification it reads 2/2. The card, `task list` and `task show` all show this.
    pub fn round(&self) -> (i64, i64) {
        (self.review_rounds + 1, self.round_limit)
    }

    /// Whether the round is worth showing: the team has the task, or a review already sent it back.
    pub fn shows_round(&self) -> bool {
        self.review_rounds > 0 || self.status.in_progress()
    }

    /// Whether the team may still start a fix round: the current verification is not
    /// the last one before Done.
    pub fn fix_round_left(&self) -> bool {
        self.review_rounds + 1 < self.round_limit
    }

    /// A task as the hub creates it, with every optional field empty (for tests).
    pub fn new(workspace: &str, number: i64, title: &str, status: TaskStatus) -> Task {
        Task {
            id: format!("t{number}"),
            workspace: workspace.into(),
            number,
            key: task_key(number),
            title: title.into(),
            description: String::new(),
            details: None,
            status,
            position: 0,
            priority: TaskPriority::Normal,
            waiting: None,
            review_rounds: 0,
            round_limit: ROUND_LIMIT,
            test_result: None,
            review_result: None,
            run_id: None,
            worktree: None,
            parent_id: None,
            created_at: 0,
            updated_at: 0,
            done_at: None,
            archived: false,
            current_worker: None,
        }
    }
}

/// The *Waiting for you* flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Waiting {
    pub kind: WaitingKind,
    pub reason: String,
    /// The `chat_ask` question the task waits on.
    #[serde(default)]
    pub question_id: Option<String>,
    pub since: i64,
    /// For `usage_limit`: when the limit resets (ms since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub status: TestStatus,
    /// Hub id of the coder session.
    pub worker: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub failures: Vec<String>,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewResult {
    pub decision: ReviewDecision,
    /// Which review this is (1 = the first), set by the hub.
    pub round: i64,
    /// Hub id of the reviewer session.
    pub worker: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
    pub at: i64,
}

impl ReviewResult {
    pub fn has_critical(&self) -> bool {
        self.findings.iter().any(|f| f.critical)
    }

    /// Whether a finding is critical or high: only those start a fix round (PRD E4).
    pub fn has_blocking(&self) -> bool {
        self.findings.iter().any(|f| f.level() != Severity::Other)
    }
}

/// One review finding. Critical findings carry the category from the PRD list (K17).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub critical: bool,
    #[serde(default)]
    pub category: Option<FindingCategory>,
    /// `critical`, `high` or `other` (PRD E3). Older results may hold a reviewer's own label
    /// (`blocking`, `medium`, …); `level` reads both.
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub location: String,
    pub problem: String,
    #[serde(default)]
    pub required_action: String,
}

impl Finding {
    pub fn level(&self) -> Severity {
        if self.critical { Severity::Critical } else { Severity::from_label(self.severity.as_deref().unwrap_or("")) }
    }
}

/// The priority of a follow-up task filed from review findings.
///
/// Rule: the highest severity wins (Critical/High -> High; else any "medium" or unlabelled other -> Normal;
/// only "low" -> Low; empty -> Normal).
pub fn follow_up_priority(findings: &[Finding]) -> TaskPriority {
    if findings.is_empty() {
        return TaskPriority::Normal;
    }
    if findings.iter().any(|f| matches!(f.level(), Severity::Critical | Severity::High)) {
        return TaskPriority::High;
    }
    let all_low = findings
        .iter()
        .all(|f| f.severity.as_deref().map(|s| s.trim().eq_ignore_ascii_case("low")).unwrap_or(false));
    if all_low {
        TaskPriority::Low
    } else {
        TaskPriority::Normal
    }
}

/// Who did something on the Board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    User,
    Coordinator { session: String },
    Worker { session: String },
    /// The hub itself (follow-up tasks, the round-limit flag).
    Hub,
}

impl Actor {
    /// How the activity log names the actor.
    pub fn label(&self) -> String {
        match self {
            Actor::User => "you".into(),
            Actor::Coordinator { .. } => "coordinator".into(),
            Actor::Worker { session } => format!("worker {}", short_id(session)),
            Actor::Hub => "hub".into(),
        }
    }
}

/// A row of a task's activity log (K9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub id: i64,
    pub task_id: String,
    pub kind: TaskEventKind,
    pub actor: Actor,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub meta: serde_json::Value,
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_keys_round_trip() {
        assert_eq!(task_key(12), "JGT-12");
        assert_eq!(parse_task_key("jgt-12"), Some(12));
        assert_eq!(parse_task_key(" JGT-3 "), Some(3));
        assert_eq!(parse_task_key("JGT-0"), None);
        assert_eq!(parse_task_key("ABC-1"), None);
        assert_eq!(parse_task_key("01J"), None);
    }

    #[test]
    fn columns_are_in_board_order() {
        assert_eq!(TaskStatus::Backlog.prev(), None);
        assert_eq!(TaskStatus::Backlog.next(), Some(TaskStatus::OnQueue));
        assert_eq!(TaskStatus::Done.next(), None);
        assert_eq!(TaskStatus::Review.prev(), Some(TaskStatus::Testing));
        assert!(TaskStatus::Testing.in_progress() && !TaskStatus::OnQueue.in_progress());
        assert_eq!(TaskStatus::Review.next(), Some(TaskStatus::ToCommit));
        assert_eq!(TaskStatus::ToCommit.next(), Some(TaskStatus::ToMerge));
        assert_eq!(TaskStatus::ToMerge.next(), Some(TaskStatus::ToPush));
        assert_eq!(TaskStatus::ToPush.next(), Some(TaskStatus::Done));
        assert_eq!(TaskStatus::Done.prev(), Some(TaskStatus::ToPush));
        assert!(!TaskStatus::Done.in_progress());
        assert!(!TaskStatus::ToCommit.in_progress());
        assert!(TaskStatus::ToCommit.is_git_stage());
        assert!(TaskStatus::ToMerge.is_git_stage());
        assert!(TaskStatus::ToPush.is_git_stage());
        assert!(!TaskStatus::Review.is_git_stage());
        assert!(!TaskStatus::Done.is_git_stage());
        assert_eq!(TaskStatus::ToCommit.git_step(), Some("commit"));
        assert_eq!(TaskStatus::ToMerge.git_step(), Some("merge"));
        assert_eq!(TaskStatus::ToPush.git_step(), Some("push"));
        assert_eq!(TaskStatus::Review.git_step(), None);
        assert!(TaskStatus::ToCommit.is_past_review());
        assert!(TaskStatus::ToMerge.is_past_review());
        assert!(TaskStatus::ToPush.is_past_review());
        assert!(TaskStatus::Done.is_past_review());
        assert!(!TaskStatus::Review.is_past_review());
        assert_eq!(TaskStatus::ALL.len(), 9);
    }

    #[test]
    fn follow_up_priority_highest_severity_wins() {
        let f = |critical: bool, severity: Option<&str>| Finding {
            critical,
            category: None,
            severity: severity.map(str::to_string),
            location: String::new(),
            problem: "p".into(),
            required_action: String::new(),
        };

        // Empty -> Normal
        assert_eq!(follow_up_priority(&[]), TaskPriority::Normal);

        // Only low -> Low
        assert_eq!(follow_up_priority(&[f(false, Some("low"))]), TaskPriority::Low);
        assert_eq!(
            follow_up_priority(&[f(false, Some("low")), f(false, Some("LOW")), f(false, Some(" low "))]),
            TaskPriority::Low
        );

        // Any medium or unlabelled other -> Normal
        assert_eq!(follow_up_priority(&[f(false, Some("medium"))]), TaskPriority::Normal);
        assert_eq!(follow_up_priority(&[f(false, None)]), TaskPriority::Normal);
        assert_eq!(follow_up_priority(&[f(false, Some(""))]), TaskPriority::Normal);
        assert_eq!(follow_up_priority(&[f(false, Some("nit"))]), TaskPriority::Normal);
        assert_eq!(
            follow_up_priority(&[f(false, Some("medium")), f(false, Some("low")), f(false, Some("low"))]),
            TaskPriority::Normal
        );
        assert_eq!(
            follow_up_priority(&[f(false, None), f(false, Some("low"))]),
            TaskPriority::Normal
        );

        // Critical or High -> High
        assert_eq!(follow_up_priority(&[f(true, None)]), TaskPriority::High);
        assert_eq!(follow_up_priority(&[f(true, Some("low"))]), TaskPriority::High);
        assert_eq!(follow_up_priority(&[f(false, Some("high"))]), TaskPriority::High);
        assert_eq!(follow_up_priority(&[f(false, Some("blocking"))]), TaskPriority::High);
        assert_eq!(
            follow_up_priority(&[f(false, Some("high")), f(false, Some("medium")), f(false, Some("low"))]),
            TaskPriority::High
        );
        assert_eq!(
            follow_up_priority(&[f(true, None), f(false, Some("low"))]),
            TaskPriority::High
        );
    }

    #[test]
    fn severities_read_reviewer_labels() {
        assert_eq!(Severity::from_label("Blocking"), Severity::High);
        assert_eq!(Severity::from_label("medium"), Severity::Other);
        let f = |critical, severity: &str| Finding {
            critical,
            category: None,
            severity: Some(severity.into()),
            location: String::new(),
            problem: "p".into(),
            required_action: String::new(),
        };
        assert_eq!(f(true, "other").level(), Severity::Critical);
        assert_eq!(f(false, "high").level(), Severity::High);
        let review = |findings| ReviewResult { decision: ReviewDecision::Approved, round: 1, worker: "w".into(), findings, at: 0 };
        assert!(!review(vec![f(false, "low")]).has_blocking());
        assert!(review(vec![f(false, "low"), f(false, "high")]).has_blocking());
    }

    #[test]
    fn actors_serialize_with_a_kind_tag() {
        let a = Actor::Coordinator { session: "s1".into() };
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"kind":"coordinator","session":"s1"}"#);
        assert_eq!(serde_json::to_string(&Actor::User).unwrap(), r#"{"kind":"user"}"#);
    }

    #[test]
    fn not_coordinator_error_distinguishes_no_role_and_worker_role() {
        let mut s: Session = serde_json::from_value(serde_json::json!({
            "id": "1",
            "harness": "claude-code",
            "external_id": "ext1",
            "title": "my-worker",
            "status": "active",
            "role": null,
            "listen": false,
            "mirror": false,
            "archived": false,
            "awaiting_reply": false,
            "created_at": 0,
            "updated_at": 0,
            "last_seen_at": 0,
        }))
        .unwrap();

        assert_eq!(
            s.not_coordinator_error(),
            "this Claude Code session has no jg-team-dev role (JGT_ROLE was not set when it started). Run jg-team-dev claim-coordinator, or restart it with jg-team-dev start."
        );
        assert!(!s.is_worker());
        assert_eq!(s.require_coordinator(), Err(s.not_coordinator_error()));

        s.role = Some("coder".into());
        assert_eq!(
            s.not_coordinator_error(),
            "session my-worker is a coder worker, not a Coordinator."
        );
        assert!(s.is_worker());
        assert_eq!(s.require_coordinator(), Err(s.not_coordinator_error()));

        s.role = Some(COORDINATOR.into());
        assert!(s.is_coordinator());
        assert!(!s.is_worker());
        assert_eq!(s.require_coordinator(), Ok(()));
    }
}
