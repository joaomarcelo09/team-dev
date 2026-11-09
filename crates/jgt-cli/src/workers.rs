//! Workers: fresh `claude -p` sessions a Coordinator starts with one skill and a brief.
//!
//! `jg-team-dev worker` returns at once; a detached `jg-team-dev worker-run` runs the session and
//! relays its result to the Coordinator through the hub, like a message from the user. That way
//! the result reaches the Coordinator even while it waits for the user in listen mode, and wakes
//! it when it went to sleep.

use crate::{runs, snapshot, team::prepare_worker_files, worktree};
use anyhow::{Context, Result, bail};
use jgt_core::{
    Harness, HubClient, Session, SessionStatus, Task, TaskEventKind,
    api::{ActorRequest, PatchTaskRequest, SessionsResponse, TaskEventRequest},
    client::encode_query,
    config::{Config, EFFORT_LEVELS, SessionSettings},
    harness::{self, WorkerLaunch},
    paths, procs,
    team::{self, PARENT_ENV, ROLE_ENV, TASK_ENV, WORKER_SKILLS},
};
use serde_json::Value;
use std::{
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

#[derive(clap::Args)]
pub struct WorkerArgs {
    /// planner, coder, reviewer or git-coder.
    pub skill: String,
    /// Short task title, shown as the worker's chat title (`<skill> · <task>`).
    #[arg(long)]
    pub task: String,
    /// Read the brief from this file (otherwise from the remaining arguments, or stdin).
    #[arg(long)]
    pub brief_file: Option<PathBuf>,
    /// Folder the worker works in (default: the current folder).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Work in the git worktree `.team/worktrees/<name>` (branch `jgt/<name>`), created from HEAD
    /// if it does not exist. Coders need this in a git repository; reviewers can point at a
    /// coder's worktree with the same name.
    #[arg(long, conflicts_with = "cwd")]
    pub worktree: Option<String>,
    /// Let a coder write the main worktree directly (only after warning the user).
    #[arg(long, conflicts_with = "worktree")]
    pub main_worktree: bool,
    /// Coordinator session id (default: the Coordinator running this command).
    #[arg(long)]
    pub parent: Option<String>,
    /// Record the worker in this run (default: the Coordinator's open run, or a new one).
    #[arg(long)]
    pub run: Option<String>,
    /// The Board task the worker works on (JGT-12): the worktree defaults to `jgt-12`, the run
    /// to the task's, the worker reads the task with `task_get`, and the card shows it (K11, K14).
    #[arg(long)]
    pub task_id: Option<String>,
    /// Model for this worker only, over the config's (E1b).
    #[arg(long)]
    pub model: Option<String>,
    /// Effort for this worker only (low, medium, high, xhigh, max), over the config's (E1b).
    #[arg(long)]
    pub effort: Option<String>,
    /// The brief.
    pub brief: Vec<String>,
}

#[derive(clap::Args)]
pub struct WorkerRunArgs {
    #[arg(long)]
    pub skill: String,
    #[arg(long)]
    pub task: String,
    #[arg(long)]
    pub brief_file: PathBuf,
    #[arg(long)]
    pub cwd: PathBuf,
    #[arg(long)]
    pub parent: String,
    #[arg(long)]
    pub session_id: String,
    #[arg(long)]
    pub run_dir: PathBuf,
    /// The Board task (hub id), handed to the worker as `JGT_TASK`.
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
}

/// The settings a worker runs with: the role's from the config, moved to the role's fallback
/// harness when its own cannot run here, then this worker's `--model` and `--effort` over them
/// (they apply to the harness it actually runs on, which `jg-team-dev worker` prints). The notes
/// say what was replaced, for the worker's chat and the Coordinator.
pub(crate) fn worker_settings(
    config: &Config,
    skill: &str,
    cwd: &Path,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(SessionSettings, Vec<String>)> {
    pick_settings(config.team.for_skill_checked(skill, cwd), skill, model, effort, |h| harness_ready(config, h))
}

fn pick_settings(
    (mut settings, mut notes): (SessionSettings, Vec<String>),
    skill: &str,
    model: Option<&str>,
    effort: Option<&str>,
    ready: impl Fn(Harness) -> std::result::Result<(), String>,
) -> Result<(SessionSettings, Vec<String>)> {
    if let Err(why) = ready(settings.harness) {
        let Some(fallback) = settings.on_fallback() else {
            bail!("{skill}: {why}, and the role has no fallback harness");
        };
        notes.push(format!("{skill}: {why}; using the fallback {}", fallback.describe(skill)));
        settings = fallback;
        if let Err(why) = ready(settings.harness) {
            bail!("{skill}: the fallback cannot run either: {why}");
        }
    }
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        settings.model = m.to_string();
    }
    if let Some(e) = effort.map(str::trim).filter(|e| !e.is_empty()) {
        if !EFFORT_LEVELS.contains(&e) {
            bail!("unknown effort `{e}`; use one of: {}", EFFORT_LEVELS.join(", "));
        }
        settings.effort = jgt_core::harness::nearest_effort(settings.harness, e).unwrap_or(e).to_string();
    }
    Ok((settings, notes))
}

/// Whether a worker can run on `harness` here.
fn harness_ready(config: &Config, harness: Harness) -> std::result::Result<(), String> {
    let adapter = harness::adapter(harness).ok_or_else(|| format!("{} cannot run workers", harness.label()))?;
    adapter.find(config).map(|_| ()).map_err(|e| format!("{} is not installed ({e})", harness.label()))
}

/// The worktree of a worker on a task: the one asked for, else the task key in lowercase
/// (`JGT-12` → `.team/worktrees/jgt-12`, branch `jgt/jgt-12`), unless it works elsewhere.
fn task_worktree(args: &WorkerArgs, task: Option<&Task>) -> Option<String> {
    match (&args.worktree, task) {
        (Some(name), _) => Some(name.clone()),
        (None, Some(t)) if args.cwd.is_none() && !args.main_worktree => Some(t.key.to_lowercase()),
        _ => None,
    }
}

/// `/v1/tasks/<task>[<tail>]?workspace=<the Coordinator's folder>`.
fn task_path(parent: &Session, task: &str, tail: &str) -> Result<String> {
    let workspace = parent.cwd.as_deref().context("the Coordinator has no folder")?;
    let workspace = jgt_core::team::project_folder(workspace);
    Ok(format!("/v1/tasks/{}{tail}?workspace={}", encode_query(task), encode_query(workspace)))
}

/// Logs a worker's start or finish on its task, so the card shows who is on it.
async fn log_worker(client: &HubClient, actor: &str, task: &str, kind: TaskEventKind, body: String, meta: Value) -> Result<()> {
    let req = TaskEventRequest { actor: ActorRequest::Session { session: actor.to_string() }, kind, body, meta };
    let _: jgt_core::TaskEvent = client.post(&format!("/v1/tasks/{task}/events"), &req).await?;
    Ok(())
}

/// A random version 4 UUID, used as the worker's Claude Code session id.
fn new_uuid() -> Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// The Coordinator that runs this command: the hub session of the Claude Code process above us.
pub(crate) async fn coordinator(client: &HubClient, explicit: Option<String>) -> Result<Session> {
    let session: Session = match explicit {
        Some(id) => client.get(&format!("/v1/sessions/{id}")).await?,
        None => {
            let pid = procs::harness_pid().context("not started from a Claude Code session; pass --parent")?;
            client
                .get(&format!("/v1/resolve?pid={pid}"))
                .await
                .context("this Claude Code session is not a jg-team-dev Coordinator; pass --parent")?
        }
    };
    if !session.is_coordinator() {
        bail!("{}", session.not_coordinator_error());
    }
    Ok(session)
}

fn read_brief(args: &WorkerArgs) -> Result<String> {
    let brief = if let Some(file) = &args.brief_file {
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?
    } else if !args.brief.is_empty() {
        args.brief.join(" ")
    } else if !std::io::stdin().is_terminal() {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    } else {
        String::new()
    };
    if brief.trim().is_empty() {
        bail!("the brief is empty: pass it as arguments, with --brief-file, or on stdin");
    }
    Ok(brief)
}

/// `jg-team-dev worker`: starts a worker in the background and returns at once.
pub async fn start(args: WorkerArgs) -> Result<()> {
    if !WORKER_SKILLS.contains(&args.skill.as_str()) {
        bail!("unknown skill `{}`; use one of: {}", args.skill, WORKER_SKILLS.join(", "));
    }
    let brief = read_brief(&args)?;
    if let Some(e) = args.effort.as_deref().filter(|e| !EFFORT_LEVELS.contains(e)) {
        bail!("unknown effort `{e}`; use one of: {}", EFFORT_LEVELS.join(", "));
    }
    let client = HubClient::default();
    let parent = coordinator(&client, args.parent.clone()).await?;
    let task: Option<Task> = match &args.task_id {
        Some(key) => Some(client.get(&task_path(&parent, key, "")?).await.with_context(|| format!("reading task {key}"))?),
        None => None,
    };
    let worktree_name = task_worktree(&args, task.as_ref());
    let cwd = match &args.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir()?,
    };
    let cwd = cwd.canonicalize().with_context(|| format!("{} does not exist", cwd.display()))?;
    let cwd = match &worktree_name {
        Some(name) => worktree::ensure(&cwd, name)?,
        None if args.skill == "coder" && !args.main_worktree && worktree::repo_root(&cwd).is_some() => bail!(
            "coders work in a git worktree: pass --worktree <name>. To write the main worktree \
             directly, warn the user first, then pass --main-worktree."
        ),
        None => cwd,
    };

    // A coder of a fix round: save what the last verification checked first, so the next one
    // reviews only the fix (E4). Done by hand too with `jg-team-dev snapshot`.
    if let Some(t) = task.as_ref().filter(|t| args.skill == "coder" && t.review_rounds > 0 && worktree_name.is_some()) {
        match snapshot::latest(&cwd, &t.key) {
            Ok(Some((round, _))) if round >= t.review_rounds => {}
            _ => match snapshot::save(&cwd, &t.key, t.review_rounds) {
                Ok(name) => println!("Saved {name}: the next verification reviews only the fix."),
                Err(e) => eprintln!("jg-team-dev: saving the snapshot of {}: {e:#}", t.key),
            },
        }
    }
    let project = runs::project_dir(&parent)?;
    let run = match args.run.as_ref().or(task.as_ref().and_then(|t| t.run_id.as_ref())) {
        Some(id) => {
            let run = runs::open(&project, id)?;
            if run.finished {
                bail!("run {} is finished; open a new one with `jg-team-dev run start`", run.id);
            }
            run
        }
        None => match runs::current(&project, &parent.id) {
            Some(run) => run,
            None => runs::create(
                &project,
                &args.task,
                &format!("(Opened by the first worker; no `jg-team-dev run start`.)\n\n{brief}"),
                &parent.id,
            )?,
        },
    };

    let id = new_uuid()?;
    let briefs = paths::data_dir().join("briefs");
    std::fs::create_dir_all(&briefs)?;
    let brief_file = briefs.join(format!("{id}.md"));
    std::fs::write(&brief_file, &brief)?;
    let logs = paths::data_dir().join("logs");
    std::fs::create_dir_all(&logs)?;
    let log = std::fs::File::create(logs.join(format!("worker-{id}.log")))?;

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("worker-run")
        .args(["--skill", &args.skill, "--task", &args.task, "--parent", &parent.id, "--session-id", &id])
        .arg("--brief-file")
        .arg(&brief_file)
        .arg("--cwd")
        .arg(&cwd)
        .arg("--run-dir")
        .arg(&run.dir)
        .args(task.iter().flat_map(|t| ["--task-id", t.id.as_str()]))
        .args(args.model.iter().flat_map(|m| ["--model", m.as_str()]))
        .args(args.effort.iter().flat_map(|e| ["--effort", e.as_str()]))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Its own process group: it outlives this command and the Coordinator's Bash call.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().context("starting the worker")?;
    runs::worker_started(&run.dir, &id, &args.skill, &args.task, &cwd)?;
    if let Some(t) = &task {
        if t.worktree.is_none()
            && let Some(name) = &worktree_name
        {
            let mut req = PatchTaskRequest::new(ActorRequest::Session { session: parent.id.clone() });
            req.worktree = Some(name.clone());
            if let Err(e) = client.patch::<_, Task>(&format!("/v1/tasks/{}", t.id), &req).await {
                eprintln!("jg-team-dev: linking the worktree to {}: {e}", t.key);
            }
        }
        let meta = serde_json::json!({ "worker": id, "skill": args.skill, "run": run.id });
        if let Err(e) = log_worker(&client, &parent.id, &t.id, TaskEventKind::WorkerStarted, format!("{} started", args.skill), meta).await {
            eprintln!("jg-team-dev: logging the worker on {}: {e:#}", t.key);
        }
    }

    println!(
        "Started {} worker {} for \"{}\" in run {}. It reports progress in its own chat; its result will \
         reach you as a message.",
        args.skill,
        jgt_core::short_id(&id),
        args.task.trim(),
        run.id
    );
    match worker_settings(&Config::load(), &args.skill, &cwd, args.model.as_deref(), args.effort.as_deref()) {
        Ok((settings, notes)) => {
            println!("It runs on {}.", settings.describe(&args.skill));
            for note in notes {
                println!("Note: {note}");
            }
        }
        Err(e) => println!("Warning: {e:#}. It will fail; fix the config and start it again."),
    }
    Ok(())
}

/// Lines a relayed result may take (E6); the full text stays in the run and the worker's chat.
const DIGEST_LINES: usize = 10;

/// Longest one-line summary in a digest.
const SUMMARY_MAX: usize = 200;

/// The last ```json block of a worker's reply: its result object.
fn final_json(result: &str) -> Option<Value> {
    let start = result.rfind("```json")?;
    let body = &result[start + "```json".len()..];
    let end = body.find("```").unwrap_or(body.len());
    serde_json::from_str(body[..end].trim()).ok()
}

fn clip(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    format!("{}…", line.chars().take(max - 1).collect::<String>())
}

/// Counts of a result's findings: (critical, high, other). `blocking` counts as high.
fn finding_counts(findings: &[Value]) -> (usize, usize, usize) {
    let mut counts = (0, 0, 0);
    for f in findings {
        let severity = f.get("severity").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        if f.get("critical").and_then(Value::as_bool) == Some(true) || severity == "critical" {
            counts.0 += 1;
        } else if matches!(severity.as_str(), "high" | "blocking") {
            counts.1 += 1;
        } else {
            counts.2 += 1;
        }
    }
    counts
}

/// What the Coordinator gets for a finished worker (E6): at most `DIGEST_LINES` lines with the
/// status or decision, a one-line summary, counts, and the path to the full result.
fn digest(head: &str, result: &str, notes: &[String], file: Option<&Path>) -> String {
    let mut lines = vec![head.to_string()];
    match final_json(result) {
        Some(v) => {
            let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
            let len = |k: &str| v.get(k).and_then(Value::as_array).map(Vec::len);
            let state = [field("status"), field("decision")].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            if !state.is_empty() {
                lines.push(format!("Status: {state}"));
            }
            let summary = field("summary").or_else(|| {
                v.get("notes").and_then(Value::as_array).and_then(|n| n.first()).and_then(Value::as_str).map(str::to_owned)
            });
            if let Some(summary) = summary.filter(|s| !s.trim().is_empty()) {
                lines.push(format!("Summary: {}", clip(&summary, SUMMARY_MAX)));
            }
            let mut counts = vec![];
            if let Some(n) = len("filesChanged") {
                counts.push(format!("{n} files changed"));
            }
            if let Some(status) = v.get("tests").and_then(|t| t.get("status")).and_then(Value::as_str) {
                counts.push(format!("tests: {status}"));
            }
            if let Some(findings) = v.get("findings").and_then(Value::as_array) {
                let (critical, high, other) = finding_counts(findings);
                counts.push(format!("findings: {critical} critical, {high} high, {other} other"));
            }
            if let Some(n) = len("commands") {
                counts.push(format!("{n} checks run, {} failed", len("failures").unwrap_or(0)));
            }
            if !counts.is_empty() {
                lines.push(counts.join(" · "));
            }
        }
        None => {
            let body: Vec<&str> = result.lines().filter(|l| !l.trim().is_empty()).collect();
            // Room left beside the head, the "…", the notes and the path.
            let room = DIGEST_LINES - 3 - notes.len().min(2);
            lines.extend(body.iter().take(room).map(|l| clip(l, SUMMARY_MAX)));
            if body.len() > room {
                lines.push("…".into());
            }
        }
    }
    lines.extend(notes.iter().take(2).map(|n| format!("Note: {}", clip(n, SUMMARY_MAX))));
    lines.truncate(DIGEST_LINES - 1);
    match file {
        Some(path) => lines.push(format!("Full result: {}", path.display())),
        None => lines.push("Full result: in the worker's chat.".into()),
    }
    lines.join("\n")
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

// ── usage limit (E7) ────────────────────────────────────────────────────────

/// What a worker stopped by the harness's usage limit says (PRD E7, Q4): the text Claude Code
/// ends a session with, "You've hit your session limit · resets 5pm (America/Sao_Paulo)".
const USAGE_LIMIT_TEXT: &str = "hit your session limit";

/// A worker that stopped at the usage limit: when the limit resets.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageLimit {
    /// The reset as the user reads it, in the zone the harness named: `17:00`.
    pub resets: String,
    /// The reset in ms since the epoch, when the time could be read.
    pub until: Option<i64>,
}

/// The UTC offset of the IANA zone `zone` now (`date +%z` with `TZ` set), or of the local zone.
fn zone_offset(zone: Option<&str>) -> Option<chrono::FixedOffset> {
    let Some(zone) = zone.filter(|z| z.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c))) else {
        return Some(*chrono::Local::now().offset());
    };
    let out = Command::new("date").arg("+%z").env("TZ", zone).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    let sign = if text.starts_with('-') { -1 } else { 1 };
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    let (h, m): (i32, i32) = (digits.get(..2)?.parse().ok()?, digits.get(2..4)?.parse().ok()?);
    chrono::FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

/// `5pm`, `5:30pm`, `17:00` → (hour, minute).
fn parse_clock(text: &str) -> Option<(u32, u32)> {
    let t = text.trim().to_ascii_lowercase();
    let (body, pm) = match (t.strip_suffix("pm"), t.strip_suffix("am")) {
        (Some(b), _) => (b.trim(), Some(true)),
        (_, Some(b)) => (b.trim(), Some(false)),
        _ => (t.as_str(), None),
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (body.parse::<u32>().ok()?, 0),
    };
    let h = match pm {
        Some(true) if h < 12 => h + 12,
        Some(false) if h == 12 => 0,
        _ => h,
    };
    (h < 24 && m < 60).then_some((h, m))
}

/// Reads a usage-limit death from a failed worker's result. Both must hold (Q4): the run is an
/// error and its text names the limit.
fn usage_limit(
    result: &str,
    is_error: bool,
    now: chrono::DateTime<chrono::Utc>,
    offset_of: impl Fn(Option<&str>) -> Option<chrono::FixedOffset>,
) -> Option<UsageLimit> {
    if !is_error || !result.to_ascii_lowercase().contains(USAGE_LIMIT_TEXT) {
        return None;
    }
    let lower = result.to_ascii_lowercase();
    let after = &result[lower.find("resets")? + "resets".len()..];
    let line = after.lines().next().unwrap_or("").trim();
    let (clock, zone) = match line.split_once('(') {
        Some((clock, rest)) => (clock.trim(), rest.split(')').next().map(str::trim)),
        None => (line, None),
    };
    let Some((h, m)) = parse_clock(clock) else {
        return Some(UsageLimit { resets: clock.to_string(), until: None });
    };
    let resets = format!("{h:02}:{m:02}");
    let until = offset_of(zone).and_then(|offset| {
        let local = now.with_timezone(&offset);
        let mut at = local.date_naive().and_hms_opt(h, m, 0)?.and_local_timezone(offset).single()?;
        if at <= local {
            at += chrono::Duration::days(1);
        }
        Some(at.timestamp_millis())
    });
    Some(UsageLimit { resets, until })
}

/// The brief for the fresh worker that resumes after the limit (E7 step 3): the original brief,
/// what the stopped worker left in the worktree, and the resume line.
fn resume_brief(brief: &str, cwd: &Path) -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
            .unwrap_or_else(|| "(not a git worktree)".into())
    };
    format!(
        "Resume: the previous worker stopped mid-task at the usage limit. Keep what is there; finish the rest; \
         do not restart.\n\n## What is in the worktree now\n\n`git status --short`:\n\n```\n{}\n```\n\n\
         `git diff --stat`:\n\n```\n{}\n```\n\n## The original brief\n\n{}\n",
        git(&["status", "--short"]),
        git(&["diff", "--stat"]),
        brief.trim()
    )
}

/// Registers a worker whose harness has no session-start hook, so its chat exists and our MCP
/// server finds it through `JGT_SESSION` (its `external_id`).
async fn register(client: &HubClient, args: &WorkerRunArgs, settings: &SessionSettings, title: &str, pid: Option<u32>) -> Option<Session> {
    let req = jgt_core::api::RegisterRequest {
        harness: settings.harness,
        external_id: args.session_id.clone(),
        cwd: Some(args.cwd.to_string_lossy().into_owned()),
        title: Some(title.to_string()),
        pid: pid.map(i64::from),
        transcript_path: None,
        source: Some("startup".into()),
        role: Some(args.skill.clone()),
        parent: Some(args.parent.clone()),
    };
    match client.post::<_, Session>("/v1/sessions/register", &req).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("jg-team-dev: registering the worker: {e}");
            None
        }
    }
}

/// Records the worker's model and effort on its session once it registered (Claude Code
/// registers itself from its hook a moment after it starts).
async fn record_settings(client: HubClient, harness: Harness, external_id: String, settings: SessionSettings) {
    let path = format!("/v1/resolve?harness={}&external_id={}", harness.as_str(), encode_query(&external_id));
    for _ in 0..60 {
        if let Ok(s) = client.get::<Session>(&path).await {
            let req = jgt_core::api::PatchSessionRequest {
                model: Some(settings.model.clone()),
                effort: Some(settings.effort.clone()),
                ..Default::default()
            };
            let _: Result<Session, _> = client.patch(&format!("/v1/sessions/{}?source=agent", s.id), &req).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Where `worker-run` reports a worker that could not start: the Coordinator and the task.
trait StartReport {
    async fn relay(&self, text: &str) -> Result<()>;
    async fn log(&self, task: &str, body: String, meta: Value) -> Result<()>;
}

struct HubReport<'a> {
    client: &'a HubClient,
    parent: &'a str,
}

impl StartReport for HubReport<'_> {
    async fn relay(&self, text: &str) -> Result<()> {
        team::relay(self.client, self.parent, None, text).await.map_err(anyhow::Error::msg)
    }

    async fn log(&self, task: &str, body: String, meta: Value) -> Result<()> {
        log_worker(self.client, self.parent, task, TaskEventKind::WorkerFinished, body, meta).await
    }
}

/// Tells the Coordinator, and the task's log when there is one, that the worker could not start,
/// so it never waits for a result that will not come.
async fn could_not_start(report: &impl StartReport, args: &WorkerRunArgs, title: &str, err: &anyhow::Error) {
    if let Some(task) = &args.task_id {
        let meta = serde_json::json!({ "worker": args.session_id, "skill": args.skill, "ok": false, "error": format!("{err:#}") });
        if let Err(e) = report.log(task, format!("{} could not start: {err:#}", args.skill), meta).await {
            eprintln!("jg-team-dev: logging the failed start on {task}: {e:#}");
        }
    }
    let text = format!("[jg-team-dev] Worker **{title}** could not start: {err:#}");
    if let Err(e) = report.relay(&text).await {
        eprintln!("jg-team-dev: telling the Coordinator the worker could not start: {e:#}");
    }
    let _ = std::fs::remove_file(&args.brief_file);
}

/// A worker that started: what it runs with and its process.
struct Started {
    settings: SessionSettings,
    notes: Vec<String>,
    adapter: &'static dyn harness::HarnessAdapter,
    brief: String,
    child: tokio::process::Child,
}

/// Resolves the worker's settings, writes what its harness loads, and starts it.
fn start_worker(config: &Config, args: &WorkerRunArgs, title: &str) -> Result<Started> {
    let (settings, notes) = worker_settings(config, &args.skill, &args.cwd, args.model.as_deref(), args.effort.as_deref())?;
    let adapter = harness::adapter(settings.harness).context("no adapter for this harness")?;
    let files = prepare_worker_files(settings.harness)?;
    let program = adapter.find(config).map_err(anyhow::Error::msg)?;
    let brief = std::fs::read_to_string(&args.brief_file)
        .with_context(|| format!("reading the brief {}", args.brief_file.display()))?;
    let exe = std::env::current_exe()?;
    let mut env = vec![(ROLE_ENV.to_string(), args.skill.clone()), (PARENT_ENV.to_string(), args.parent.clone())];
    env.extend(args.task_id.iter().map(|t| (TASK_ENV.to_string(), t.clone())));
    let plan = adapter
        .worker(&WorkerLaunch {
            program,
            exe,
            files: &files,
            skill: &args.skill,
            title,
            brief: &brief,
            brief_file: &args.brief_file,
            settings: &settings,
            cwd: &args.cwd,
            session_id: &args.session_id,
            env,
        })
        .map_err(anyhow::Error::msg)?;
    let mut cmd = tokio::process::Command::new(&plan.program);
    cmd.args(&plan.args)
        .envs(plan.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = &plan.cwd {
        cmd.current_dir(dir);
    }
    if let Some(extra) = &plan.path_prepend {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(std::iter::once(extra.clone()).chain(std::env::split_paths(&path)))?;
        cmd.env("PATH", joined);
    }
    let child = cmd.spawn().with_context(|| format!("running {}", plan.program.display()))?;
    Ok(Started { settings, notes, adapter, brief, child })
}

/// `jg-team-dev worker-run` (detached): runs the worker and relays its result.
pub async fn run(args: WorkerRunArgs) -> Result<()> {
    let config = Config::load();
    let client = HubClient::default();
    let title = team::worker_title(&args.skill, &args.task);
    let started = Instant::now();
    // Never hang: any failure to start reaches the Coordinator at once.
    let Started { settings, notes, adapter, brief, child } = match start_worker(&config, &args, &title) {
        Ok(s) => s,
        Err(e) => {
            could_not_start(&HubReport { client: &client, parent: &args.parent }, &args, &title, &e).await;
            return Err(e);
        }
    };
    // A harness without a session-start hook: register the worker ourselves (PRD Part B).
    let registered = if adapter.caps().registers_itself {
        None
    } else {
        register(&client, &args, &settings, &title, child.id()).await
    };
    // Its chat header and Team row show what it runs on (H7).
    tokio::spawn(record_settings(client.clone(), settings.harness, args.session_id.clone(), settings.clone()));
    let waited = child.wait_with_output();
    let out = match adapter.caps().run_timeout {
        Some(limit) => match tokio::time::timeout(limit, waited).await {
            Ok(out) => out,
            // Never hang: the child is killed when its handle drops.
            Err(_) => Err(std::io::Error::other(format!("stopped after {}m without finishing", limit.as_secs() / 60))),
        },
        None => waited.await,
    };
    let (out_ok, stdout, stderr, status) = match out {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            o.status.to_string(),
        ),
        Err(e) => (false, String::new(), e.to_string(), e.to_string()),
    };
    let elapsed = started.elapsed().as_secs();
    let outcome = adapter.parse_result(&stdout, &stderr, out_ok);
    let (ok, result, cost) = (outcome.ok, outcome.text.clone(), outcome.cost_usd);
    let limit = usage_limit(&result, !ok, chrono::Utc::now(), zone_offset);
    if !ok && limit.is_none() && result.to_ascii_lowercase().contains("limit") {
        // Q4: a failure that mentions a limit but does not match is logged, to fix the match.
        eprintln!("jg-team-dev: worker failed with an unmatched limit text: {}", clip(&result, 300));
    }
    // The fresh worker that resumes gets this brief (E7): saved in the run before ours goes.
    let resume = match &limit {
        Some(_) => {
            let path = args.run_dir.join("briefs").join(format!("resume-{}-{}.md", args.skill, jgt_core::short_id(&args.session_id)));
            let written = path.parent().map(std::fs::create_dir_all).transpose().and_then(|_| {
                std::fs::write(&path, resume_brief(&brief, &args.cwd))
            });
            match written {
                Ok(()) => Some(path),
                Err(e) => {
                    eprintln!("jg-team-dev: writing the resume brief: {e}");
                    None
                }
            }
        }
        None => None,
    };

    let worker: Option<Session> = match registered {
        Some(w) => Some(w),
        None => client
            .get(&format!(
                "/v1/resolve?harness={}&external_id={}",
                settings.harness.as_str(),
                encode_query(&args.session_id)
            ))
            .await
            .ok(),
    };
    // What it ran on, and any setting replaced, opens its chat's record.
    if let Some(w) = &worker {
        let mut body = format!("Ran on {}.", settings.describe(&args.skill));
        for note in &notes {
            body.push_str(&format!("\n\nNote: {note}"));
        }
        let req = jgt_core::api::AgentMessageRequest { kind: jgt_core::MessageKind::Event, body, meta: Value::Null };
        let _: Result<jgt_core::Message, _> = client.post(&format!("/v1/sessions/{}/messages", w.id), &req).await;
    }
    // The full result goes to the worker's own chat, where the user follows it.
    if let (Some(w), true) = (&worker, ok) {
        let _: Result<jgt_core::Message, _> = client
            .post(
                &format!("/v1/sessions/{}/messages", w.id),
                &jgt_core::api::AgentMessageRequest {
                    kind: jgt_core::MessageKind::Reply,
                    body: result.clone(),
                    meta: Value::Null,
                },
            )
            .await;
    }
    let detail = if ok || !result.is_empty() { result.clone() } else { tail(&stderr, 20) };
    let finished = runs::Finished {
        worker: &args.session_id,
        skill: &args.skill,
        task: &args.task,
        ok,
        secs: elapsed,
        cost_usd: cost,
        result: &detail,
        harness: settings.harness,
        model: &settings.model,
        effort: &settings.effort,
        tokens: (outcome.tokens_in, outcome.tokens_out),
    };
    let result_file = match runs::worker_finished(&args.run_dir, &finished) {
            Ok(path) => Some(path),
            Err(e) => {
                eprintln!("jg-team-dev: recording the result in {}: {e:#}", args.run_dir.display());
                None
            }
        };
    if let Some(w) = &worker {
        let _: Result<Session, _> = client
            .patch(
                &format!("/v1/sessions/{}?source=agent", w.id),
                &jgt_core::api::PatchSessionRequest {
                    run_secs: Some(elapsed as i64),
                    cost_usd: cost,
                    model: Some(settings.model.clone()),
                    effort: Some(settings.effort.clone()),
                    tokens_in: outcome.tokens_in,
                    tokens_out: outcome.tokens_out,
                    ..Default::default()
                },
            )
            .await;
    }
    if let Some(task) = &args.task_id {
        // Reported by the worker itself: its Coordinator may have reset meanwhile.
        let actor = worker.as_ref().map_or(args.parent.as_str(), |w| w.id.as_str());
        let mut meta = serde_json::json!({
            "worker": args.session_id, "skill": args.skill, "ok": ok, "secs": elapsed, "cost_usd": cost,
            "harness": settings.harness, "model": settings.model,
        });
        if let Some(l) = &limit {
            // The hub sets *Waiting: usage limit* on the task and wakes the Coordinator at `until`.
            meta["usage_limit"] = serde_json::json!({ "resets": l.resets, "until": l.until, "resume_brief": resume });
        }
        let body = match (&limit, ok) {
            (Some(l), _) => format!("{} stopped by the usage limit; resets {}", args.skill, l.resets),
            (None, true) => format!("{} finished", args.skill),
            (None, false) => format!("{} failed", args.skill),
        };
        if let Err(e) = log_worker(&client, actor, task, TaskEventKind::WorkerFinished, body, meta).await {
            eprintln!("jg-team-dev: logging the worker's finish on {task}: {e:#}");
        }
    }
    let cost = cost.map(|c| format!(", ${c:.2}")).unwrap_or_default();
    let took = format!("{}m{:02}s{cost}", elapsed / 60, elapsed % 60);
    let head = if let Some(l) = &limit {
        let resume = resume.as_ref().map(|p| format!(" Resume brief: {}.", p.display())).unwrap_or_default();
        let wake = if args.task_id.is_some() {
            "The hub wakes you when it resets; start no worker until then."
        } else {
            "Start no worker until then."
        };
        format!("[jg-team-dev] Worker **{title}** stopped by the usage limit after {took}; resets {}. {wake}{resume}", l.resets)
    } else if ok {
        format!("[jg-team-dev] Worker **{title}** finished ({took}).")
    } else {
        format!("[jg-team-dev] Worker **{title}** failed after {took} ({status}).")
    };
    let text = digest(&head, &detail, &notes, result_file.as_deref());
    team::relay(&client, &args.parent, worker.as_ref().map(|w| w.id.as_str()), &text)
        .await
        .map_err(anyhow::Error::msg)?;
    let _ = std::fs::remove_file(&args.brief_file);
    // The Coordinator closed the run while this worker ran: hide its chat like the others.
    if let (Some(w), Some(run)) = (&worker, runs::load(&args.run_dir))
        && run.finished
    {
        let _ = runs::archive(&client, w).await;
    }
    Ok(())
}

/// `jg-team-dev workers`: the workers of the Coordinator running this command.
pub async fn list(parent: Option<String>) -> Result<()> {
    let client = HubClient::default();
    let parent = coordinator(&client, parent).await?;
    let all: SessionsResponse = client.get("/v1/sessions?archived=true").await?;
    let mine: Vec<&Session> = all.sessions.iter().filter(|s| s.parent_id.as_deref() == Some(&parent.id)).collect();
    let running = mine.iter().filter(|s| s.status.is_connected()).count();
    println!("{running} running, {} finished", mine.len() - running);
    for s in mine {
        let state = match s.status {
            SessionStatus::Active => "working",
            SessionStatus::Idle | SessionStatus::Listening => "idle",
            SessionStatus::Disconnected => "finished",
            SessionStatus::Ended => "ended",
        };
        println!("{:<10} {:<9} {}", jgt_core::short_id(&s.external_id), state, s.display_name());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_path_maps_worktree_folder_to_project() {
        let parent: Session = serde_json::from_value(serde_json::json!({
            "id": "c1",
            "harness": "claude-code",
            "external_id": "ext-c1",
            "status": "idle",
            "listen": false,
            "mirror": false,
            "archived": false,
            "awaiting_reply": false,
            "created_at": 0,
            "updated_at": 0,
            "last_seen_at": 0,
            "role": "coordinator",
            "cwd": "/w/.team/worktrees/jgt-5",
        }))
        .unwrap();
        let path = task_path(&parent, "JGT-12", "").unwrap();
        assert_eq!(path, "/v1/tasks/JGT-12?workspace=%2Fw");
    }

    #[test]
    fn uuids_are_version_4() {
        let id = new_uuid().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, new_uuid().unwrap());
    }

    #[test]
    fn digests_stay_short_and_point_at_the_full_result() {
        let file = Path::new("/p/.team/runs/r1/worker-results/coder-abcd1234.md");
        let long_summary = "x".repeat(400);
        let coder = format!(
            "Lots of narration\n{}\n```json\n{{\"status\": \"completed\", \"summary\": \"{long_summary}\", \
             \"filesChanged\": [\"a.rs\", \"b.rs\", \"c.rs\"]}}\n```",
            "line\n".repeat(200)
        );
        let text = digest("[jg-team-dev] Worker **coder · x** finished (1m00s).", &coder, &[], Some(file));
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() <= DIGEST_LINES, "{text}");
        assert_eq!(lines[1], "Status: completed");
        assert!(lines[2].starts_with("Summary: xxx") && lines[2].chars().count() <= SUMMARY_MAX + 9, "{}", lines[2]);
        assert_eq!(lines[3], "3 files changed");
        assert_eq!(lines.last().copied(), Some("Full result: /p/.team/runs/r1/worker-results/coder-abcd1234.md"));

        let reviewer = r#"done
```json
{"status": "passed", "decision": "changes_requested", "commands": ["cargo test", "cargo build"], "failures": [],
 "findings": [{"severity": "critical", "critical": true}, {"severity": "high", "critical": false},
              {"severity": "other", "critical": false}, {"severity": "other", "critical": false}]}
```"#;
        let text = digest("head", reviewer, &[], Some(file));
        assert!(text.contains("Status: passed · changes_requested"), "{text}");
        assert!(text.contains("findings: 1 critical, 1 high, 2 other · 2 checks run, 0 failed"), "{text}");

        // No final JSON: the first lines, then the path.
        let text = digest("head", &"some line\n".repeat(50), &["coder: a note".into()], None);
        assert!(text.lines().count() <= DIGEST_LINES, "{text}");
        assert!(text.contains("Note: coder: a note") && text.ends_with("Full result: in the worker's chat."), "{text}");
    }

    #[test]
    fn coder_digest_shows_tests_status() {
        let file = Path::new("/p/.team/runs/r1/worker-results/coder-abcd1234.md");
        let coder = r#"Narration
```json
{"status": "completed", "summary": "Done with changes", "filesChanged": ["a.rs", "b.rs"],
 "tests": {"status": "pass", "commands": ["cargo test"], "summary": "all pass", "failures": []}}
```"#;
        let text = digest("[jg-team-dev] Worker **coder · x** finished.", coder, &[], Some(file));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "Status: completed");
        assert_eq!(lines[2], "Summary: Done with changes");
        assert_eq!(lines[3], "2 files changed · tests: pass");
    }

    #[test]
    fn workers_fall_back_when_their_harness_cannot_run() {
        let config = Config::default();
        let cwd = Path::new("/nonexistent");
        let only_claude = |h: Harness| if h == Harness::ClaudeCode { Ok(()) } else { Err(format!("{} is missing", h.label())) };
        let coder = config.team.for_skill_checked("coder", cwd);
        let (s, notes) = pick_settings(coder.clone(), "coder", None, None, only_claude).unwrap();
        assert_eq!(s.describe("coder"), "coder · Claude Code · sonnet · medium");
        assert_eq!(notes, ["coder: Antigravity is missing; using the fallback coder · Claude Code · sonnet · medium"]);
        // `--model`/`--effort` apply to the harness the worker runs on.
        let (s, _) = pick_settings(coder.clone(), "coder", Some("opus"), Some("high"), only_claude).unwrap();
        assert_eq!(s.describe("coder"), "coder · Claude Code · opus · high");
        assert!(pick_settings(coder.clone(), "coder", None, Some("hihg"), only_claude).is_err());
        // Antigravity available: the role's own settings.
        let (s, notes) = pick_settings(coder, "coder", None, None, |_| Ok(())).unwrap();
        assert_eq!((s.harness, notes.len()), (Harness::Antigravity, 0));
        // No fallback and no harness: a clear error, never a hang.
        let tester = config.team.for_skill_checked("tester", cwd);
        let err = pick_settings(tester, "tester", None, None, |_| Err("claude not found".into())).unwrap_err();
        assert_eq!(err.to_string(), "tester: claude not found, and the role has no fallback harness");
    }

    #[test]
    fn the_coder_keeps_its_harness_under_a_team_model() {
        let mut config = Config::default();
        config.team.model = "opus".into();
        let coder = config.team.for_skill_checked("coder", Path::new("/nonexistent"));
        let (s, _) = pick_settings(coder.clone(), "coder", None, None, |_| Ok(())).unwrap();
        assert_eq!(s.describe("coder"), "coder · Antigravity · gemini-3.8-flash-high · high");
        let no_agy = |h: Harness| if h == Harness::Antigravity { Err("agy is missing".to_string()) } else { Ok(()) };
        let (s, _) = pick_settings(coder, "coder", None, None, no_agy).unwrap();
        assert_eq!(s.describe("coder"), "coder · Claude Code · sonnet · medium");
    }

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);

    impl StartReport for Recorder {
        async fn relay(&self, text: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("relay: {text}"));
            Ok(())
        }

        async fn log(&self, task: &str, body: String, meta: Value) -> Result<()> {
            self.0.lock().unwrap().push(format!("log {task}: {body} ({})", meta["ok"]));
            // A hub that cannot log does not stop the Coordinator hearing of it.
            bail!("hub down")
        }
    }

    #[tokio::test]
    async fn a_worker_that_cannot_start_tells_the_coordinator_and_the_task() {
        let dir = std::env::temp_dir().join(format!("jgt-start-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let brief_file = dir.join("brief.md");
        std::fs::write(&brief_file, "Do it.").unwrap();
        let mut args = WorkerRunArgs {
            skill: "tester".into(),
            task: "check".into(),
            brief_file: brief_file.clone(),
            cwd: dir.clone(),
            parent: "coord".into(),
            session_id: "uuid".into(),
            run_dir: dir.clone(),
            task_id: Some("t1".into()),
            model: None,
            effort: Some("hihg".into()),
        };
        // Fails before it writes any file: a bad `--effort`, or no `claude` here.
        let err = match start_worker(&Config::default(), &args, "tester · check") {
            Err(e) => e,
            Ok(_) => panic!("started with a bad effort"),
        };
        let report = Recorder::default();
        could_not_start(&report, &args, "tester · check", &err).await;
        let seen = report.0.into_inner().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].starts_with(&format!("log t1: tester could not start: {err:#} (false)")), "{seen:?}");
        assert_eq!(seen[1], format!("relay: [jg-team-dev] Worker **tester · check** could not start: {err:#}"));
        assert!(!brief_file.exists());
        // No task: the Coordinator alone hears of it.
        args.task_id = None;
        let report = Recorder::default();
        could_not_start(&report, &args, "tester · check", &anyhow::anyhow!("reading the brief")).await;
        assert_eq!(report.0.into_inner().unwrap(), ["relay: [jg-team-dev] Worker **tester · check** could not start: reading the brief"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_limit_deaths_are_detected_with_their_reset() {
        use chrono::TimeZone;
        // 14:10 in São Paulo (UTC-3).
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 26, 17, 10, 0).unwrap();
        let sao_paulo = |zone: Option<&str>| {
            assert_eq!(zone, Some("America/Sao_Paulo"));
            chrono::FixedOffset::west_opt(3 * 3600)
        };
        let text = "You've hit your session limit · resets 5pm (America/Sao_Paulo)";
        let limit = usage_limit(text, true, now, sao_paulo).unwrap();
        assert_eq!(limit.resets, "17:00");
        assert_eq!(limit.until, Some(chrono::Utc.with_ymd_and_hms(2026, 10, 26, 20, 0, 0).unwrap().timestamp_millis()));
        // A reset earlier in the day than now is tomorrow's.
        let text = "You've hit your session limit · resets 10:30am (America/Sao_Paulo)";
        let limit = usage_limit(text, true, now, sao_paulo).unwrap();
        assert_eq!(limit.resets, "10:30");
        assert_eq!(limit.until, Some(chrono::Utc.with_ymd_and_hms(2026, 10, 27, 13, 30, 0).unwrap().timestamp_millis()));
        // Both must hold: an error, and the text (Q4).
        assert_eq!(usage_limit(text, false, now, sao_paulo), None);
        assert_eq!(usage_limit("Error: the build failed", true, now, sao_paulo), None);
        // An unreadable time still stops the worker as a usage limit, without a timer.
        let odd = usage_limit("You've hit your session limit · resets soon", true, now, |_| None).unwrap();
        assert_eq!((odd.resets.as_str(), odd.until), ("soon", None));
        assert_eq!(parse_clock("12am"), Some((0, 0)));
        assert_eq!(parse_clock("12pm"), Some((12, 0)));
        assert_eq!(parse_clock("17:00"), Some((17, 0)));
        assert_eq!(parse_clock("25:00"), None);
    }

    #[test]
    fn the_resume_brief_keeps_the_original_and_says_not_to_restart() {
        let dir = std::env::temp_dir().join(format!("jgt-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = resume_brief("Implement X.\n", &dir);
        assert!(text.starts_with("Resume: the previous worker stopped mid-task"));
        assert!(text.contains("do not restart") && text.contains("`git status --short`") && text.contains("`git diff --stat`"));
        assert!(text.trim_end().ends_with("Implement X."));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn brief_must_not_be_empty() {
        let args = WorkerArgs {
            skill: "coder".into(),
            task: "t".into(),
            brief_file: None,
            cwd: None,
            worktree: None,
            main_worktree: false,
            parent: None,
            run: None,
            task_id: None,
            model: None,
            effort: None,
            brief: vec!["fix".into(), "it".into()],
        };
        assert_eq!(read_brief(&args).unwrap(), "fix it");
    }

    #[test]
    fn worker_worktree_defaults_to_the_task_key() {
        let mut args = WorkerArgs {
            skill: "coder".into(),
            task: "t".into(),
            brief_file: None,
            cwd: None,
            worktree: None,
            main_worktree: false,
            parent: None,
            run: None,
            task_id: Some("JGT-12".into()),
            model: None,
            effort: None,
            brief: vec![],
        };
        let task = Task::new("/w", 12, "fix navbar", jgt_core::TaskStatus::Working);
        assert_eq!(task_worktree(&args, Some(&task)).as_deref(), Some("jgt-12"));
        assert!(worktree::check_name("jgt-12").is_ok());
        assert_eq!(task_worktree(&args, None), None, "no task, no default");
        args.worktree = Some("shared".into());
        assert_eq!(task_worktree(&args, Some(&task)).as_deref(), Some("shared"));
        args.worktree = None;
        args.main_worktree = true;
        assert_eq!(task_worktree(&args, Some(&task)), None);
        args.main_worktree = false;
        args.cwd = Some("/elsewhere".into());
        assert_eq!(task_worktree(&args, Some(&task)), None);
    }

    #[test]
    fn coordinator_rejects_non_coordinator_sessions() {
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

        let check = |sess: &Session| -> Result<Session> {
            if !sess.is_coordinator() {
                bail!("{}", sess.not_coordinator_error());
            }
            Ok(sess.clone())
        };

        let err = check(&s).unwrap_err().to_string();
        assert_eq!(
            err,
            "this Claude Code session has no jg-team-dev role (JGT_ROLE was not set when it started). Run jg-team-dev claim-coordinator, or restart it with jg-team-dev start."
        );

        s.role = Some("coder".into());
        let err = check(&s).unwrap_err().to_string();
        assert_eq!(err, "session my-worker is a coder worker, not a Coordinator.");

        s.role = Some(jgt_core::COORDINATOR.into());
        assert!(check(&s).is_ok());
    }
}
