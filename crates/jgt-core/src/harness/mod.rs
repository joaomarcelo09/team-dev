//! What each harness (Claude Code, OpenCode, Antigravity) supports, and how a worker is started
//! on it (PRD Part B, H4). One `HarnessAdapter` per harness; `worker-run` and the reconnect
//! launcher go through it. The Coordinator stays on Claude Code (D2), whose hooks it needs.

pub mod antigravity;
pub mod claude_code;
pub mod opencode;

pub use antigravity::Antigravity;
pub use claude_code::{ClaudeCode, session_args, session_args_for_role};
pub use opencode::Opencode;

use crate::{
    config::{Config, EFFORT_LEVELS, SessionSettings},
    launch::LaunchPlan,
    model::{Harness, Session},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// What a harness can do for jg-team-dev.
#[derive(Debug, Clone, PartialEq)]
pub struct Caps {
    /// It can run the Coordinator: block the end of a turn and park in listen mode (hooks).
    pub coordinator: bool,
    /// Its sessions register themselves with the hub (a session-start hook); otherwise
    /// `worker-run` registers the worker and hands its id to the MCP server in `JGT_SESSION`.
    pub registers_itself: bool,
    /// Permission prompts can be routed to the app.
    pub permission_routing: bool,
    /// Our MCP server (chat and task tools) is attached to its workers.
    pub mcp: bool,
    /// The effort levels it takes (see `effort_levels`).
    pub effort_levels: &'static [&'static str],
    /// It reports a cost for a run.
    pub reports_cost: bool,
    /// How long a worker may run before `worker-run` stops it (never hang). `None`: no limit,
    /// as for Claude Code workers today.
    pub run_timeout: Option<Duration>,
}

/// Everything a harness needs to start a worker.
#[derive(Debug, Clone)]
pub struct WorkerLaunch<'a> {
    /// The harness program, found with `HarnessAdapter::find`.
    pub program: PathBuf,
    /// The jg-team-dev executable (for the MCP server command).
    pub exe: PathBuf,
    /// Where `install` wrote the team files (`team::files_dir`).
    pub files: &'a Path,
    pub skill: &'a str,
    /// Chat title: `<skill> · <task>`.
    pub title: &'a str,
    pub brief: &'a str,
    /// The brief's file, handed over instead when the brief is too long for a prompt.
    pub brief_file: &'a Path,
    pub settings: &'a SessionSettings,
    pub cwd: &'a Path,
    /// Our id for the worker: Claude Code's `--session-id`, the hub's `external_id`.
    pub session_id: &'a str,
    /// `JGT_ROLE`, `JGT_PARENT`, `JGT_TASK`, …
    pub env: Vec<(String, String)>,
}

/// A finished worker's result, as its harness reported it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    /// The final text (or what explains the failure).
    pub text: String,
    pub cost_usd: Option<f64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    /// The harness's own id for the conversation, when it differs from ours.
    pub conversation: Option<String>,
}

/// Briefs longer than this go to the worker as a file instead of in its first prompt.
pub const INLINE_BRIEF_MAX: usize = 64 * 1024;

pub trait HarnessAdapter: Sync {
    fn harness(&self) -> Harness;
    fn caps(&self) -> Caps;
    /// The program, from `[commands]` or the usual install folders.
    fn find(&self, config: &Config) -> Result<PathBuf, String>;
    /// Writes what the harness loads for team sessions (skills/agents, MCP config) under `files`.
    fn install(&self, files: &Path) -> std::io::Result<()>;
    /// The command that runs one headless worker turn and exits.
    fn worker(&self, w: &WorkerLaunch) -> Result<LaunchPlan, String>;
    /// The command that resumes a worker session for one more headless turn.
    fn resume(&self, s: &Session, prompt: &str, config: &Config) -> Result<LaunchPlan, String>;
    fn parse_result(&self, stdout: &str, stderr: &str, ok: bool) -> Outcome;
    /// The flags for `level`, or `None` when the harness takes no effort setting.
    fn effort_args(&self, level: &str) -> Option<Vec<String>>;
    /// Lists the models or aliases available for this harness.
    fn models(&self, config: &Config) -> Result<Vec<String>, String>;
}

/// The adapter of `harness`, if jg-team-dev can run workers on it.
pub fn adapter(harness: Harness) -> Option<&'static dyn HarnessAdapter> {
    match harness {
        Harness::ClaudeCode => Some(&ClaudeCode),
        Harness::Opencode => Some(&Opencode),
        Harness::Antigravity => Some(&Antigravity),
        Harness::Custom => None,
    }
}

/// The worker harnesses whose program is installed here, for the app's Settings.
pub fn installed(config: &Config) -> Vec<Harness> {
    [Harness::ClaudeCode, Harness::Opencode, Harness::Antigravity]
        .into_iter()
        .filter(|h| adapter(*h).is_some_and(|a| a.find(config).is_ok()))
        .collect()
}

/// A skill's text for a harness without our plugin: `plugin/skills/<skill>/SKILL.md` without its
/// front matter, which `install` of Claude Code wrote.
pub fn skill_text(files: &Path, skill: &str) -> std::io::Result<String> {
    let text = std::fs::read_to_string(files.join("plugin").join("skills").join(skill).join("SKILL.md"))?;
    let body = match text.strip_prefix("---") {
        Some(rest) => rest.split_once("\n---").map_or(rest, |(_, body)| body),
        None => &text,
    };
    // The skills speak of "a fresh Claude Code session"; on another harness it is just a session.
    Ok(body.trim().replace("Claude Code session", "session"))
}

/// The brief as the first prompt carries it: inline, or a pointer to its file when too long.
pub fn brief_prompt(brief: &str, brief_file: &Path) -> String {
    if brief.len() <= INLINE_BRIEF_MAX {
        brief.to_string()
    } else {
        format!("Your brief is long; read it first: {}", brief_file.display())
    }
}

/// The effort levels a harness takes, lowest first. Empty: it has no effort setting we can map,
/// so the flag is dropped and the chat shows "effort: n/a".
pub fn effort_levels(harness: Harness) -> &'static [&'static str] {
    match harness {
        // `claude --help`: --effort <level> (low, medium, high, xhigh, max).
        Harness::ClaudeCode => EFFORT_LEVELS,
        // `agy --help` (1.4.8): --effort (low|medium|high|xhigh|max).
        Harness::Antigravity => EFFORT_LEVELS,
        // `opencode run --variant` is "provider-specific reasoning effort". `opencode models
        // --verbose` (1.18.35) lists each model's `variants`: mostly our names, but a different
        // subset per model (none at all, `high, max`, `none, thinking`, …), so one table cannot
        // map them; we pass none (PRD Q7: no variant if unclear).
        Harness::Opencode | Harness::Custom => &[],
    }
}

/// The level `harness` runs for `level`: the same one, else the nearest lower one it has, else
/// its lowest. `None` when the harness has no effort setting.
pub fn nearest_effort(harness: Harness, level: &str) -> Option<&'static str> {
    let levels = effort_levels(harness);
    let wanted = EFFORT_LEVELS.iter().position(|l| *l == level)?;
    EFFORT_LEVELS[..=wanted].iter().rev().find_map(|l| levels.iter().find(|h| *h == l).copied()).or(levels.first().copied())
}

/// How long `models` commands may take before being stopped.
pub const MODELS_TIMEOUT: Duration = Duration::from_secs(20);

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    if pid > 1 {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

fn kill_child_and_group(child: &mut std::process::Child) {
    kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs `<program>` with `args` and `timeout`, prepending `program.parent()` to PATH.
pub(crate) fn run_with_timeout(
    program: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    if let Some(extra) = program.parent() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![extra.to_path_buf()];
        paths.extend(std::env::split_paths(&path));
        if let Ok(joined) = std::env::join_paths(paths) {
            cmd.env("PATH", joined);
        }
    }

    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", program.display()))?;

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();

    let _ = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut h) = stdout_handle {
            let _ = std::io::Read::read_to_end(&mut h, &mut out);
        }
        let _ = stdout_tx.send(String::from_utf8_lossy(&out).into_owned());
    });

    let _ = std::thread::spawn(move || {
        let mut err = Vec::new();
        if let Some(mut h) = stderr_handle {
            let _ = std::io::Read::read_to_end(&mut h, &mut err);
        }
        let _ = stderr_tx.send(String::from_utf8_lossy(&err).into_owned());
    });

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_child_and_group(&mut child);
                    return Err("timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                kill_child_and_group(&mut child);
                return Err(format!("failed waiting for {}: {e}", program.display()));
            }
        }
    };

    // Kill any background processes in the group so pipes close promptly.
    kill_process_group(child.id());

    let remaining = timeout.saturating_sub(start.elapsed()).min(Duration::from_millis(500));
    let stdout = stdout_rx.recv_timeout(remaining).unwrap_or_default();
    let stderr = stderr_rx.recv_timeout(remaining).unwrap_or_default();

    if status.success() {
        Ok(stdout)
    } else {
        let msg = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("{} exited with {}", program.display(), status)
        };
        Err(msg)
    }
}

/// Lists the models or aliases available for `harness`.
///
/// Claude Code returns built-in aliases (`opus`, `sonnet`, `fable`, `haiku`) without running a command.
/// Antigravity runs `agy models` with a 20 s timeout and keeps the first column of lines that contain a tab.
/// OpenCode runs `opencode models` with a 20 s timeout and keeps lines that contain `/`.
pub fn models(harness: Harness, config: &Config) -> Result<Vec<String>, String> {
    match adapter(harness) {
        Some(a) => a.models(config),
        None => Err(format!("{} does not support listing models", harness.label())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_maps_to_the_harness_levels() {
        assert_eq!(nearest_effort(Harness::ClaudeCode, "xhigh"), Some("xhigh"));
        assert_eq!(nearest_effort(Harness::Antigravity, "low"), Some("low"));
        assert_eq!(nearest_effort(Harness::Opencode, "high"), None);
        assert_eq!(nearest_effort(Harness::ClaudeCode, "bogus"), None);
    }

    #[test]
    fn models_returns_builtin_for_claude_code() {
        let list = models(Harness::ClaudeCode, &Config::default()).unwrap();
        assert_eq!(list, ["opus", "sonnet", "fable", "haiku"]);
    }

    #[test]
    fn models_returns_error_for_custom() {
        let err = models(Harness::Custom, &Config::default()).unwrap_err();
        assert!(err.contains("does not support listing models"), "{err}");
    }

    #[test]
    fn run_with_timeout_times_out() {
        let res = run_with_timeout(Path::new("sh"), &["-c", "sleep 1"], Duration::from_millis(50));
        assert_eq!(res.unwrap_err(), "timed out");
    }

    #[test]
    fn run_with_timeout_grandchild_pipe_does_not_block_on_timeout() {
        let start = std::time::Instant::now();
        let res = run_with_timeout(Path::new("sh"), &["-c", "sleep 5 & sleep 5"], Duration::from_millis(50));
        assert_eq!(res.unwrap_err(), "timed out");
        assert!(start.elapsed() < Duration::from_secs(2), "elapsed: {:?}", start.elapsed());
    }

    #[test]
    fn run_with_timeout_captures_failure_message() {
        let res = run_with_timeout(Path::new("sh"), &["-c", "echo 'something broke' >&2; exit 2"], Duration::from_secs(5));
        assert_eq!(res.unwrap_err(), "something broke");
    }

    #[test]
    fn models_antigravity_returns_models_when_installed() {
        let config = Config::default();
        if Antigravity.find(&config).is_ok() {
            let list = models(Harness::Antigravity, &config).unwrap();
            assert_eq!(
                list,
                [
                    "gemini-3.8-flash-high",
                    "gemini-3.8-flash-medium",
                    "gemini-3.8-flash-low",
                    "claude-sonnet-4-6",
                    "claude-opus-4-6-thinking",
                    "gpt-oss-120b-medium",
                ]
            );
        }
    }
}
