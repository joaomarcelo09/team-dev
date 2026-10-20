//! Workers: fresh `claude -p` sessions a Coordinator starts with one skill and a brief.
//!
//! `jg-team-dev worker` returns at once; a detached `jg-team-dev worker-run` runs the session and
//! relays its result to the Coordinator through the hub, like a message from the user. That way
//! the result reaches the Coordinator even while it waits for the user in listen mode, and wakes
//! it when it went to sleep.

use crate::team::prepare_files;
use anyhow::{Context, Result, bail};
use jgt_core::{
    Harness, HubClient, Session, SessionStatus,
    api::SessionsResponse,
    client::encode_query,
    config::Config,
    launch, paths, procs,
    team::{self, PARENT_ENV, PLUGIN_NAME, ROLE_ENV, WORKER_SKILLS},
};
use serde_json::Value;
use std::{
    io::{IsTerminal, Read},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

/// Briefs longer than this go to the worker as a file instead of in its first prompt.
const INLINE_BRIEF_MAX: usize = 64 * 1024;

#[derive(clap::Args)]
pub struct WorkerArgs {
    /// planner, coder, reviewer, tester or git-coder.
    pub skill: String,
    /// Short task title, shown as the worker's chat title (`<skill> · <task>`).
    #[arg(long)]
    pub task: String,
    /// Read the brief from this file (otherwise from the remaining arguments, or stdin).
    #[arg(long)]
    pub brief_file: Option<PathBuf>,
    /// Folder the worker works in, e.g. a git worktree (default: the current folder).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Coordinator session id (default: the Coordinator running this command).
    #[arg(long)]
    pub parent: Option<String>,
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
async fn coordinator(client: &HubClient, explicit: Option<String>) -> Result<Session> {
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
        bail!("session {} is not a Coordinator", session.display_name());
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
    let client = HubClient::default();
    let parent = coordinator(&client, args.parent.clone()).await?;
    let cwd = match &args.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir()?,
    };
    let cwd = cwd.canonicalize().with_context(|| format!("{} does not exist", cwd.display()))?;

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
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Its own process group: it outlives this command and the Coordinator's Bash call.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().context("starting the worker")?;

    println!(
        "Started {} worker {} for \"{}\". It reports progress in its own chat; its result will reach you \
         as a message.",
        args.skill,
        jgt_core::short_id(&id),
        args.task.trim()
    );
    Ok(())
}

/// Pulls `result`, cost and duration out of `claude -p --output-format json`.
fn summarize(stdout: &str) -> (bool, String, Option<f64>) {
    let parsed: Option<Value> = stdout.lines().rev().find_map(|l| serde_json::from_str(l).ok());
    match parsed {
        Some(v) => {
            let ok = !v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let result = v.get("result").and_then(Value::as_str).unwrap_or("(no result)").to_string();
            (ok, result, v.get("total_cost_usd").and_then(Value::as_f64))
        }
        None => (false, stdout.trim().to_string(), None),
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// `jg-team-dev worker-run` (detached): runs the worker and relays its result.
pub async fn run(args: WorkerRunArgs) -> Result<()> {
    let config = Config::load();
    let files = prepare_files()?;
    let claude = launch::find_program("claude", config.commands.claude.as_deref()).map_err(anyhow::Error::msg)?;
    let brief = std::fs::read_to_string(&args.brief_file)?;
    let prompt = if brief.len() <= INLINE_BRIEF_MAX {
        format!("/{PLUGIN_NAME}:{} {brief}", args.skill)
    } else {
        format!("/{PLUGIN_NAME}:{} Your brief is long; read it first: {}", args.skill, args.brief_file.display())
    };
    let title = team::worker_title(&args.skill, &args.task);

    let started = Instant::now();
    let out = tokio::process::Command::new(&claude)
        .arg("-p")
        .arg(&prompt)
        .args(["--session-id", &args.session_id, "--output-format", "json"])
        .args(team::session_args(&files, &title, &config))
        .current_dir(&args.cwd)
        .env(ROLE_ENV, &args.skill)
        .env(PARENT_ENV, &args.parent)
        .stdin(Stdio::null())
        .output()
        .await
        .context("running claude")?;
    let elapsed = started.elapsed().as_secs();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (ok, result, cost) = summarize(&stdout);
    let ok = ok && out.status.success();

    let client = HubClient::default();
    let worker: Option<Session> = client
        .get(&format!(
            "/v1/resolve?harness={}&external_id={}",
            Harness::ClaudeCode.as_str(),
            encode_query(&args.session_id)
        ))
        .await
        .ok();
    // The result also goes to the worker's own chat, where the user follows it.
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
    let cost = cost.map(|c| format!(", ${c:.2}")).unwrap_or_default();
    let text = if ok {
        format!(
            "[jg-team-dev] Worker **{title}** finished ({}m{:02}s{cost}). Its result:\n\n{result}",
            elapsed / 60,
            elapsed % 60
        )
    } else {
        let detail = if result.is_empty() { tail(&stderr, 20) } else { result };
        format!("[jg-team-dev] Worker **{title}** failed after {}m{:02}s ({}).\n\n{detail}", elapsed / 60, elapsed % 60, out.status)
    };
    team::relay(&client, &args.parent, worker.as_ref().map(|w| w.id.as_str()), &text)
        .await
        .map_err(anyhow::Error::msg)?;
    let _ = std::fs::remove_file(&args.brief_file);
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
    fn uuids_are_version_4() {
        let id = new_uuid().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, new_uuid().unwrap());
    }

    #[test]
    fn summarize_reads_the_json_result() {
        let (ok, result, cost) = summarize(r#"{"is_error":false,"result":"done ```json\n{}\n```","total_cost_usd":0.25}"#);
        assert!(ok);
        assert!(result.starts_with("done"));
        assert_eq!(cost, Some(0.25));
        let (ok, result, _) = summarize("Error: something broke");
        assert!(!ok);
        assert_eq!(result, "Error: something broke");
    }

    #[test]
    fn brief_must_not_be_empty() {
        let args = WorkerArgs {
            skill: "coder".into(),
            task: "t".into(),
            brief_file: None,
            cwd: None,
            parent: None,
            brief: vec!["fix".into(), "it".into()],
        };
        assert_eq!(read_brief(&args).unwrap(), "fix it");
    }
}
