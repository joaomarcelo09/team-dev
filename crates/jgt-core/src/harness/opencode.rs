//! OpenCode workers (PRD H5).
//!
//! Checked on this machine with `opencode run --help` (1.18.35) and one smoke run (Oct 27, 2026):
//! `opencode run [message]` runs headless; `--format json` prints one JSON event per line:
//! `{"type": "text", "sessionID", "part": {"messageID", "text"}}` for the reply and
//! `{"type": "step_finish", "part": {"tokens": {"input", "output", …}, "cost"}}` per step.
//! `--model provider/model`, `--agent`, `--dir`, `--variant`, `--auto` (approve what is not
//! denied), `--pure` (no external plugins) and `--session` exist. `OPENCODE_CONFIG_DIR` and
//! `OPENCODE_CONFIG_CONTENT` are read from the environment.
//!
//! Our files live under jg-team-dev's own data folder: the user's `~/.config/opencode` (and its
//! plugins) is never touched, and `--pure` keeps those plugins out of the worker.

use super::{Caps, HarnessAdapter, Outcome, WorkerLaunch, brief_prompt, effort_levels, skill_text};
use crate::{
    config::Config,
    launch::{LaunchPlan, find_program},
    model::{Harness, Session},
    team::{SESSION_ENV, WORKER_SKILLS},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub struct Opencode;

/// Extracts model identifiers from `opencode models` output.
///
/// Keeps lines that contain `/` (`provider/model`), ignoring header or status lines.
pub fn parse_models(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.contains('/') {
                return None;
            }
            let token = line.split_whitespace().find(|w| w.contains('/'))?;
            Some(token.to_string())
        })
        .collect()
}

/// `<files>/opencode`: our OpenCode config folder (`OPENCODE_CONFIG_DIR`), with one agent per skill.
fn config_dir(files: &Path) -> PathBuf {
    files.join("opencode")
}

/// The agent a worker with `skill` runs as.
fn agent(skill: &str) -> String {
    format!("jgt-{skill}")
}

/// What `OPENCODE_CONFIG_CONTENT` adds for a worker: our MCP server, told the worker's id, and
/// deny rules for the git commands only the git-coder may run (PRD Q8: no routing, `--auto`).
fn inline_config(exe: &Path, session_id: &str, skill: &str) -> Value {
    let mut bash = serde_json::Map::new();
    if skill != "git-coder" {
        // Checked with `opencode debug agent`: each becomes a `bash` rule with this glob pattern,
        // after the `*` allow, so it wins.
        for cmd in ["git commit*", "git push*", "git merge*", "git rebase*", "git reset --hard*"] {
            bash.insert(cmd.into(), json!("deny"));
        }
    }
    // Everything else is approved by `--auto`, so no catch-all rule (whose place in the
    // matching order is unverified).
    json!({
        "mcp": {
            crate::team::PLUGIN_NAME: {
                "type": "local",
                "command": [exe.to_string_lossy(), "mcp", "--harness", "opencode"],
                "environment": { SESSION_ENV: session_id },
                "enabled": true,
            }
        },
        "permission": { "bash": bash },
    })
}

impl HarnessAdapter for Opencode {
    fn harness(&self) -> Harness {
        Harness::Opencode
    }

    fn caps(&self) -> Caps {
        Caps {
            coordinator: false,
            registers_itself: false,
            permission_routing: false,
            mcp: true,
            effort_levels: effort_levels(Harness::Opencode),
            reports_cost: true,
            run_timeout: Some(Duration::from_secs(2 * 3600)),
        }
    }

    fn find(&self, config: &Config) -> Result<PathBuf, String> {
        find_program("opencode", config.commands.opencode.as_deref())
    }

    /// One agent file per worker skill (PRD Q9), from the skills Claude Code's install wrote.
    fn install(&self, files: &Path) -> std::io::Result<()> {
        let agents = config_dir(files).join("agents");
        std::fs::create_dir_all(&agents)?;
        for skill in WORKER_SKILLS {
            let body = skill_text(files, skill)?;
            // `opencode debug agent` reads these as name, description, mode and the body as the prompt.
            let text = format!("---\ndescription: jg-team-dev {skill} worker\nmode: primary\n---\n\n{body}\n");
            std::fs::write(agents.join(format!("{}.md", agent(skill))), text)?;
        }
        Ok(())
    }

    fn worker(&self, w: &WorkerLaunch) -> Result<LaunchPlan, String> {
        let mut args: Vec<String> = vec!["run".into(), "--format".into(), "json".into(), "--pure".into()];
        args.extend(["--agent".into(), agent(w.skill)]);
        args.extend(["--dir".into(), w.cwd.to_string_lossy().into_owned()]);
        // OpenCode names models `provider/model`; anything else (a Claude alias) is left to its default.
        if w.settings.model.contains('/') {
            args.extend(["--model".into(), w.settings.model.clone()]);
        }
        args.extend(self.effort_args(&w.settings.effort).unwrap_or_default());
        args.extend(["--title".into(), w.title.to_string(), "--auto".into()]);
        args.push(brief_prompt(w.brief, w.brief_file));
        let mut env = w.env.clone();
        env.push(("OPENCODE_CONFIG_DIR".into(), config_dir(w.files).to_string_lossy().into_owned()));
        env.push(("OPENCODE_CONFIG_CONTENT".into(), inline_config(&w.exe, w.session_id, w.skill).to_string()));
        env.push((SESSION_ENV.into(), w.session_id.to_string()));
        Ok(LaunchPlan {
            program: w.program.clone(),
            args,
            cwd: Some(w.cwd.to_path_buf()),
            // `opencode` may be a `#!/usr/bin/env node` script next to its `node` (nvm installs).
            path_prepend: w.program.parent().map(Path::to_path_buf),
            wait: true,
            log: None,
            env,
        })
    }

    /// Our id is not OpenCode's session id (`ses_…`), so a finished worker is not resumed; the
    /// Coordinator starts a fresh one.
    fn resume(&self, _s: &Session, _prompt: &str, _config: &Config) -> Result<LaunchPlan, String> {
        Err("OpenCode workers are not resumed; start a fresh worker".into())
    }

    /// The text of the last message that has text, and the tokens and cost of every step.
    fn parse_result(&self, stdout: &str, stderr: &str, ok: bool) -> Outcome {
        let events: Vec<Value> = stdout.lines().filter_map(|l| serde_json::from_str(l.trim()).ok()).collect();
        if events.is_empty() {
            let text = if stdout.trim().is_empty() { stderr.trim() } else { stdout.trim() };
            return Outcome { ok: false, text: text.to_string(), ..Outcome::default() };
        }
        let kind = |e: &Value| e.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let part = |e: &Value, k: &str| e.get("part").and_then(|p| p.get(k)).cloned();
        let last_message = events
            .iter()
            .filter(|e| kind(e) == "text")
            .filter_map(|e| part(e, "messageID").and_then(|m| m.as_str().map(str::to_owned)))
            .next_back();
        let text: Vec<String> = events
            .iter()
            .filter(|e| kind(e) == "text")
            .filter(|e| part(e, "messageID").and_then(|m| m.as_str().map(str::to_owned)) == last_message)
            .filter_map(|e| part(e, "text").and_then(|t| t.as_str().map(str::to_owned)))
            .collect();
        let steps: Vec<&Value> = events.iter().filter(|e| kind(e) == "step_finish").collect();
        let sum_tokens = |k: &str| {
            let values: Vec<i64> =
                steps.iter().filter_map(|e| part(e, "tokens").and_then(|t| t.get(k).and_then(Value::as_i64))).collect();
            (!values.is_empty()).then(|| values.iter().sum())
        };
        let costs: Vec<f64> = steps.iter().filter_map(|e| part(e, "cost").and_then(|c| c.as_f64())).collect();
        // An error event (seen with an unknown model, exit 1):
        // `{"type": "error", "error": {"name": "UnknownError", "data": {"message": "…"}}}`.
        let error = events.iter().find(|e| kind(e) == "error").map(|e| {
            let err = e.get("error");
            let field = |k: &str| err.and_then(|v| v.get(k)).and_then(Value::as_str);
            let message = err.and_then(|v| v.pointer("/data/message")).and_then(Value::as_str);
            match (field("name"), message) {
                (Some(name), Some(m)) => format!("{name}: {m}"),
                (None, Some(m)) => m.to_string(),
                _ => err.or_else(|| e.get("part")).map_or_else(|| e.to_string(), Value::to_string),
            }
        });
        Outcome {
            ok: ok && error.is_none() && !text.is_empty(),
            text: match (&error, text.is_empty()) {
                (Some(err), _) => err.clone(),
                (None, true) => "(no reply)".into(),
                (None, false) => text.join(""),
            },
            cost_usd: (!costs.is_empty()).then(|| costs.iter().sum()),
            tokens_in: sum_tokens("input"),
            tokens_out: sum_tokens("output"),
            conversation: events.iter().find_map(|e| e.get("sessionID").and_then(Value::as_str).map(str::to_owned)),
        }
    }

    /// `--variant` names are provider-specific; with no table of them we pass none (PRD Q7).
    fn effort_args(&self, _level: &str) -> Option<Vec<String>> {
        None
    }

    fn models(&self, config: &Config) -> Result<Vec<String>, String> {
        let program = self.find(config)?;
        let stdout = super::run_with_timeout(&program, &["models"], super::MODELS_TIMEOUT)?;
        Ok(parse_models(&stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_models_extracts_slash_containing_lines_and_skips_headers() {
        let sample = "\
OpenCode 1.18.35
Available models:
anthropic/claude-3-5-sonnet
openai/gpt-4o
google/gemini-1.5-pro
deepseek/deepseek-chat
";
        let models = parse_models(sample);
        assert_eq!(
            models,
            [
                "anthropic/claude-3-5-sonnet",
                "openai/gpt-4o",
                "google/gemini-1.5-pro",
                "deepseek/deepseek-chat",
            ]
        );
    }

    #[test]
    fn parse_models_handles_descriptions_and_whitespace() {
        let sample = "  anthropic/claude-3-5-sonnet (recommended)  \nopenai/gpt-4o\tDefault\n";
        assert_eq!(parse_models(sample), ["anthropic/claude-3-5-sonnet", "openai/gpt-4o"]);
    }

    #[test]
    fn the_smoke_run_output_parses() {
        let out = r#"{"type":"step_start","timestamp":1,"sessionID":"ses_e81c","part":{"messageID":"msg_1","type":"step-start"}}
{"type":"text","timestamp":1,"sessionID":"ses_e81c","part":{"messageID":"msg_1","type":"text","text":"ok"}}
{"type":"step_finish","timestamp":1,"sessionID":"ses_e81c","part":{"reason":"stop","messageID":"msg_1","type":"step-finish","tokens":{"total":20168,"input":18374,"output":2,"reasoning":0,"cache":{"write":0,"read":1792}},"cost":0}}"#;
        let o = Opencode.parse_result(out, "", true);
        assert_eq!((o.ok, o.text.as_str(), o.cost_usd, o.tokens_in, o.tokens_out), (true, "ok", Some(0.0), Some(18374), Some(2)));
        assert_eq!(o.conversation.as_deref(), Some("ses_e81c"));
        // Several messages: the reply is the last one's text.
        let two = r#"{"type":"text","part":{"messageID":"m1","text":"thinking about it"}}
{"type":"text","part":{"messageID":"m2","text":"done "}}
{"type":"text","part":{"messageID":"m2","text":"```json\n{}\n```"}}"#;
        assert_eq!(Opencode.parse_result(two, "", true).text, "done ```json\n{}\n```");
        assert!(!Opencode.parse_result(r#"{"type":"error","error":{"message":"bad model"}}"#, "", true).ok);
        let err = r#"{"type":"error","timestamp":1,"sessionID":"ses_e815","error":{"name":"UnknownError","data":{"message":"Unexpected server error. Check server logs for details.","ref":"err_c7fd"}}}"#;
        let o = Opencode.parse_result(err, "", false);
        assert_eq!((o.ok, o.text.as_str()), (false, "UnknownError: Unexpected server error. Check server logs for details."));
        let none = Opencode.parse_result("", "Error: provider not configured", false);
        assert_eq!((none.ok, none.text.as_str()), (false, "Error: provider not configured"));
    }

    #[test]
    fn workers_use_our_config_dir_and_mcp_server() {
        let files = std::env::temp_dir().join(format!("jgt-oc-{}", std::process::id()));
        for skill in WORKER_SKILLS {
            let dir = files.join("plugin/skills").join(skill);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("---\nname: {skill}\n---\n\n# {skill}\n")).unwrap();
        }
        Opencode.install(&files).unwrap();
        let agent = std::fs::read_to_string(files.join("opencode/agents/jgt-coder.md")).unwrap();
        assert!(agent.starts_with("---\ndescription: jg-team-dev coder worker\nmode: primary\n---\n\n# coder"), "{agent}");

        let mut settings = Config::default().team.for_skill("reviewer", Path::new("/nonexistent"));
        settings.harness = Harness::Opencode;
        settings.model = "deepseek/deepseek-chat".into();
        let w = WorkerLaunch {
            program: "/nvm/bin/opencode".into(),
            exe: "/bin/jg-team-dev".into(),
            files: &files,
            skill: "reviewer",
            title: "reviewer · x",
            brief: "Review it.",
            brief_file: Path::new("/b.md"),
            settings: &settings,
            cwd: Path::new("/w"),
            session_id: "uuid",
            env: vec![],
        };
        let plan = Opencode.worker(&w).unwrap();
        assert_eq!(
            plan.args,
            [
                "run", "--format", "json", "--pure", "--agent", "jgt-reviewer", "--dir", "/w", "--model",
                "deepseek/deepseek-chat", "--title", "reviewer · x", "--auto", "Review it."
            ]
        );
        let env = |k: &str| plan.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(env("OPENCODE_CONFIG_DIR"), files.join("opencode").to_string_lossy());
        assert_eq!(env(SESSION_ENV), "uuid");
        let inline: Value = serde_json::from_str(&env("OPENCODE_CONFIG_CONTENT")).unwrap();
        assert_eq!(inline["mcp"]["jg-team-dev"]["command"], json!(["/bin/jg-team-dev", "mcp", "--harness", "opencode"]));
        assert_eq!(inline["mcp"]["jg-team-dev"]["environment"][SESSION_ENV], "uuid");
        assert_eq!(inline["permission"]["bash"]["git push*"], "deny");
        assert_eq!(plan.path_prepend.as_deref(), Some(Path::new("/nvm/bin")));
        let _ = std::fs::remove_dir_all(&files);
    }
}
