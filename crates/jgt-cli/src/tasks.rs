//! `jg-team-dev task`: the workspace Board from scripts and the terminal.
//!
//! The CLI only forwards: every gate is checked by the hub. It acts as the calling agent session
//! when there is one (a Coordinator, or a worker, which may only read), else as the user, whose
//! moves may skip gates and are logged as "moved by you".

use crate::runs::read_text;
use anyhow::{Context, Result, bail};
use chrono::{Local, TimeZone};
use jgt_core::{
    Decision, HubClient, ReviewDecision, Session, Task, TaskEvent, TaskEventKind, TaskPriority, TaskStatus, TestStatus,
    api::*, client::encode_query, procs,
};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct TaskArgs {
    /// The workspace (Coordinator folder). Default: the calling Coordinator's folder, else the
    /// current folder.
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    #[command(subcommand)]
    command: TaskCommand,
}

#[derive(clap::Subcommand)]
pub enum TaskCommand {
    /// List the Board, column by column.
    List {
        /// Include archived tasks.
        #[arg(long)]
        all: bool,
    },
    /// Show a task.
    Show { task: String },
    /// Create a task (in Backlog, or On Queue with --queue).
    Create {
        #[arg(long)]
        title: String,
        /// low, normal or high.
        #[arg(long, value_parser = parse_priority)]
        priority: Option<TaskPriority>,
        /// Put it straight on the queue (the user only).
        #[arg(long)]
        queue: bool,
        /// The task this one is a subtask of (key or id).
        #[arg(long)]
        parent: Option<String>,
        /// Read the description from this file.
        #[arg(long)]
        file: Option<PathBuf>,
        /// The description.
        description: Vec<String>,
    },
    /// Change a task's title, priority or description (the user only).
    Edit {
        task: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, value_parser = parse_priority)]
        priority: Option<TaskPriority>,
        #[arg(long)]
        description_file: Option<PathBuf>,
    },
    /// Write a task's Details: plan, files, acceptance criteria (markdown).
    Details {
        task: String,
        #[arg(long)]
        file: Option<PathBuf>,
        text: Vec<String>,
    },
    /// Move a task to a column: backlog, on_queue, working, testing, review or done.
    Move {
        task: String,
        #[arg(value_parser = parse_column)]
        column: TaskStatus,
        /// Index in the column (default: the end).
        #[arg(long)]
        position: Option<i64>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Record the test (`--test pass|fail --worker ID`) or the review (`--review-file`) of a task.
    Result {
        task: String,
        #[arg(long, value_parser = parse_test, conflicts_with = "review_file", requires = "worker")]
        test: Option<TestStatus>,
        /// Hub id of the coder.
        #[arg(long)]
        worker: Option<String>,
        /// The test summary, from a file.
        #[arg(long, requires = "test")]
        file: Option<PathBuf>,
        /// The review as JSON: `{"decision": "approved"|"changes", "worker": "<reviewer id>",
        /// "findings": [{"severity": "critical"|"high"|"other", "critical": bool, "category": "security"|…,
        /// "problem": "…", …}]}`.
        #[arg(long)]
        review_file: Option<PathBuf>,
    },
    /// Decide on a task in Orchestrator review: done, more-rounds (one more fix), take-over or stop.
    Decide {
        task: String,
        #[arg(value_parser = parse_decision)]
        choice: Decision,
        /// For the Coordinator: the answered chat_ask question with the user's choice.
        #[arg(long)]
        question: Option<String>,
    },
    /// Comment on a task.
    Comment { task: String, text: Vec<String> },
    /// Delete a task (the user only; its key is never reused).
    Delete { task: String },
    /// Show a task's activity log.
    Log { task: String },
    /// Write the Board as Markdown (default: docs/board.md in the workspace).
    Export {
        /// The file to write.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Include archived tasks.
        #[arg(long)]
        all: bool,
    },
}

/// Printed when the folder has no Coordinator to hear about a queued task or a comment.
const NO_COORDINATOR: &str = "No Coordinator is running for this folder";

fn parse_priority(s: &str) -> Result<TaskPriority, String> {
    s.to_lowercase().parse()
}

/// Column names as typed: `on_queue`, `on-queue`, `"On Queue"`, `queue`.
pub(crate) fn parse_column(s: &str) -> Result<TaskStatus, String> {
    let s = s.trim().to_lowercase().replace(['-', ' '], "_");
    let s = match s.as_str() {
        "queue" | "onqueue" => "on_queue".to_string(),
        "orchestrator" | "orchestratorreview" => "orchestrator_review".to_string(),
        _ => s,
    };
    s.parse().map_err(|_| {
        format!("unknown column `{s}`; use one of: {}", TaskStatus::ALL.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "))
    })
}

fn parse_test(s: &str) -> Result<TestStatus, String> {
    match s.to_lowercase().as_str() {
        "pass" | "passed" | "green" => Ok(TestStatus::Pass),
        "fail" | "failed" | "red" => Ok(TestStatus::Fail),
        other => Err(format!("unknown test status `{other}`; use pass or fail")),
    }
}

/// Decisions as typed: `done`, `more_rounds` (or `one more fix`), `take_over`, `stop`.
pub(crate) fn parse_decision(s: &str) -> Result<Decision, String> {
    let s = s.trim().to_lowercase().replace(['-', ' '], "_");
    match s.as_str() {
        "one_more_fix" | "more_fix" | "fix" => Ok(Decision::MoreRounds),
        "accept" => Ok(Decision::Done),
        _ => s.parse().map_err(|_| format!("unknown decision `{s}`; use done, more_rounds, take_over or stop")),
    }
}

fn time(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

/// The agent session running this command, if it is one the hub knows.
async fn calling_session(client: &HubClient) -> Option<Session> {
    let pid = procs::harness_pid()?;
    client.get(&format!("/v1/resolve?pid={pid}")).await.ok()
}

/// Who the CLI acts as, and in which workspace.
pub(crate) struct Board {
    pub(crate) client: HubClient,
    actor: ActorRequest,
    pub(crate) workspace: String,
}

impl Board {
    pub(crate) async fn new(workspace: Option<PathBuf>) -> Result<Board> {
        let client = HubClient::default();
        let caller = calling_session(&client).await;
        let workspace = match (workspace, &caller) {
            (Some(dir), _) => dir.canonicalize().unwrap_or(dir),
            (None, Some(s)) if s.is_coordinator() => s.cwd.clone().map(PathBuf::from).context("the Coordinator has no folder")?,
            (None, Some(s)) => match &s.parent_id {
                Some(parent) => {
                    let coordinator: Session = client.get(&format!("/v1/sessions/{parent}")).await?;
                    coordinator.cwd.map(PathBuf::from).context("the Coordinator has no folder")?
                }
                None => std::env::current_dir()?.canonicalize()?,
            },
            (None, None) => std::env::current_dir()?.canonicalize()?,
        };
        let actor = match caller {
            Some(s) => ActorRequest::Session { session: s.id },
            None => ActorRequest::User,
        };
        Ok(Board { client, actor, workspace: workspace.to_string_lossy().into_owned() })
    }

    /// `/v1/tasks/<task>[/<tail>]?workspace=…`, so `<task>` may be a key.
    pub(crate) fn path(&self, task: &str, tail: &str) -> String {
        format!("/v1/tasks/{}{tail}?workspace={}", encode_query(task), encode_query(&self.workspace))
    }
}

pub async fn command(args: TaskArgs) -> Result<()> {
    let cx = Board::new(args.workspace).await?;
    match args.command {
        TaskCommand::List { all } => {
            let r: TasksResponse = cx
                .client
                .get(&format!("/v1/tasks?workspace={}&archived={all}", encode_query(&cx.workspace)))
                .await?;
            if r.tasks.is_empty() {
                println!("The Board of {} is empty. Add a task with `jg-team-dev task create --title …`.", cx.workspace);
                return Ok(());
            }
            println!("{:<8} {:<9} {:<7} {:<6} {:<8}  TITLE", "KEY", "STATUS", "PRI", "ROUND", "FLAGS");
            for t in r.tasks {
                // The review cycle the task is in, as on the card ("round 2/3").
                let round = if t.shows_round() { round(&t) } else { String::new() };
                let mut flags = vec![];
                if t.waiting.is_some() {
                    flags.push("waiting");
                }
                if t.archived {
                    flags.push("archived");
                }
                println!(
                    "{:<8} {:<9} {:<7} {:<6} {:<8}  {}",
                    t.key,
                    t.status.as_str(),
                    t.priority.as_str(),
                    round,
                    flags.join(","),
                    t.title
                );
            }
        }
        TaskCommand::Show { task } => {
            let t: Task = cx.client.get(&cx.path(&task, "")).await?;
            print_task(&t);
        }
        TaskCommand::Create { title, priority, queue, parent, file, description } => {
            let description =
                if file.is_some() || !description.is_empty() { read_text(file.as_deref(), &description, "description")? } else { String::new() };
            let t: Task = cx
                .client
                .post("/v1/tasks", &CreateTaskRequest {
                    actor: cx.actor.clone(),
                    workspace: cx.workspace.clone(),
                    title,
                    description,
                    priority,
                    status: queue.then_some(TaskStatus::OnQueue),
                    parent_id: parent,
                })
                .await?;
            println!("{} created in {}: {}", t.key, t.status.label(), t.title);
        }
        TaskCommand::Edit { task, title, priority, description_file } => {
            let mut req = PatchTaskRequest::new(cx.actor.clone());
            req.title = title;
            req.priority = priority;
            if let Some(file) = description_file {
                req.description = Some(std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?);
            }
            let t: Task = cx.client.patch(&cx.path(&task, ""), &req).await?;
            println!("{} updated", t.key);
        }
        TaskCommand::Details { task, file, text } => {
            let mut req = PatchTaskRequest::new(cx.actor.clone());
            req.details = Some(read_text(file.as_deref(), &text, "details")?);
            let t: Task = cx.client.patch(&cx.path(&task, ""), &req).await?;
            println!("{} details written", t.key);
        }
        TaskCommand::Move { task, column, position, reason } => {
            let r: MoveResponse = cx
                .client
                .post(&cx.path(&task, "/move"), &MoveTaskRequest { actor: cx.actor.clone(), to: column, position, reason })
                .await?;
            println!("{} is in {}", r.task.key, r.task.status.label());
            match &r.coordinator {
                // The queue message waits for it; wake it if it sleeps.
                Some(coordinator) => jgt_core::team::wake(&cx.client, coordinator).await.map_err(anyhow::Error::msg)?,
                None if r.queued => println!(
                    "{} (the queue waits). Start the team with `jg-team-dev start {}`.",
                    NO_COORDINATOR, cx.workspace
                ),
                None => {}
            }
        }
        TaskCommand::Result { task, test, worker, file, review_file } => {
            let req = match (test, review_file) {
                (Some(status), None) => {
                    let summary = match file {
                        Some(f) => std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?,
                        None => String::new(),
                    };
                    let worker = worker.context("pass the coder with --worker")?;
                    ResultRequest {
                        actor: cx.actor.clone(),
                        test: Some(TestResultInput { status, worker, summary, failures: vec![] }),
                        review: None,
                    }
                }
                (None, Some(path)) => {
                    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                    let review: ReviewResultInput =
                        serde_json::from_str(&text).with_context(|| format!("{} is not a review", path.display()))?;
                    ResultRequest { actor: cx.actor.clone(), test: None, review: Some(review) }
                }
                _ => bail!("pass --test pass|fail --worker <id>, or --review-file <json>"),
            };
            let t: Task = cx.client.post(&cx.path(&task, "/results"), &req).await?;
            match (&t.test_result, &t.review_result) {
                (_, Some(r)) if req.review.is_some() => {
                    println!("{} review round {}: {}", t.key, r.round, r.decision);
                    if r.decision == ReviewDecision::Changes && t.waiting.is_some() {
                        println!("{} now waits for the user: {}", t.key, t.waiting.as_ref().map_or("", |w| w.reason.as_str()));
                    }
                }
                (Some(r), _) => println!("{} test: {}", t.key, r.status),
                _ => println!("{} updated", t.key),
            }
        }
        TaskCommand::Decide { task, choice, question } => {
            let t: Task = cx
                .client
                .post(&cx.path(&task, "/decision"), &DecisionRequest { actor: cx.actor.clone(), choice, question_id: question })
                .await?;
            println!("{} {}: in {}, round {}", t.key, choice, t.status.label(), round(&t));
        }
        TaskCommand::Comment { task, text } => {
            let body = read_text(None, &text, "comment")?;
            let _: TaskEvent = cx
                .client
                .post(&cx.path(&task, "/events"), &TaskEventRequest {
                    actor: cx.actor.clone(),
                    kind: TaskEventKind::Comment,
                    body,
                    meta: serde_json::Value::Null,
                })
                .await?;
            println!("comment added");
            // The hub relays a comment of the user's to the Coordinator; wake it to read it.
            if cx.actor == ActorRequest::User {
                match jgt_core::team::current_coordinator(&cx.client, &cx.workspace).await.map_err(anyhow::Error::msg)? {
                    Some(coordinator) => jgt_core::team::wake(&cx.client, &coordinator).await.map_err(anyhow::Error::msg)?,
                    None => println!("{NO_COORDINATOR}: it reads the comment once it starts."),
                }
            }
        }
        TaskCommand::Export { out, all } => {
            let r: TasksResponse = cx
                .client
                .get(&format!("/v1/tasks?workspace={}&archived={all}", encode_query(&cx.workspace)))
                .await?;
            let path = out.unwrap_or_else(|| jgt_core::board::export_path(&cx.workspace));
            jgt_core::board::write_atomic(&path, &jgt_core::board::to_markdown(&r.tasks))
                .with_context(|| format!("writing {}", path.display()))?;
            println!("Board exported to {} ({} tasks)", path.display(), r.tasks.len());
        }
        TaskCommand::Delete { task } => {
            let who = match &cx.actor {
                ActorRequest::User => "actor=user".to_string(),
                ActorRequest::Session { session } => format!("session={}", encode_query(session)),
            };
            let () = cx.client.delete(&format!("{}&{who}", cx.path(&task, ""))).await?;
            println!("{task} deleted");
        }
        TaskCommand::Log { task } => {
            let t: Task = cx.client.get(&cx.path(&task, "")).await?;
            let r: TaskEventsResponse = cx.client.get(&cx.path(&t.id, "/events")).await?;
            println!("── {} · {}", t.key, t.title);
            for e in r.events {
                println!("{} {:<12} {}", time(e.created_at), e.actor.label(), describe(&e));
            }
        }
    }
    Ok(())
}

/// `2/3`: the current review cycle and the limit.
fn round(t: &Task) -> String {
    let (round, limit) = t.round();
    format!("{round}/{limit}")
}

/// One line for an activity log row.
pub(crate) fn describe(e: &TaskEvent) -> String {
    let field = |k: &str| e.meta.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let text = match e.kind {
        TaskEventKind::Moved => {
            let mut line = format!("moved {} → {}", field("from"), field("to"));
            if !e.body.is_empty() {
                line.push_str(&format!(" ({})", e.body));
            }
            if let Some(gate) = e.meta.get("gate_skipped").and_then(|v| v.as_str()) {
                line.push_str(&format!(" [gate skipped: {gate}]"));
            }
            return line;
        }
        TaskEventKind::Created => return format!("created in {}", field("status")),
        _ => e.body.clone(),
    };
    if text.is_empty() { e.kind.to_string() } else { format!("{}: {}", e.kind, text.trim()) }
}

fn print_task(t: &Task) {
    print!("{}", show(t));
}

/// What `task show` prints for a task.
pub(crate) fn show(t: &Task) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{} · {}", t.key, t.title);
    let _ = writeln!(out, "status     {}", t.status.label());
    let _ = writeln!(out, "priority   {}", t.priority);
    if t.shows_round() {
        let _ = writeln!(out, "round      {}", round(t));
    }
    if let Some(w) = &t.waiting {
        let _ = writeln!(out, "waiting    {} ({})", w.reason, w.kind);
    }
    if let Some(worker) = &t.current_worker {
        let _ = writeln!(out, "worker     ↳ {worker}");
    }
    for (label, value) in [("run", &t.run_id), ("worktree", &t.worktree), ("parent", &t.parent_id)] {
        if let Some(v) = value {
            let _ = writeln!(out, "{label:<10} {v}");
        }
    }
    let _ = writeln!(out, "created    {}", time(t.created_at));
    if let Some(done) = t.done_at {
        let _ = writeln!(out, "done       {}", time(done));
    }
    if !t.description.trim().is_empty() {
        let _ = writeln!(out, "\n## Description\n{}", t.description.trim());
    }
    if let Some(details) = &t.details {
        let _ = writeln!(out, "\n## Details\n{}", details.trim());
    }
    if let Some(test) = &t.test_result {
        let _ = writeln!(out, "\n## Test: {}\n{}", test.status, test.summary.trim());
        for f in &test.failures {
            let _ = writeln!(out, "- {f}");
        }
    }
    if let Some(review) = &t.review_result {
        let _ = writeln!(out, "\n## Review round {}: {}", review.round, review.decision);
        for f in &review.findings {
            let tag = match (f.level(), f.category) {
                (level, Some(c)) if level != jgt_core::Severity::Other => format!("{level}/{c}"),
                (level, _) => level.to_string(),
            };
            let _ = writeln!(out, "- [{tag}] {} {}", f.location, f.problem.trim());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_parse_the_way_people_type_them() {
        for s in ["on_queue", "on-queue", "On Queue", "queue"] {
            assert_eq!(parse_column(s), Ok(TaskStatus::OnQueue), "{s}");
        }
        assert_eq!(parse_column("Done"), Ok(TaskStatus::Done));
        assert!(parse_column("doing").is_err());
        assert_eq!(parse_decision("more-rounds"), Ok(Decision::MoreRounds));
        assert_eq!(parse_decision("One more fix"), Ok(Decision::MoreRounds));
        assert_eq!(parse_decision("done"), Ok(Decision::Done));
        assert!(parse_decision("later").is_err());
        assert_eq!(parse_column("Orchestrator review"), Ok(TaskStatus::OrchestratorReview));
        assert_eq!(parse_column("orchestrator"), Ok(TaskStatus::OrchestratorReview));
        assert_eq!(parse_test("green"), Ok(TestStatus::Pass));
    }

    #[test]
    fn show_prints_the_round_only_while_it_matters() {
        let mut t = Task::new("/w", 1, "one", TaskStatus::Backlog);
        assert!(!show(&t).contains("round"));
        t.status = TaskStatus::Review;
        assert!(show(&t).contains("round      1/2"));
        t.status = TaskStatus::Backlog;
        t.review_rounds = 1;
        assert!(show(&t).contains("round      2/2"));
        t.status = TaskStatus::OrchestratorReview;
        assert!(show(&t).contains("round      2/2"));
    }
}
