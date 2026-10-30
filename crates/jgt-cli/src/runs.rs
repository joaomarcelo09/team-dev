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
use jgt_core::{Harness, HubClient, Session, api::PatchSessionRequest, client::encode_query, now_ms};
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
        /// Short title of the request.
        #[arg(long)]
        title: String,
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
                run.cost_usd += event.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
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

/// Saves a worker's result in `worker-results/` and records its end. Returns the result file.
#[allow(clippy::too_many_arguments)]
pub fn worker_finished(
    run: &Path,
    worker: &str,
    skill: &str,
    task: &str,
    ok: bool,
    secs: u64,
    cost_usd: Option<f64>,
    result: &str,
) -> Result<PathBuf> {
    let name = format!("{skill}-{}.md", jgt_core::short_id(worker));
    let dir = run.join(RESULTS);
    std::fs::create_dir_all(&dir)?;
    let state = if ok { "finished" } else { "failed" };
    let file = dir.join(&name);
    std::fs::write(&file, format!("# {skill} · {} ({state})\n\n{}\n", task.trim(), result.trim()))?;
    append(
        run,
        json!({
            "event": "worker_finished", "worker": worker, "skill": skill, "ok": ok, "secs": secs,
            "cost_usd": cost_usd, "result": format!("{RESULTS}/{name}"),
        }),
    )?;
    Ok(file)
}

/// The worker's hub session, when it registered.
async fn worker_session(client: &HubClient, worker: &str) -> Option<Session> {
    client
        .get(&format!("/v1/resolve?harness={}&external_id={}", Harness::ClaudeCode.as_str(), encode_query(worker)))
        .await
        .ok()
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
fn read_text(file: Option<&Path>, words: &[String], what: &str) -> Result<String> {
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
        RunCommand::Start { title, request_file, parent, request } => {
            let request = read_text(request_file.as_deref(), &request, "request")?;
            let client = HubClient::default();
            let coordinator = crate::workers::coordinator(&client, parent).await?;
            let project = project_dir(&coordinator)?;
            let run = create(&project, &title, &request, &coordinator.id)?;
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
            std::fs::write(run.dir.join("final-report.md"), format!("{}\n", report.trim()))?;
            append(&run.dir, json!({"event": "run_finished", "cost_usd": run.cost_usd}))?;
            let archived = archive_finished_workers(&client, &run).await;
            let running = run.workers.len() - run.workers_done;
            let mut line = format!(
                "Finished run {}: {} worker{}, ${:.2}. Report: {}. Archived {archived} worker chat{}.",
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
        worker_finished(&run.dir, "w-111111111", "coder", "fix", true, 61, Some(0.5), "done").unwrap();
        worker_finished(&run.dir, "w-222222222", "reviewer", "review", false, 5, None, "boom").unwrap();
        let loaded = load(&run.dir).unwrap();
        assert_eq!((loaded.workers.len(), loaded.workers_done, loaded.cost_usd), (2, 2, 0.5));
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
