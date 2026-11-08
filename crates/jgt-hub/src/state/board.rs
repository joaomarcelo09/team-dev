//! The Board: tasks, their activity log, and the gates the team cannot skip (K6, K8, K10, K12,
//! K17, and the team-efficiency flow D3/D4: one automatic fix round, then Orchestrator review). Every rule lives here, in the hub, so the app, the CLI, the MCP tools and a raw HTTP
//! client all get the same answers. The user may move any task anywhere (K7); a move that skips a
//! gate is allowed and logged with the gate it skipped.

use super::{Hub, HubError, HubResult};
use crate::db::{Board, NewTask};
use jgt_core::{
    Actor, Decision, Finding, MORE_ROUNDS, QuestionStatus, ROUND_LIMIT, ReviewDecision, ReviewResult, Session,
    Severity, Task, TaskEvent, TaskEventKind, TaskPriority, TaskStatus, TestResult, TestStatus, WaitingKind, api::*,
    now_ms, parse_task_key,
};
use serde_json::json;
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// The workspace key the hub stores: the folder without a trailing slash (`/` stays `/`).
pub fn normalize_workspace(workspace: &str) -> String {
    let trimmed = workspace.trim();
    match trimmed.trim_end_matches('/') {
        "" if trimmed.starts_with('/') => "/".into(),
        rest => rest.into(),
    }
}

/// Logged on a task when a queue message or a comment has no Coordinator to go to.
pub const NO_COORDINATOR: &str = "No Coordinator is running for this folder";

/// Done tasks are archived (kept, hidden) this long after they got there (K15).
pub const ARCHIVE_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// How long after a usage limit resets the hub wakes the Coordinator to resume (PRD E7).
pub const RESUME_AFTER_MS: i64 = 2 * 60 * 1000;

/// `meta.usage_limit` of a `worker_finished` event: (resets, until, resume brief path).
fn usage_limit(meta: &serde_json::Value) -> Option<(String, Option<i64>, Option<String>)> {
    let limit = meta.get("usage_limit").filter(|l| l.is_object())?;
    let resets = limit.get("resets").and_then(|r| r.as_str()).unwrap_or("later").to_string();
    let until = limit.get("until").and_then(|u| u.as_i64());
    let resume = limit.get("resume_brief").and_then(|p| p.as_str()).map(str::to_owned);
    Some((resets, until, resume))
}

/// Moves that are the user's to make (Backlog and On Queue are theirs, reopening a Done task,
/// and the decision on a task in Orchestrator review): never logged as skipping a gate.
fn users_own_move(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Backlog, OnQueue) | (OnQueue, Backlog) | (Done, OnQueue) | (OrchestratorReview, Done | Working | Backlog)
    )
}

/// Roles whose results the hub records: coder records tests, reviewer records reviews.
const TEST_ROLES: &[&str] = &["coder"];
const REVIEW_ROLES: &[&str] = &["reviewer"];

/// What a Board write changed, published once its transaction committed.
#[derive(Default)]
struct Changes {
    tasks: Vec<String>,
    events: Vec<TaskEvent>,
}

impl Changes {
    fn task(&mut self, id: &str) {
        if !self.tasks.iter().any(|t| t == id) {
            self.tasks.push(id.to_string());
        }
    }

    /// Writes a row of the task's activity log.
    fn log(
        &mut self,
        b: &Board,
        task_id: &str,
        kind: TaskEventKind,
        actor: &Actor,
        body: &str,
        meta: serde_json::Value,
    ) -> rusqlite::Result<()> {
        self.task(task_id);
        self.events.push(b.insert_event(task_id, kind, actor, body, &meta)?);
        Ok(())
    }
}

/// Renders review findings as markdown, for a follow-up task's description.
fn render_findings(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| {
            let mut line = String::from("- ");
            if !f.location.is_empty() {
                line.push_str(&format!("`{}`: ", f.location));
            }
            line.push_str(f.problem.trim());
            if !f.required_action.trim().is_empty() {
                line.push_str(&format!(" — {}", f.required_action.trim()));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_findings(input: &[FindingInput]) -> HubResult<Vec<Finding>> {
    input
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let n = i + 1;
            let label = f.severity.as_deref().map(Severity::from_label);
            // `severity: critical` is enough; otherwise mark `critical` directly.
            let critical = f.critical.or(label.map(|l| l == Severity::Critical)).ok_or_else(|| {
                HubError::BadRequest(format!("finding {n}: mark it critical or non-critical (K17)"))
            })?;
            let severity = if critical { Severity::Critical } else { label.unwrap_or(Severity::Other) };
            let severity = if severity == Severity::Critical && !critical { Severity::High } else { severity };
            if critical && f.category.is_none() {
                return Err(HubError::BadRequest(format!(
                    "finding {n} is critical: give its category ({})",
                    jgt_core::FindingCategory::ALL.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ")
                )));
            }
            if f.problem.trim().is_empty() {
                return Err(HubError::BadRequest(format!("finding {n}: describe the problem")));
            }
            Ok(Finding {
                critical,
                category: f.category,
                severity: Some(severity.as_str().to_string()),
                location: f.location.clone(),
                problem: f.problem.clone(),
                required_action: f.required_action.clone(),
            })
        })
        .collect()
}

impl Hub {
    fn publish(&self, changes: Changes) {
        self.bump();
        for id in &changes.tasks {
            if let Ok(Some(task)) = self.db.board::<_, rusqlite::Error>(|b| b.task(id)) {
                self.emit(jgt_core::Event::Task { task });
            }
        }
        for event in changes.events {
            self.emit(jgt_core::Event::TaskEvent { event });
        }
    }

    /// Who sends a Board request. A session is the workspace's Coordinator, or a worker; any other
    /// Coordinator (another folder, retired) is refused.
    pub fn actor(&self, req: &ActorRequest, workspace: &str) -> HubResult<Actor> {
        let id = match req {
            ActorRequest::User => return Ok(Actor::User),
            ActorRequest::Session { session } => session,
        };
        let session =
            self.db.session(id)?.ok_or_else(|| HubError::Forbidden(format!("unknown session {id}")))?;
        if session.status == jgt_core::SessionStatus::Ended {
            return Err(HubError::Forbidden(format!("session {} has ended", session.display_name())));
        }
        if session.is_worker() {
            return Ok(Actor::Worker { session: session.id });
        }
        if !session.is_coordinator() {
            return Err(HubError::Forbidden(session.not_coordinator_error()));
        }
        match self.db.current_coordinator(workspace)? {
            Some(current) if current.id == session.id => Ok(Actor::Coordinator { session: session.id }),
            _ => Err(HubError::Forbidden(format!(
                "session {} is not the Coordinator of {workspace}",
                session.display_name()
            ))),
        }
    }

    /// Like `actor`, for requests that change the Board: workers only read it.
    fn writer(&self, req: &ActorRequest, workspace: &str) -> HubResult<Actor> {
        match self.actor(req, workspace)? {
            Actor::Worker { .. } => Err(HubError::Forbidden(
                "workers can read the Board but not change it; report to your Coordinator".into(),
            )),
            actor => Ok(actor),
        }
    }

    /// A task by id, or by key (`JGT-12`) within `workspace`.
    pub fn task(&self, id: &str, workspace: Option<&str>) -> HubResult<Task> {
        let found = match parse_task_key(id) {
            Some(number) => {
                let workspace = workspace
                    .map(normalize_workspace)
                    .ok_or_else(|| HubError::BadRequest(format!("pass the workspace to find {id}")))?;
                self.db.board::<_, rusqlite::Error>(|b| b.task_by_key(&workspace, number))?
            }
            None => self.db.board::<_, rusqlite::Error>(|b| b.task(id))?,
        };
        found.ok_or(HubError::NotFound("task"))
    }

    pub fn tasks(&self, workspace: &str, include_archived: bool) -> HubResult<Vec<Task>> {
        let workspace = normalize_workspace(workspace);
        if workspace.is_empty() {
            return Err(HubError::BadRequest("workspace is required".into()));
        }
        Ok(self.db.board::<_, rusqlite::Error>(|b| b.tasks(&workspace, include_archived))?)
    }

    pub fn task_events(&self, id: &str, workspace: Option<&str>) -> HubResult<Vec<TaskEvent>> {
        let task = self.task(id, workspace)?;
        Ok(self.db.board::<_, rusqlite::Error>(|b| b.events(&task.id))?)
    }

    pub fn create_task(&self, req: &CreateTaskRequest) -> HubResult<Task> {
        let workspace = normalize_workspace(&req.workspace);
        if workspace.is_empty() {
            return Err(HubError::BadRequest("workspace is required".into()));
        }
        let title = req.title.trim();
        if title.is_empty() {
            return Err(HubError::BadRequest("a task needs a title".into()));
        }
        let actor = self.writer(&req.actor, &workspace)?;
        let status = req.status.unwrap_or(TaskStatus::Backlog);
        if !matches!(status, TaskStatus::Backlog | TaskStatus::OnQueue) {
            return Err(HubError::BadRequest("new tasks go to Backlog or On Queue".into()));
        }
        if matches!(actor, Actor::Coordinator { .. }) && status != TaskStatus::Backlog {
            return Err(HubError::Forbidden(
                "the Coordinator creates tasks in Backlog only (K10); queue a subtask with a move".into(),
            ));
        }
        let parent = match req.parent_id.as_deref() {
            Some(p) => {
                let parent = self.task(p, Some(&workspace))?;
                if parent.workspace != workspace {
                    return Err(HubError::BadRequest(format!("{} belongs to another workspace", parent.key)));
                }
                Some(parent)
            }
            None => None,
        };
        let mut changes = Changes::default();
        let task = self.db.board(|b| {
            let task = b.insert_task(&NewTask {
                workspace: &workspace,
                title,
                description: &req.description,
                priority: req.priority.unwrap_or(TaskPriority::Normal),
                status,
                parent_id: parent.as_ref().map(|p| p.id.as_str()),
            })?;
            let meta = json!({ "status": status, "parent_id": task.parent_id });
            changes.log(b, &task.id, TaskEventKind::Created, &actor, "", meta)?;
            Ok::<_, HubError>(task)
        })?;
        self.publish(changes);
        self.task(&task.id, None)
    }

    pub fn patch_task(&self, id: &str, workspace: Option<&str>, req: &PatchTaskRequest) -> HubResult<Task> {
        let task = self.task(id, workspace)?;
        let actor = self.writer(&req.actor, &task.workspace)?;
        let user_fields = req.title.is_some() || req.description.is_some() || req.priority.is_some();
        if user_fields && actor != Actor::User {
            return Err(HubError::Forbidden(
                "the title, description and priority belong to the user; write the plan in details".into(),
            ));
        }
        if !user_fields && req.details.is_none() && req.waiting.is_none() && req.run_id.is_none() && req.worktree.is_none()
        {
            return Err(HubError::BadRequest("nothing to change".into()));
        }
        if req.waiting.as_ref().is_some_and(|w| w.as_ref().is_some_and(|w| w.reason.trim().is_empty())) {
            return Err(HubError::BadRequest("say why the task waits for the user".into()));
        }
        let title = req.title.as_deref().map(str::trim);
        if title == Some("") {
            return Err(HubError::BadRequest("a task needs a title".into()));
        }

        let mut changes = Changes::default();
        self.db.board(|b| {
            // Re-read inside the transaction, and write only the requested columns: a move,
            // result or decision committed meanwhile keeps its rounds, results and flags.
            let mut task = b.task(&task.id)?.ok_or(HubError::NotFound("task"))?;
            if req.waiting.is_some()
                && actor != Actor::User
                && task.waiting.as_ref().is_some_and(|w| w.kind == WaitingKind::UsageLimit)
            {
                return Err(HubError::Conflict(format!(
                    "{} waits for the usage limit to reset; the hub clears it and wakes you then",
                    task.key
                )));
            }
            let mut edited = vec![];
            if let Some(title) = title {
                task.title = title.to_string();
                edited.push("title");
            }
            if let Some(description) = &req.description {
                task.description = description.clone();
                edited.push("description");
            }
            if let Some(priority) = req.priority {
                task.priority = priority;
                edited.push("priority");
            }
            if let Some(details) = &req.details {
                task.details = Some(details.clone()).filter(|d| !d.trim().is_empty());
            }
            if let Some(waiting) = &req.waiting {
                task.waiting = waiting.as_ref().map(|w| jgt_core::Waiting {
                    kind: WaitingKind::Question,
                    reason: w.reason.trim().to_string(),
                    question_id: w.question_id.clone(),
                    since: now_ms(),
                    until: None,
                });
            }
            if let Some(run_id) = &req.run_id {
                task.run_id = Some(run_id.clone()).filter(|r| !r.trim().is_empty());
            }
            if let Some(worktree) = &req.worktree {
                task.worktree = Some(worktree.clone()).filter(|w| !w.trim().is_empty());
            }
            let priority = task.priority.as_str();
            let waiting = crate::db::to_json(&task.waiting);
            let mut columns: Vec<(&str, &dyn rusqlite::ToSql)> = vec![];
            if req.title.is_some() {
                columns.push(("title", &task.title));
            }
            if req.description.is_some() {
                columns.push(("description", &task.description));
            }
            if req.priority.is_some() {
                columns.push(("priority", &priority));
            }
            if req.details.is_some() {
                columns.push(("details", &task.details));
            }
            if req.waiting.is_some() {
                columns.push(("waiting", &waiting));
            }
            if req.run_id.is_some() {
                columns.push(("run_id", &task.run_id));
            }
            if req.worktree.is_some() {
                columns.push(("worktree", &task.worktree));
            }
            b.set_columns(&task.id, &columns)?;
            if !edited.is_empty() {
                changes.log(b, &task.id, TaskEventKind::Edited, &actor, "", json!({ "fields": edited }))?;
            }
            if req.details.is_some() {
                changes.log(b, &task.id, TaskEventKind::Details, &actor, "", json!({}))?;
            }
            if req.waiting.is_some() {
                let body = task.waiting.as_ref().map_or("cleared", |w| w.reason.as_str());
                changes.log(b, &task.id, TaskEventKind::Waiting, &actor, body, json!({ "waiting": task.waiting }))?;
            }
            if req.run_id.is_some() || req.worktree.is_some() {
                let meta = json!({ "run_id": task.run_id, "worktree": task.worktree });
                changes.log(b, &task.id, TaskEventKind::Link, &actor, "", meta)?;
            }
            Ok::<_, HubError>(())
        })?;
        self.publish(changes);
        let task = self.task(&task.id, None)?;
        if req.waiting.as_ref().is_some_and(Option::is_some) {
            self.notify_waiting(&task)?;
        }
        Ok(task)
    }

    pub fn delete_task(&self, id: &str, workspace: Option<&str>, actor: &ActorRequest) -> HubResult<()> {
        let task = self.task(id, workspace)?;
        if self.writer(actor, &task.workspace)? != Actor::User {
            return Err(HubError::Forbidden("only the user deletes tasks".into()));
        }
        let mut changes = Changes::default();
        self.db.board(|b| {
            // Subtasks and follow-ups lose their link (ON DELETE SET NULL): publish them too.
            for t in b.tasks(&task.workspace, true)? {
                if t.parent_id.as_deref() == Some(task.id.as_str()) {
                    changes.task(&t.id);
                }
            }
            b.delete_task(&task.id)?;
            Ok::<_, HubError>(())
        })?;
        self.publish(changes);
        self.emit(jgt_core::Event::TaskRemoved { id: task.id, workspace: task.workspace });
        Ok(())
    }

    /// The team's transition table (plan §1.4, changed by PRD D3/D4). `Err` names the gate that
    /// refuses the move.
    ///
    /// The flow: coder → first verification → Done when clean; otherwise one automatic fix round
    /// (Review or Testing → Working) → second verification → Orchestrator review, where only the
    /// user decides.
    fn check_team_move(&self, b: &Board, task: &Task, to: TaskStatus, wip_limit: u8) -> HubResult<Result<(), String>> {
        use TaskStatus::*;
        let from = task.status;
        let refused = |why: String| Ok(Err(why));
        if from == OrchestratorReview && to != from {
            return refused(format!(
                "{} is in Orchestrator review: only the user decides (done, one more fix, take over or stop)",
                task.key
            ));
        }
        match (from, to) {
            (Backlog, OnQueue) => {
                if b.taken_from_team(&task.id)? {
                    return refused(format!("the user took {} away from the team", task.key));
                }
                let parent = match task.parent_id.as_deref() {
                    Some(p) => b.task(p)?,
                    None => None,
                };
                let Some(parent) = parent else {
                    return refused(
                        "the Coordinator queues only subtasks of a task the user queued (K10); the user queues the others"
                            .into(),
                    );
                };
                if !(parent.status == OnQueue || parent.status.in_progress()) {
                    return refused(format!("its parent {} is not on the queue or in progress (K10)", parent.key));
                }
                if !b.queued_by_user(&parent.id)? {
                    return refused(format!("its parent {} was not queued by the user (K10)", parent.key));
                }
            }
            (OnQueue, Working) => {
                let busy = b.count_in_progress(&task.workspace, &task.id)?;
                if busy >= i64::from(wip_limit) {
                    return refused(format!("WIP limit reached: {busy} task(s) in progress, the limit is {wip_limit} (K12)"));
                }
            }
            (Working, Testing) => {
                if let Some(w) = &task.waiting {
                    return refused(format!("{} waits for the user: {}", task.key, w.reason));
                }
            }
            (Testing, Working) => {
                if task.test_result.as_ref().map(|t| t.status) != Some(TestStatus::Fail) {
                    return refused("back to Working needs a red test of this pass".into());
                }
                if !task.fix_round_left() {
                    return refused(format!(
                        "verification {} of {} is done: no further automatic fix; move it to Orchestrator review (D3)",
                        task.round().0,
                        task.round_limit
                    ));
                }
            }
            (Testing, OrchestratorReview) => {
                if task.test_result.as_ref().map(|t| t.status) != Some(TestStatus::Fail) || task.fix_round_left() {
                    return refused(
                        "Orchestrator review from Testing needs a red test of the last verification before the user decides"
                            .into(),
                    );
                }
            }
            (Testing, Review) => {
                if task.test_result.as_ref().map(|t| t.status) != Some(TestStatus::Pass) {
                    return refused("Review needs a green test of this pass".into());
                }
            }
            (Review, Working) => {
                let Some(review) = task.review_result.as_ref().filter(|r| r.decision == ReviewDecision::Changes) else {
                    return refused("back to Working needs a review of this pass that requests changes".into());
                };
                if !task.fix_round_left() {
                    return refused(format!(
                        "verification {} of {} is done: no further automatic fix; move it to Orchestrator review (D3)",
                        review.round, task.round_limit
                    ));
                }
            }
            (Review, OrchestratorReview) => {
                if task.review_result.is_none() {
                    return refused("Orchestrator review needs the review of this pass".into());
                }
                if task.review_rounds == 0 {
                    return refused(
                        "the first verification goes to Done when clean, or back to Working for the fix round (D3)".into(),
                    );
                }
            }
            (Review, Done) => {
                if task.test_result.as_ref().map(|t| t.status) != Some(TestStatus::Pass) {
                    return refused("Done needs a green test of this pass (K6)".into());
                }
                let Some(review) = &task.review_result else {
                    return refused("Done needs a review of this pass (K6)".into());
                };
                if review.decision != ReviewDecision::Approved {
                    return refused("Done needs an approved review (K6)".into());
                }
                if task.review_rounds > 0 {
                    return refused(
                        "a task that needed a fix round goes to Orchestrator review for the user's decision (D3)".into(),
                    );
                }
            }
            (a, b) if a == b => return refused(format!("{} is already in {}", task.key, a.label())),
            _ => return refused(format!("the team cannot move a task from {} to {}", from.label(), to.label())),
        }
        Ok(Ok(()))
    }

    pub fn move_task(&self, id: &str, workspace: Option<&str>, req: &MoveTaskRequest) -> HubResult<MoveResponse> {
        let task = self.task(id, workspace)?;
        let actor = self.writer(&req.actor, &task.workspace)?;
        let wip_limit = self.config().team.wip_limit(Path::new(&task.workspace));
        let reason = req.reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
        let mut changes = Changes::default();
        let mut queued = false;
        self.db.board(|b| {
            // Re-read inside the transaction, so the gates see what is committed now.
            let mut task = b.task(&task.id)?.ok_or(HubError::NotFound("task"))?;
            let from = task.status;
            let to = req.to;
            queued = to == TaskStatus::OnQueue && from != to;
            if from == to {
                if actor != Actor::User {
                    return Err(HubError::Conflict(format!("{} is already in {}", task.key, to.label())));
                }
                // The user reorders a column: no log row, the card just moves.
                b.place(&task.id, &task.workspace, to, req.position)?;
                changes.task(&task.id);
                return Ok(());
            }
            let gate = self.check_team_move(b, &task, to, wip_limit)?;
            let gate_skipped = match (gate, &actor) {
                (Ok(()), _) => None,
                (Err(_), Actor::User) if users_own_move(from, to) => None,
                (Err(why), Actor::User) => Some(why),
                (Err(why), _) => return Err(HubError::Conflict(why)),
            };
            self.apply_move(b, &mut task, to, &actor, &mut changes)?;
            b.save(&task)?;
            b.place(&task.id, &task.workspace, to, req.position)?;
            let body = match (&actor, &gate_skipped, reason) {
                (Actor::User, Some(_), Some(r)) => format!("moved by you: {r}"),
                (Actor::User, Some(_), None) => "moved by you".to_string(),
                (_, _, r) => r.unwrap_or_default().to_string(),
            };
            let mut meta = json!({ "from": from, "to": to });
            if let Some(r) = reason {
                meta["reason"] = json!(r);
            }
            if let Some(why) = &gate_skipped {
                meta["gate_skipped"] = json!(why);
            }
            changes.log(b, &task.id, TaskEventKind::Moved, &actor, &body, meta)?;
            if to == TaskStatus::Done {
                self.follow_up(b, &task, &mut changes)?;
            }
            Ok(())
        })?;
        self.publish(changes);
        let before = task.status;
        let task = self.task(&task.id, None)?;
        if task.status == TaskStatus::Done && before != TaskStatus::Done && actor != Actor::User {
            let body = format!(
                "{} \"{}\" is Done: tested and reviewed. It is ready to merge once you agree.",
                task.key, task.title
            );
            self.notify_task(&task, "success", &format!("{} is Done", task.key), &body)?;
        }
        if task.status == TaskStatus::OrchestratorReview && before != task.status && actor != Actor::User {
            self.notify_orchestrator_review(&task)?;
        }
        // Queueing a task is the request (K4): the Coordinator hears of it, and the caller wakes it.
        let coordinator = if queued {
            let priority = match task.priority {
                TaskPriority::Normal => String::new(),
                p => format!(" (priority {p})"),
            };
            let text = format!(
                "[jg-team-dev] Task {} \"{}\" is on the queue{priority}. Read it with task_get.",
                task.key, task.title
            );
            self.tell_coordinator(&task, &text)?
        } else {
            None
        };
        Ok(MoveResponse { task, coordinator, queued })
    }

    /// Posts a `notify` message in the Coordinator's chat for a task reaching Done or getting
    /// *Waiting for you*: it shows in the chat, and the notifier turns it into a desktop
    /// notification like any other (no second notification path).
    fn notify_task(&self, task: &Task, level: &str, title: &str, body: &str) -> HubResult<()> {
        if let Some(coordinator) = self.db.current_coordinator(&task.workspace)? {
            let meta = json!({ "title": title, "level": level, "task_id": task.id });
            let req = AgentMessageRequest { kind: jgt_core::MessageKind::Notify, body: body.to_string(), meta };
            self.agent_message(&coordinator.id, &req)?;
        }
        Ok(())
    }

    /// A task reached Orchestrator review: the user decides (done, one more fix, take over, stop).
    fn notify_orchestrator_review(&self, task: &Task) -> HubResult<()> {
        let result = match &task.review_result {
            Some(r) => {
                let count = |l: Severity| r.findings.iter().filter(|f| f.level() == l).count();
                format!(
                    "verification {}: {}, {} critical, {} high, {} other",
                    r.round,
                    r.decision,
                    count(Severity::Critical),
                    count(Severity::High),
                    count(Severity::Other)
                )
            }
            None => match &task.test_result {
                Some(t) => format!("verification {}: test {}", task.round().0, t.status),
                None => "no verification recorded".into(),
            },
        };
        let body = format!(
            "{} \"{}\" waits for your review ({result}). Decide on its card: Done, One more fix, Take over or Stop.",
            task.key, task.title
        );
        self.notify_task(task, "warning", &format!("{} waits for your review", task.key), &body)
    }

    fn notify_waiting(&self, task: &Task) -> HubResult<()> {
        let Some(waiting) = &task.waiting else { return Ok(()) };
        let body = format!("{} \"{}\" is waiting for you: {}", task.key, task.title, waiting.reason);
        self.notify_task(task, "warning", &format!("{} is waiting for you", task.key), &body)
    }

    /// Queues `text` for the workspace's current Coordinator, as a relayed message tagged with the
    /// task, and returns that Coordinator so the caller can wake it (the hub never starts agents).
    /// With no Coordinator running, the task's log says so.
    pub(crate) fn tell_coordinator(&self, task: &Task, text: &str) -> HubResult<Option<Session>> {
        let Some(coordinator) = self.db.current_coordinator(&task.workspace)? else {
            let mut changes = Changes::default();
            self.db.board(|b| {
                changes.log(b, &task.id, TaskEventKind::Note, &Actor::Hub, NO_COORDINATOR, json!({}))
            })?;
            self.publish(changes);
            return Ok(None);
        };
        let meta = json!({ "relay_from": null, "task_id": task.id });
        let message = self.db.insert_message(
            &coordinator.id,
            jgt_core::Direction::ToAgent,
            jgt_core::MessageKind::Text,
            text,
            &meta,
            jgt_core::Delivery::Queued,
        )?;
        self.bump();
        self.emit_message(&message);
        self.emit_session(&coordinator.id);
        Ok(Some(self.session(&coordinator.id)?))
    }

    /// Reminds the Coordinator when it becomes Idle with tasks on the queue and capacity to work them.
    pub(crate) fn remind_queue_if_idle(&self, session: &Session) -> HubResult<()> {
        if !session.is_coordinator() {
            return Ok(());
        }
        let Some(workspace) = session.cwd.as_deref().map(normalize_workspace) else {
            return Ok(());
        };
        if workspace.is_empty() {
            return Ok(());
        }
        if self.db.has_queued(&session.id)? {
            return Ok(());
        }
        let tasks = self.tasks(&workspace, false)?;
        let on_queue: Vec<&Task> = tasks.iter().filter(|t| t.status == TaskStatus::OnQueue).collect();
        if on_queue.is_empty() {
            return Ok(());
        }
        let in_progress = tasks.iter().filter(|t| t.status.in_progress()).count();
        let wip_limit = self.config().team.wip_limit(Path::new(&workspace));
        if in_progress >= usize::from(wip_limit) {
            return Ok(());
        }
        let key_set = on_queue.iter().map(|t| t.key.as_str()).collect::<Vec<_>>().join(",");
        {
            let reminders = self.queue_reminders.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((last_keys, last_time)) = reminders.get(&session.id)
                && last_keys == &key_set
                && last_time.elapsed() < Duration::from_secs(10 * 60)
            {
                return Ok(());
            }
        }
        let first_task = on_queue[0];
        let keys_str = if on_queue.len() > 5 {
            let first5 = on_queue[..5].iter().map(|t| t.key.as_str()).collect::<Vec<_>>().join(", ");
            format!("{first5}, ...")
        } else {
            on_queue.iter().map(|t| t.key.as_str()).collect::<Vec<_>>().join(", ")
        };
        let text = format!(
            "[jg-team-dev] {} task(s) are on the queue: {keys_str}. Take the next one (task_list status=on_queue).",
            on_queue.len()
        );
        let meta = json!({ "relay_from": null, "task_id": first_task.id });
        let message = self.db.insert_message(
            &session.id,
            jgt_core::Direction::ToAgent,
            jgt_core::MessageKind::Text,
            &text,
            &meta,
            jgt_core::Delivery::Queued,
        )?;
        self.queue_reminders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.id.clone(), (key_set, Instant::now()));
        self.bump();
        self.emit_message(&message);
        self.emit_session(&session.id);
        Ok(())
    }

    /// What entering `to` does to a task's results and flags. The task is saved by the caller.
    fn apply_move(&self, b: &Board, task: &mut Task, to: TaskStatus, actor: &Actor, changes: &mut Changes) -> HubResult<()> {
        use TaskStatus::*;
        let from = task.status;
        if task.archived {
            // Moving an archived task brings it back on the Board.
            task.archived = false;
        }
        if from == Done {
            // Reopened: a fresh start (decision 3).
            task.review_rounds = 0;
            task.round_limit = ROUND_LIMIT;
            task.test_result = None;
            task.review_result = None;
            task.waiting = None;
            task.done_at = None;
        }
        if matches!(from, Review | Testing | OrchestratorReview) && to == Working {
            // A fix round. Out of Orchestrator review it is the user's "one more fix" (by a
            // decision or a drag): it grants the verification that follows.
            task.review_rounds += 1;
            if from == OrchestratorReview {
                task.round_limit = task.round_limit.max(task.review_rounds) + MORE_ROUNDS;
            }
            if from == Review {
                task.test_result = None;
            }
        }
        match to {
            // A new pass starts: no green test or approved review may outlive it (a user move
            // Testing → Working → Review would otherwise carry the old green test). Only what
            // sent the task back stays, for the coder: the red test's failures, the review's
            // requested changes.
            Working => {
                task.test_result = task.test_result.take().filter(|t| t.status == TestStatus::Fail);
                task.review_result = task.review_result.take().filter(|r| r.decision == ReviewDecision::Changes);
            }
            Testing => task.test_result = None,
            // Only a test of the pass that just left Testing counts for this review: a green test
            // kept through a Stop or a take-over must not reach Done by a user move to Review.
            Review => {
                task.review_result = None;
                if from != Testing {
                    task.test_result = None;
                }
            }
            Done => task.done_at = Some(now_ms()),
            _ => {}
        }
        let legacy_limit = task.waiting.as_ref().is_some_and(|w| w.kind == WaitingKind::RoundLimit);
        if task.waiting.is_some() && (matches!(to, Backlog | Done) || legacy_limit) {
            task.waiting = None;
            changes.log(b, &task.id, TaskEventKind::Waiting, actor, "cleared", json!({ "waiting": null }))?;
        }
        Ok(())
    }

    /// On Done with review findings left (an approved review with findings that are neither
    /// critical nor high, the user's Done from Orchestrator review, or a user move): a follow-up
    /// task in Backlog that carries them (K8, E4).
    fn follow_up(&self, b: &Board, task: &Task, changes: &mut Changes) -> HubResult<()> {
        let Some(review) = &task.review_result else {
            return Ok(());
        };
        if review.findings.is_empty() {
            return Ok(());
        }
        let description = format!(
            "Findings left by review round {} of {} \"{}\":\n\n{}\n",
            review.round,
            task.key,
            task.title,
            render_findings(&review.findings)
        );
        let follow = b.insert_task(&NewTask {
            workspace: &task.workspace,
            title: &format!("Follow-up: {}", task.title),
            description: &description,
            priority: TaskPriority::Normal,
            status: TaskStatus::Backlog,
            parent_id: Some(&task.id),
        })?;
        let meta = json!({ "status": TaskStatus::Backlog, "parent_id": task.id });
        changes.log(b, &follow.id, TaskEventKind::Created, &Actor::Hub, "", meta)?;
        let body = format!("follow-up of {}", task.key);
        changes.log(b, &follow.id, TaskEventKind::FollowUp, &Actor::Hub, &body, json!({ "task": task.id }))?;
        let body = format!("{} findings moved to {}", review.findings.len(), follow.key);
        changes.log(b, &task.id, TaskEventKind::FollowUp, &Actor::Hub, &body, json!({ "follow_up": follow.id }))?;
        Ok(())
    }

    /// Checks that `worker` is a session with one of `roles`, started by the acting Coordinator.
    fn check_worker(&self, worker: &str, roles: &[&str], actor: &Actor) -> HubResult<Session> {
        let session = match self.db.session(worker)? {
            Some(s) => s,
            None => {
                let matches = if worker.len() >= 8 {
                    self.db.sessions_by_prefix(worker)?
                } else {
                    Vec::new()
                };
                match matches.len() {
                    1 => matches.into_iter().next().unwrap(),
                    0 => {
                        return Err(HubError::BadRequest(format!(
                            "worker {worker} is not a known session; use the full session id (26 chars, `jg-team-dev sessions`)"
                        )));
                    }
                    n => {
                        let list = matches
                            .iter()
                            .map(|s| format!("{} ({})", s.display_name(), s.id))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(HubError::BadRequest(format!("{n} sessions match `{worker}`: {list}")));
                    }
                }
            }
        };
        let role = session.role.clone().unwrap_or_default();
        if !roles.contains(&role.as_str()) {
            return Err(HubError::BadRequest(format!(
                "worker {} is not a {}",
                session.display_name(),
                roles.join(" or a ")
            )));
        }
        if let Actor::Coordinator { session: coordinator } = actor
            && session.parent_id.as_deref() != Some(coordinator.as_str())
        {
            return Err(HubError::BadRequest(format!(
                "{role} {} was not started by this Coordinator",
                session.display_name()
            )));
        }
        Ok(session)
    }

    /// Records the test (task in Testing) or the review (task in Review) of the current pass.
    pub fn record_results(&self, id: &str, workspace: Option<&str>, req: &ResultRequest) -> HubResult<Task> {
        let task = self.task(id, workspace)?;
        let actor = self.writer(&req.actor, &task.workspace)?;
        let mut changes = Changes::default();
        match (&req.test, &req.review) {
            (Some(test), None) => {
                let worker = self.check_worker(&test.worker, TEST_ROLES, &actor)?;
                self.db.board(|b| {
                    let mut task = b.task(&task.id)?.ok_or(HubError::NotFound("task"))?;
                    if task.status != TaskStatus::Testing {
                        return Err(HubError::Conflict(format!(
                            "{} is in {}: test results go to a task in Testing",
                            task.key,
                            task.status.label()
                        )));
                    }
                    task.test_result = Some(TestResult {
                        status: test.status,
                        worker: worker.id.clone(),
                        summary: test.summary.clone(),
                        failures: test.failures.clone(),
                        at: now_ms(),
                    });
                    b.save(&task)?;
                    let meta = json!({
                        "status": test.status, "worker": worker.id, "failures": test.failures, "role": worker.role,
                    });
                    let body = test.summary.clone();
                    changes.log(b, &task.id, TaskEventKind::TestResult, &actor, &body, meta)?;
                    Ok(())
                })?;
            }
            (None, Some(review)) => {
                let findings = validate_findings(&review.findings)?;
                let count = |l: Severity| findings.iter().filter(|f| f.level() == l).count();
                let (critical, high) = (count(Severity::Critical), count(Severity::High));
                match review.decision {
                    ReviewDecision::Approved if critical + high > 0 => {
                        return Err(HubError::BadRequest(
                            "an approved review cannot carry a critical or high finding".into(),
                        ));
                    }
                    ReviewDecision::Changes if critical + high == 0 => {
                        return Err(HubError::BadRequest(
                            "a review that requests changes needs a critical or high finding; findings that are \
                             neither never start a fix round: approve, and they go to the follow-up task (E4)"
                                .into(),
                        ));
                    }
                    _ => {}
                }
                let worker = self.check_worker(&review.worker, REVIEW_ROLES, &actor)?;
                self.db.board(|b| {
                    let mut task = b.task(&task.id)?.ok_or(HubError::NotFound("task"))?;
                    if task.status != TaskStatus::Review {
                        return Err(HubError::Conflict(format!(
                            "{} is in {}: reviews go to a task in Review",
                            task.key,
                            task.status.label()
                        )));
                    }
                    let round = task.review_rounds + 1;
                    let result = ReviewResult {
                        decision: review.decision,
                        round,
                        worker: worker.id.clone(),
                        findings,
                        at: now_ms(),
                    };
                    let meta = json!({
                        "decision": review.decision, "round": round, "worker": worker.id,
                        "findings": result.findings.len(), "critical": critical, "high": high, "role": worker.role,
                    });
                    task.review_result = Some(result);
                    b.save(&task)?;
                    let body = format!("round {round}: {}", review.decision);
                    changes.log(b, &task.id, TaskEventKind::ReviewResult, &actor, &body, meta)?;
                    Ok(())
                })?;
            }
            _ => return Err(HubError::BadRequest("send either a test or a review result".into())),
        }
        self.publish(changes);
        self.task(&task.id, None)
    }

    /// The user's decision on a task in Orchestrator review (PRD D3/D4): done, one more fix, take
    /// over or stop. The Coordinator must cite the `chat_ask` question the user answered, asked
    /// after the task got there.
    pub fn decide(&self, id: &str, workspace: Option<&str>, req: &DecisionRequest) -> HubResult<Task> {
        let task = self.task(id, workspace)?;
        let actor = self.writer(&req.actor, &task.workspace)?;
        let not_waiting =
            |task: &Task| HubError::Conflict(format!("{} is not in Orchestrator review: there is nothing to decide", task.key));
        if task.status != TaskStatus::OrchestratorReview {
            return Err(not_waiting(&task));
        }
        // The cited question, read before the transaction (`Db::board` holds the connection);
        // it is checked against the task as the transaction sees it.
        let question = match &actor {
            Actor::Coordinator { session } => {
                let question_id = req.question_id.as_deref().ok_or_else(|| {
                    HubError::Forbidden("this is the user's decision: cite the chat_ask question they answered".into())
                })?;
                let question = self
                    .db
                    .question(question_id)?
                    .ok_or_else(|| HubError::Forbidden(format!("question {question_id} does not exist")))?;
                Some((session, question))
            }
            _ => None,
        };
        let mut changes = Changes::default();
        self.db.board(|b| {
            let mut task = b.task(&task.id)?.ok_or(HubError::NotFound("task"))?;
            // Re-checked here: a second decision sent meanwhile finds the task gone from the column.
            if task.status != TaskStatus::OrchestratorReview {
                return Err(not_waiting(&task));
            }
            if let Some((session, question)) = &question {
                let since = b.entered(&task.id, TaskStatus::OrchestratorReview)?.unwrap_or(task.updated_at);
                if question.session_id != **session
                    || question.status != QuestionStatus::Answered
                    || question.created_at < since
                {
                    return Err(HubError::Forbidden(format!(
                        "question {} is not an answered question you asked after {} reached Orchestrator review",
                        question.id, task.key
                    )));
                }
            }
            let from = task.status;
            let (to, body) = match req.choice {
                Decision::Done => (TaskStatus::Done, "accepted: Done".to_string()),
                Decision::MoreRounds => (TaskStatus::Working, "one more fix and verification".to_string()),
                Decision::TakeOver => (TaskStatus::Backlog, "the user took over; the team leaves this task".to_string()),
                Decision::Stop => (TaskStatus::Backlog, "stopped; back to Backlog with the findings".to_string()),
            };
            self.apply_move(b, &mut task, to, &actor, &mut changes)?;
            let mut meta = json!({ "choice": req.choice, "question_id": req.question_id });
            if req.choice == Decision::MoreRounds {
                meta["round_limit"] = json!(task.round_limit);
            }
            b.save(&task)?;
            changes.log(b, &task.id, TaskEventKind::Decision, &actor, &body, meta)?;
            b.place(&task.id, &task.workspace, to, None)?;
            let meta = json!({ "from": from, "to": to, "reason": req.choice });
            changes.log(b, &task.id, TaskEventKind::Moved, &actor, &body, meta)?;
            if to == TaskStatus::Done {
                self.follow_up(b, &task, &mut changes)?;
            }
            Ok::<_, HubError>(())
        })?;
        self.publish(changes);
        self.task(&task.id, None)
    }

    /// Clears *Waiting: usage limit* on every task whose limit reset `RESUME_AFTER_MS` ago, and
    /// queues "[jg-team-dev] Usage limit reset: resume JGT-n" for its Coordinator (E7 step 3).
    /// The monitor calls it and wakes the Coordinators it returns: the hub owns this timer,
    /// because the Coordinator may be asleep or stopped by the limit itself.
    pub fn resume_usage_limits(&self) -> HubResult<Vec<Session>> {
        let now = now_ms();
        let due: Vec<(Task, Option<String>)> = self.db.board(|b| {
            let mut due = vec![];
            for task in b.waiting_tasks()? {
                let Some(w) = task.waiting.as_ref().filter(|w| w.kind == WaitingKind::UsageLimit) else { continue };
                if w.until.is_some_and(|until| until + RESUME_AFTER_MS <= now) {
                    let resume = b.events(&task.id)?.into_iter().rev().find_map(|e| {
                        (e.kind == TaskEventKind::Waiting)
                            .then(|| e.meta.get("resume_brief").and_then(|p| p.as_str()).map(str::to_owned))
                            .flatten()
                    });
                    due.push((task, resume));
                }
            }
            Ok::<_, rusqlite::Error>(due)
        })?;
        let mut wake: Vec<Session> = vec![];
        for (task, resume) in due {
            let mut changes = Changes::default();
            self.db.board(|b| {
                let null: Option<String> = None;
                b.set_columns(&task.id, &[("waiting", &null)])?;
                changes.log(b, &task.id, TaskEventKind::Waiting, &Actor::Hub, "cleared: the usage limit reset", json!({ "waiting": null }))
            })?;
            self.publish(changes);
            let resume = resume.map(|p| format!(" Resume brief: {p}.")).unwrap_or_default();
            let text = format!(
                "[jg-team-dev] Usage limit reset: resume {} \"{}\". Start a fresh worker in the same worktree with the \
                 resume brief; it must not restart the work.{resume}",
                task.key, task.title
            );
            if let Some(c) = self.tell_coordinator(&task, &text)?
                && !wake.iter().any(|w| w.id == c.id)
            {
                wake.push(c);
            }
        }
        Ok(wake)
    }

    /// Archives the Done tasks that got there more than seven days ago (K15); the monitor calls
    /// it on its slow tick. Returns how many it archived.
    pub fn archive_done_tasks(&self) -> HubResult<usize> {
        let mut changes = Changes::default();
        let count = self.db.board(|b| {
            let ids = b.done_before(now_ms() - ARCHIVE_AFTER_MS)?;
            for id in &ids {
                b.archive(id)?;
                changes.log(b, id, TaskEventKind::Archived, &Actor::Hub, "7 days in Done", json!({}))?;
            }
            Ok::<_, rusqlite::Error>(ids.len())
        })?;
        if count > 0 {
            self.publish(changes);
        }
        Ok(count)
    }

    /// Adds a comment, question, answer, link or worker start/finish to a task's log. Workers may
    /// only report their own start and finish.
    pub fn add_task_event(&self, id: &str, workspace: Option<&str>, req: &TaskEventRequest) -> HubResult<TaskEvent> {
        use TaskEventKind::*;
        let task = self.task(id, workspace)?;
        let actor = self.actor(&req.actor, &task.workspace)?;
        if !matches!(req.kind, Comment | Question | Answer | Link | WorkerStarted | WorkerFinished) {
            return Err(HubError::BadRequest(format!("{} events are written by the hub", req.kind)));
        }
        if matches!(actor, Actor::Worker { .. }) && !matches!(req.kind, WorkerStarted | WorkerFinished) {
            return Err(HubError::Forbidden(
                "workers can read the Board but not change it; report to your Coordinator".into(),
            ));
        }
        if req.kind == Comment && req.body.trim().is_empty() {
            return Err(HubError::BadRequest("the comment is empty".into()));
        }
        let mut changes = Changes::default();
        let limit = (req.kind == WorkerFinished).then(|| usage_limit(&req.meta)).flatten();
        let mut flagged = false;
        let event = self.db.board(|b| {
            changes.log(b, &task.id, req.kind, &actor, req.body.trim(), req.meta.clone())?;
            let event = changes.events.last().cloned().ok_or(HubError::NotFound("task event"))?;
            // A worker stopped by the usage limit (E7): the task waits until the reset, unless it
            // already waits for the user's answer.
            if let Some((resets, until, resume)) = &limit {
                let mut task = b.task(&task.id)?.ok_or(HubError::NotFound("task"))?;
                if task.waiting.as_ref().is_none_or(|w| w.kind == WaitingKind::UsageLimit) {
                    task.waiting = Some(jgt_core::Waiting {
                        kind: WaitingKind::UsageLimit,
                        reason: format!("usage limit, resets {resets}"),
                        question_id: None,
                        since: now_ms(),
                        until: *until,
                    });
                    let waiting = crate::db::to_json(&task.waiting);
                    b.set_columns(&task.id, &[("waiting", &waiting)])?;
                    let meta = json!({ "waiting": task.waiting, "resume_brief": resume });
                    changes.log(b, &task.id, TaskEventKind::Waiting, &Actor::Hub, &format!("usage limit, resets {resets}"), meta)?;
                    flagged = true;
                }
            }
            Ok::<_, HubError>(event)
        })?;
        self.publish(changes);
        if flagged {
            self.notify_waiting(&self.task(&task.id, None)?)?;
        }
        // A comment of the user's reaches the Coordinator like a chat message (K13); the client
        // wakes it, as after a queue move.
        if req.kind == Comment && actor == Actor::User {
            let text = format!("[jg-team-dev] Comment on {} \"{}\": {}", task.key, task.title, req.body.trim());
            self.tell_coordinator(&task, &text)?;
        }
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_lose_the_trailing_slash() {
        assert_eq!(normalize_workspace("/work/app/"), "/work/app");
        assert_eq!(normalize_workspace("/work/app"), "/work/app");
        assert_eq!(normalize_workspace("/"), "/");
        assert_eq!(normalize_workspace("  "), "");
    }
}
