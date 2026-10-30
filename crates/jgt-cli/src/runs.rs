//! Run records: one folder per piece of team work, under `.team/runs/<run-id>/` in the project.
//!
//! - `request.md`: what the user asked, written by `jg-team-dev run start`.
//! - `events.jsonl`: one JSON object per line (run and worker starts and ends), appended by
//!   whoever is running, so concurrent workers never rewrite each other's lines.
//! - `worker-results/<skill>-<id>.md`: each worker's final result.
//! - `final-report.md`: the Coordinator's summary, written by `jg-team-dev run finish`, which
//!   also archives the run's finished worker chats (still kept, just hidden).
//!
//! The state of a run is read back from `events.jsonl`; there is no other index.

use crate::worktree;
use anyhow::{Context, Result, bail};
use jgt_core::{
    Harness, HubClient, Session, Task,
    api::{ActorRequest, PatchSessionRequest, PatchTaskRequest},
    client::encode_query,
    now_ms,
};
use serde_json::{Value, json};
use std::{
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
};

const EVENTS: &str = "events.jsonl";
const RESULTS: &str = "worker-results";

#[derive(clap::Subcommand)]
pub enum RunCommand {
    /// Open a run for a request before starting its workers; prints the run id.
    Start {
        /// Short title of the request (default with --task-id: the task's title).
        #[arg(long, required_unless_present = "task_id")]
        title: Option<String>,
        /// Open the run for a Board task (JGT-12): the run id carries the key, the request is the
        /// task's text, and the task links to the run.
        #[arg(long)]
        task_id: Option<String>,
        /// Read the request from this file (otherwise from the remaining arguments, or stdin).
        #[arg(long)]
        request_file: Option<PathBuf>,
        /// Coordinator session id (default: the Coordinator running this command).
        #[arg(long)]
        parent: Option<String>,
        /// The request, in the user's words.
        request: Vec<String>,
    },
    /// Close a run with its final report; its finished worker chats are archived.
    Finish {
        /// Run id (default: this Coordinator's open run).
        #[arg(long)]
        run: Option<String>,
        /// Read the report from this file (otherwise from the remaining arguments, or stdin).
        #[arg(long)]
        report_file: Option<PathBuf>,
        /// Coordinator session id (default: the Coordinator running this command).
        #[arg(long)]
        parent: Option<String>,
        /// The report.
        report: Vec<String>,
    },
    /// List the runs of the project in the current folder.
    List,
}

/// A run, as read back from its `events.jsonl`.
#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub dir: PathBuf,
    pub title: String,
    pub coordinator: Option<String>,
    pub started_at: i64,
    pub finished: bool,
    /// Claude Code session ids of the workers started in this run.
    pub workers: Vec<String>,
    pub workers_done: usize,
    pub cost_usd: f64,
    /// Cost per harness (`claude-code`, …), for the final report (PRD H8).
    pub cost_by_harness: std::collections::BTreeMap<String, f64>,
}

impl Run {
    /// `claude-code $1.20 · opencode $0.30`, or empty with fewer than two harnesses.
    pub fn harness_costs(&self) -> String {
        if self.cost_by_harness.len() < 2 {
            return String::new();
        }
        self.cost_by_harness.iter().map(|(h, c)| format!("{h} ${c:.2}")).collect::<Vec<_>>().join(" · ")
    }
}

/// `.team/runs` of the project a Coordinator works in.
pub fn runs_root(project: &Path) -> PathBuf {
    project.join(".team").join("runs")
}

/// The project folder of a Coordinator: its working folder.
pub fn project_dir(coordinator: &Session) -> Result<PathBuf> {
    match coordinator.cwd.as_deref() {
        Some(cwd) => Ok(PathBuf::from(cwd)),
        None => Ok(std::env::current_dir()?),
    }
}

/// A task's run title starts with its key, so the run id carries it (`…-jgt-12-fix-navbar`).
fn task_run_title(key: &str, title: &str) -> String {
    let title = title.trim();
    match title.get(..key.len()) {
        Some(head) if head.eq_ignore_ascii_case(key) => title.to_string(),
        _ => format!("{key} {title}"),
    }
}

/// The request of a task's run: what the user wrote, and the Details if the team wrote them.
fn task_request(t: &Task) -> String {
    let mut out = format!("{} · {}\n", t.key, t.title.trim());
    if !t.description.trim().is_empty() {
        out.push_str(&format!("\n{}\n", t.description.trim()));
    }
    if let Some(details) = t.details.as_deref().filter(|d| !d.trim().is_empty()) {
        out.push_str(&format!("\n## Details\n\n{}\n", details.trim()));
    }
    out
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "run".into() } else { out }
}

fn append(dir: &Path, mut event: Value) -> Result<()> {
    event["at"] = json!(now_ms());
    let mut line = serde_json::to_string(&event)?;
    line.push('\n');
    // One write per line on an O_APPEND file: lines from concurrent workers never interleave.
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(EVENTS))?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Reads a run back from its folder; `None` when it holds no run.
pub fn load(dir: &Path) -> Option<Run> {
    let text = std::fs::read_to_string(dir.join(EVENTS)).ok()?;
    let mut run = Run {
        id: dir.file_name()?.to_string_lossy().into_owned(),
        dir: dir.to_path_buf(),
        title: String::new(),
        coordinator: None,
        started_at: 0,
        finished: false,
        workers: vec![],
        workers_done: 0,
        cost_usd: 0.0,
        cost_by_harness: Default::default(),
    };
    let mut started = false;
    for event in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let str_of = |k: &str| event.get(k).and_then(Value::as_str).map(str::to_owned);
        match event.get("event").and_then(Value::as_str) {
            Some("run_started") => {
                started = true;
                run.title = str_of("title").unwrap_or_default();
                run.coordinator = str_of("coordinator");
                run.started_at = event.get("at").and_then(Value::as_i64).unwrap_or(0);
            }
            Some("worker_started") => run.workers.extend(str_of("worker")),
            Some("worker_finished") => {
                run.workers_done += 1;
                let cost = event.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
                run.cost_usd += cost;
                // Runs recorded before harnesses were chosen per role ran on Claude Code.
                let harness = str_of("harness").unwrap_or_else(|| Harness::ClaudeCode.as_str().into());
                *run.cost_by_harness.entry(harness).or_default() += cost;
            }
            Some("run_finished") => run.finished = true,
            _ => {}
        }
    }
    started.then_some(run)
}

/// Every run of a project, oldest first.
pub fn list(project: &Path) -> Vec<Run> {
    let mut runs: Vec<Run> = std::fs::read_dir(runs_root(project))
        .map(|entries| entries.flatten().filter_map(|e| load(&e.path())).collect())
        .unwrap_or_default();
    runs.sort_by_key(|r| (r.started_at, r.id.clone()));
    runs
}

/// The run `id` of a project.
pub fn open(project: &Path, id: &str) -> Result<Run> {
    if id.is_empty() || id.contains('/') || id.starts_with('.') {
        bail!("bad run id `{id}`");
    }
    load(&runs_root(project).join(id)).with_context(|| format!("no run `{id}` in {}", runs_root(project).display()))
}

/// The newest open run of a Coordinator.
pub fn current(project: &Path, coordinator: &str) -> Option<Run> {
    list(project).into_iter().rev().find(|r| !r.finished && r.coordinator.as_deref() == Some(coordinator))
}

/// Opens a new run: `<YYYYMMDD-HHMM>-<slug>` with `request.md` and the first event.
pub fn create(project: &Path, title: &str, request: &str, coordinator: &str) -> Result<Run> {
    if let Some(root) = worktree::repo_root(project) {
        worktree::exclude_team_dir(&root)?;
    }
    let base = format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M"), slug(title));
    let root = runs_root(project);
    let mut id = base.clone();
    let mut n = 2;
    while root.join(&id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    let dir = root.join(&id);
    std::fs::create_dir_all(dir.join(RESULTS))?;
    std::fs::write(dir.join("request.md"), format!("# {}\n\n{}\n", title.trim(), request.trim()))?;
    append(&dir, json!({"event": "run_started", "title": title.trim(), "coordinator": coordinator}))?;
    load(&dir).context("writing the run record")
}

pub fn worker_started(run: &Path, worker: &str, skill: &str, task: &str, cwd: &Path) -> Result<()> {
    append(
        run,
        json!({"event": "worker_started", "worker": worker, "skill": skill, "task": task.trim(), "cwd": cwd}),
    )
}

/// A finished worker, as `events.jsonl` records it.
pub struct Finished<'a> {
    pub worker: &'a str,
    pub skill: &'a str,
    pub task: &'a str,
    pub ok: bool,
    pub secs: u64,
    pub cost_usd: Option<f64>,
    pub result: &'a str,
    /// What it ran on (PRD H8).
    pub harness: Harness,
    pub model: &'a str,
    pub effort: &'a str,
    /// Tokens in and out, when the harness reported them.
    pub tokens: (Option<i64>, Option<i64>),
}

/// Saves a worker's result in `worker-results/` and records its end. Returns the result file.
pub fn worker_finished(run: &Path, f: &Finished) -> Result<PathBuf> {
    let name = format!("{}-{}.md", f.skill, jgt_core::short_id(f.worker));
    let dir = run.join(RESULTS);
    std::fs::create_dir_all(&dir)?;
    let state = if f.ok { "finished" } else { "failed" };
    std::fs::write(dir.join(&name), format!("# {} · {} ({state})\n\n{}\n", f.skill, f.task.trim(), f.result.trim()))?;
    append(
        run,
        json!({
            "event": "worker_finished", "worker": f.worker, "skill": f.skill, "ok": f.ok, "secs": f.secs,
            "cost_usd": f.cost_usd, "result": format!("{RESULTS}/{name}"),
            "harness": f.harness, "model": f.model, "effort": f.effort,
            "tokens_in": f.tokens.0, "tokens_out": f.tokens.1,
        }),
    )?;
    Ok(dir.join(name))
}

/// The worker's hub session, when it registered, on whichever harness it ran.
async fn worker_session(client: &HubClient, worker: &str) -> Option<Session> {
    for harness in [Harness::ClaudeCode, Harness::Opencode, Harness::Antigravity] {
        let path = format!("/v1/resolve?harness={}&external_id={}", harness.as_str(), encode_query(worker));
        if let Ok(s) = client.get::<Session>(&path).await {
            return Some(s);
        }
    }
    None
}

pub async fn archive(client: &HubClient, session: &Session) -> Result<()> {
    let _: Session = client
        .patch(
            &format!("/v1/sessions/{}?source=agent", session.id),
            &PatchSessionRequest { archived: Some(true), ..Default::default() },
        )
        .await?;
    Ok(())
}

/// Archives the run's workers that are done; the ones still running archive themselves when
/// they finish (see `workers::run`). Returns how many were archived.
pub async fn archive_finished_workers(client: &HubClient, run: &Run) -> usize {
    let mut n = 0;
    for worker in &run.workers {
        if let Some(s) = worker_session(client, worker).await
            && !s.archived
            && !s.status.is_connected()
            && archive(client, &s).await.is_ok()
        {
            n += 1;
        }
    }
    n
}

/// Text from a file, else the words given, else stdin.
pub(crate) fn read_text(file: Option<&Path>, words: &[String], what: &str) -> Result<String> {
    let text = if let Some(file) = file {
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?
    } else if !words.is_empty() {
        words.join(" ")
    } else if !std::io::stdin().is_terminal() {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    } else {
        String::new()
    };
    if text.trim().is_empty() {
        bail!("the {what} is empty: pass it as arguments, with a file, or on stdin");
    }
    Ok(text)
}

pub async fn command(cmd: RunCommand) -> Result<()> {
    match cmd {
        RunCommand::Start { title, task_id, request_file, parent, request } => {
            let client = HubClient::default();
            let coordinator = crate::workers::coordinator(&client, parent).await?;
            let project = project_dir(&coordinator)?;
            let task: Option<Task> = match &task_id {
                Some(key) => {
                    let path = format!("/v1/tasks/{}?workspace={}", encode_query(key), encode_query(&project.to_string_lossy()));
                    Some(client.get(&path).await.with_context(|| format!("reading task {key}"))?)
                }
                None => None,
            };
            let (title, request) = match &task {
                Some(t) => {
                    let request = if request_file.is_some() || !request.is_empty() {
                        read_text(request_file.as_deref(), &request, "request")?
                    } else {
                        task_request(t)
                    };
                    (task_run_title(&t.key, title.as_deref().unwrap_or(&t.title)), request)
                }
                None => (title.unwrap_or_default(), read_text(request_file.as_deref(), &request, "request")?),
            };
            let run = create(&project, &title, &request, &coordinator.id)?;
            if let Some(t) = &task {
                let mut req = PatchTaskRequest::new(ActorRequest::Session { session: coordinator.id.clone() });
                req.run_id = Some(run.id.clone());
                let _: Task = client
                    .patch(&format!("/v1/tasks/{}", t.id), &req)
                    .await
                    .with_context(|| format!("linking run {} to {}", run.id, t.key))?;
            }
            println!(
                "Opened run {} in {}. Workers you start now are recorded in it; close it with \
                 `jg-team-dev run finish --report-file <file>`.",
                run.id,
                run.dir.display()
            );
        }
        RunCommand::Finish { run, report_file, parent, report } => {
            let report = read_text(report_file.as_deref(), &report, "report")?;
            let client = HubClient::default();
            let coordinator = crate::workers::coordinator(&client, parent).await?;
            let project = project_dir(&coordinator)?;
            let run = match run {
                Some(id) => open(&project, &id)?,
                None => current(&project, &coordinator.id).context("no open run for this Coordinator; pass --run <id>")?,
            };
            if run.finished {
                bail!("run {} is already finished", run.id);
            }
            let costs = match run.harness_costs() {
                h if h.is_empty() => String::new(),
                h => format!("\n\nCost per harness: {h}.\n"),
            };
            std::fs::write(run.dir.join("final-report.md"), format!("{}\n{costs}", report.trim()))?;
            append(&run.dir, json!({"event": "run_finished", "cost_usd": run.cost_usd, "cost_by_harness": run.cost_by_harness}))?;
            let archived = archive_finished_workers(&client, &run).await;
            let running = run.workers.len() - run.workers_done;
            let by_harness = match run.harness_costs() {
                h if h.is_empty() => String::new(),
                h => format!(" ({h})"),
            };
            let mut line = format!(
                "Finished run {}: {} worker{}, ${:.2}{by_harness}. Report: {}. Archived {archived} worker chat{}.",
                run.id,
                run.workers.len(),
                if run.workers.len() == 1 { "" } else { "s" },
                run.cost_usd,
                run.dir.join("final-report.md").display(),
                if archived == 1 { "" } else { "s" },
            );
            if running > 0 {
                line.push_str(&format!(" {running} still running; they are archived when they finish."));
            }
            println!("{line}");
        }
        RunCommand::List => {
            let project = std::env::current_dir()?;
            let runs = list(&project);
            if runs.is_empty() {
                println!("No runs in {}.", runs_root(&project).display());
            }
            for r in runs {
                let state = if r.finished { "finished" } else { "open" };
                println!(
                    "{:<40} {:<8} {}/{} workers  ${:<6.2} {}",
                    r.id,
                    state,
                    r.workers_done,
                    r.workers.len(),
                    r.cost_usd,
                    r.title
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jgt-runs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn task_runs_carry_the_key() {
        assert_eq!(task_run_title("JGT-12", "fix navbar"), "JGT-12 fix navbar");
        assert_eq!(task_run_title("JGT-12", "jgt-12 fix navbar"), "jgt-12 fix navbar");
        assert_eq!(slug(&task_run_title("JGT-12", "Fix the navbar")), "jgt-12-fix-the-navbar");
        let mut t = Task::new("/w", 12, "Fix navbar", jgt_core::TaskStatus::OnQueue);
        t.description = "It is red.".into();
        t.details = Some("Change the CSS.".into());
        assert_eq!(task_request(&t), "JGT-12 · Fix navbar\n\nIt is red.\n\n## Details\n\nChange the CSS.\n");
    }

    #[test]
    fn slugs_are_short_and_plain() {
        assert_eq!(slug("Fix the navbar color!"), "fix-the-navbar-color");
        assert_eq!(slug("  ---  "), "run");
        assert!(slug(&"a".repeat(100)).len() <= 40);
    }

    #[test]
    fn a_run_records_its_workers_and_ends() {
        let dir = project("record");
        let run = create(&dir, "Fix navbar", "Make it blue", "coord-1").unwrap();
        assert!(run.id.ends_with("-fix-navbar"));
        assert_eq!(std::fs::read_to_string(run.dir.join("request.md")).unwrap(), "# Fix navbar\n\nMake it blue\n");
        assert_eq!(current(&dir, "coord-1").unwrap().id, run.id);
        assert!(current(&dir, "coord-2").is_none(), "runs belong to one Coordinator");

        worker_started(&run.dir, "w-111111111", "coder", "fix", &dir).unwrap();
        worker_started(&run.dir, "w-222222222", "reviewer", "review", &dir).unwrap();
        let finished = |worker, skill, ok, cost, result, harness| Finished {
            worker,
            skill,
            task: "fix",
            ok,
            secs: 61,
            cost_usd: cost,
            result,
            harness,
            model: "m",
            effort: "low",
            tokens: (Some(10), Some(2)),
        };
        worker_finished(&run.dir, &finished("w-111111111", "coder", true, Some(0.5), "done", Harness::Opencode)).unwrap();
        let path =
            worker_finished(&run.dir, &finished("w-222222222", "reviewer", false, Some(0.25), "boom", Harness::ClaudeCode))
                .unwrap();
        assert_eq!(path, run.dir.join("worker-results/reviewer-w-222222.md"));
        let loaded = load(&run.dir).unwrap();
        assert_eq!((loaded.workers.len(), loaded.workers_done, loaded.cost_usd), (2, 2, 0.75));
        assert_eq!(loaded.harness_costs(), "claude-code $0.25 · opencode $0.50");
        let events = std::fs::read_to_string(run.dir.join(EVENTS)).unwrap();
        let last: Value = serde_json::from_str(events.lines().last().unwrap()).unwrap();
        assert_eq!((last["harness"].as_str(), last["model"].as_str(), last["tokens_in"].as_i64()), (Some("claude-code"), Some("m"), Some(10)));
        let result = std::fs::read_to_string(run.dir.join("worker-results/coder-w-111111.md")).unwrap();
        assert!(result.starts_with("# coder · fix (finished)"));

        append(&run.dir, json!({"event": "run_finished"})).unwrap();
        assert!(load(&run.dir).unwrap().finished);
        assert!(current(&dir, "coord-1").is_none());

        // The same title in the same minute gets its own folder.
        let again = create(&dir, "Fix navbar", "again", "coord-1").unwrap();
        assert_ne!(again.id, run.id);
        assert_eq!(list(&dir).len(), 2);
        assert!(open(&dir, "../x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
