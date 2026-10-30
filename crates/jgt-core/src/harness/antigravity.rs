//! Google Antigravity (`agy`) workers (PRD H6).
//!
//! Checked on this machine with `agy --help` (1.4.8) and one smoke run (Oct 27, 2026):
//! `-p`/`--print` runs one prompt headless and exits; `--output-format json` prints one object
//! `{"conversation_id", "status": "SUCCESS", "response", "duration_seconds", "num_turns",
//! "usage": {"input_tokens", "output_tokens", …}}` (no cost); `--model`, `--effort
//! low|medium|high|xhigh|max`, `--mode accept-edits|auto|plan`, `--dangerously-skip-permissions`,
//! `--add-dir`, `--conversation <id>` and `--print-timeout` exist. It printed its JSON without a
//! TTY (the reported bug #76 did not show). MCP servers and plugins are only configured globally
//! (`agy mcp add` has no scope flag; `agy plugin install`), which we must not change, so its
//! workers run without our MCP server and get their skill inline in the prompt.
//!
//! Permissions (a second smoke run, 1.4.8): headless mode cannot ask, so a tool call that needs
//! permission is denied, and the run still ends `SUCCESS` with `"denied_actions": [{"action":
//! "command", …}]` in its JSON. `--mode auto` may not exist for an account ("not available for
//! this account; continuing in the default mode" on stderr). Only `--dangerously-skip-permissions`
//! (our `bypassPermissions`) lets a worker run commands; allow rules live in agy's own
//! `settings.json`, which is the user's.

use super::{Caps, HarnessAdapter, Outcome, WorkerLaunch, brief_prompt, effort_levels, skill_text};
use crate::{
    config::Config,
    launch::{LaunchPlan, find_program},
    model::{Harness, Session},
};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub struct Antigravity;

/// `agy --print-timeout`: the longest a worker turn may take; agy stops it itself.
const PRINT_TIMEOUT: &str = "2h";

impl HarnessAdapter for Antigravity {
    fn harness(&self) -> Harness {
        Harness::Antigravity
    }

    fn caps(&self) -> Caps {
        Caps {
            coordinator: false,
            registers_itself: false,
            permission_routing: false,
            // No per-run MCP config: `agy mcp add` writes the user's global config.
            mcp: false,
            effort_levels: effort_levels(Harness::Antigravity),
            reports_cost: false,
            // A little over agy's own `--print-timeout`, in case it does not stop.
            run_timeout: Some(Duration::from_secs(2 * 3600 + 300)),
        }
    }

    fn find(&self, config: &Config) -> Result<PathBuf, String> {
        find_program("agy", config.commands.antigravity.as_deref())
    }

    /// Nothing to write: the skill goes inline in the prompt (see the module docs).
    fn install(&self, _files: &Path) -> std::io::Result<()> {
        Ok(())
    }

    fn worker(&self, w: &WorkerLaunch) -> Result<LaunchPlan, String> {
        let skill = skill_text(w.files, w.skill).map_err(|e| format!("reading the {} skill: {e}", w.skill))?;
        let prompt = format!(
            "{skill}\n\n---\n\nNote: the jg-team-dev chat and task tools (chat_status, chat_send, chat_ask, task_get) \
             are not available in this session; skip them. Your final reply goes to the Coordinator.\n\n\
             # Your brief\n\n{}",
            brief_prompt(w.brief, w.brief_file)
        );
        let mut args = vec!["-p".to_string(), prompt, "--output-format".into(), "json".into()];
        args.extend(["--model".into(), w.settings.model.clone()]);
        args.extend(self.effort_args(&w.settings.effort).unwrap_or_default());
        // `--mode auto` does not approve tool calls headless (see the module docs); a denied call
        // fails the run in `parse_result`. `--print-timeout` stops a run that never ends.
        match w.settings.permission_mode.as_str() {
            "bypassPermissions" => args.push("--dangerously-skip-permissions".into()),
            "acceptEdits" => args.extend(["--mode".into(), "accept-edits".into()]),
            "plan" => args.extend(["--mode".into(), "plan".into()]),
            _ => args.extend(["--mode".into(), "auto".into()]),
        }
        if w.brief.len() > super::INLINE_BRIEF_MAX
            && let Some(dir) = w.brief_file.parent()
        {
            args.extend(["--add-dir".into(), dir.to_string_lossy().into_owned()]);
        }
        args.extend(["--print-timeout".into(), PRINT_TIMEOUT.into()]);
        Ok(LaunchPlan {
            program: w.program.clone(),
            args,
            cwd: Some(w.cwd.to_path_buf()),
            path_prepend: w.program.parent().map(Path::to_path_buf),
            wait: true,
            log: None,
            env: w.env.clone(),
        })
    }

    /// Our id is not agy's conversation id, so a finished worker is not resumed; the
    /// Coordinator starts a fresh one (`agy --conversation <id>` exists for when it is stored).
    fn resume(&self, _s: &Session, _prompt: &str, _config: &Config) -> Result<LaunchPlan, String> {
        Err("Antigravity workers are not resumed; start a fresh worker".into())
    }

    fn parse_result(&self, stdout: &str, stderr: &str, ok: bool) -> Outcome {
        let parsed: Option<Value> = stdout.lines().rev().find_map(|l| serde_json::from_str(l.trim()).ok());
        match parsed {
            Some(v) => {
                let status = v.get("status").and_then(Value::as_str).unwrap_or("");
                let tokens = |k: &str| v.get("usage").and_then(|u| u.get(k)).and_then(Value::as_i64);
                // A denied tool call still ends `SUCCESS`; any status but SUCCESS is a failure.
                let denied: Vec<&str> = v
                    .get("denied_actions")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|d| d.get("action").and_then(Value::as_str)).collect())
                    .unwrap_or_default();
                let text = v
                    .get("response")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .map(str::to_owned)
                    .or_else(|| v.get("error").map(|e| e.as_str().map_or_else(|| e.to_string(), str::to_owned)))
                    .unwrap_or_else(|| format!("(no response; status {status})"));
                let text = if denied.is_empty() {
                    text
                } else {
                    format!(
                        "{text}\n\nAntigravity denied these tool calls ({}): a headless agy run cannot ask for \
                         permission. Set `permission_mode = \"bypassPermissions\"` for this role to let it run them.",
                        denied.join(", ")
                    )
                };
                Outcome {
                    ok: ok && status.eq_ignore_ascii_case("success") && denied.is_empty(),
                    text,
                    cost_usd: None,
                    tokens_in: tokens("input_tokens"),
                    tokens_out: tokens("output_tokens"),
                    conversation: v.get("conversation_id").and_then(Value::as_str).map(str::to_owned),
                }
            }
            None => {
                let text = if stdout.trim().is_empty() { stderr.trim() } else { stdout.trim() };
                Outcome { ok: false, text: text.to_string(), ..Outcome::default() }
            }
        }
    }

    fn effort_args(&self, level: &str) -> Option<Vec<String>> {
        Some(vec!["--effort".into(), level.to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smoke_run_output_parses() {
        let out = r#"{"conversation_id":"e46bfb07-6c19-4660-acd4-338a46bceb73","status":"SUCCESS","response":"ok\n","duration_seconds":2.67,"num_turns":1,"usage":{"input_tokens":6785,"output_tokens":1,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":6786}}"#;
        let o = Antigravity.parse_result(out, "", true);
        assert_eq!((o.ok, o.text.trim(), o.cost_usd, o.tokens_in, o.tokens_out), (true, "ok", None, Some(6785), Some(1)));
        assert_eq!(o.conversation.as_deref(), Some("e46bfb07-6c19-4660-acd4-338a46bceb73"));
        let failed = Antigravity.parse_result(r#"{"status":"ERROR","response":""}"#, "", true);
        assert!(!failed.ok && failed.text.contains("ERROR"));
        // The second smoke run: the shell call was denied, yet the status is SUCCESS.
        let denied = r#"{"conversation_id":"1d4d1239","status":"SUCCESS","response":"","duration_seconds":2.86,"num_turns":1,"usage":{"input_tokens":6801,"output_tokens":41},"denied_actions":[{"action":"command","display_name":"RunCommand"}]}"#;
        let o = Antigravity.parse_result(denied, "", true);
        assert!(!o.ok && o.text.contains("denied these tool calls (command)") && o.text.contains("bypassPermissions"), "{}", o.text);
        let nothing = Antigravity.parse_result("", "agy: not logged in", false);
        assert_eq!((nothing.ok, nothing.text.as_str()), (false, "agy: not logged in"));
    }

    #[test]
    fn workers_get_the_skill_inline_and_only_known_flags() {
        let files = std::env::temp_dir().join(format!("jgt-agy-{}", std::process::id()));
        let skill = files.join("plugin/skills/coder");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: coder\n---\n\n# You are the Coder\n\nA fresh Claude Code session.\n").unwrap();
        let settings = Config::default().team.for_skill("coder", Path::new("/nonexistent"));
        let w = WorkerLaunch {
            program: "/home/u/.local/bin/agy".into(),
            exe: "/bin/jg-team-dev".into(),
            files: &files,
            skill: "coder",
            title: "coder · x",
            brief: "Fix it.",
            brief_file: Path::new("/b.md"),
            settings: &settings,
            cwd: Path::new("/w"),
            session_id: "uuid",
            env: vec![],
        };
        let plan = Antigravity.worker(&w).unwrap();
        assert_eq!(plan.args[0], "-p");
        let prompt = &plan.args[1];
        assert!(prompt.starts_with("# You are the Coder\n\nA fresh session.") && prompt.ends_with("# Your brief\n\nFix it."), "{prompt}");
        assert!(!prompt.contains("name: coder"));
        assert_eq!(
            plan.args[2..],
            ["--output-format", "json", "--model", "gemini-3.8-flash-high", "--effort", "high", "--mode", "auto", "--print-timeout", "2h"]
        );
        assert_eq!(plan.path_prepend.as_deref(), Some(Path::new("/home/u/.local/bin")));
        let _ = std::fs::remove_dir_all(&files);
    }
}
