//! Claude Code: the Coordinator and, by default, every worker. Workers run
//! `claude -p "/jg-team-dev:<skill> <brief>" --output-format json` with our plugin, hooks and MCP
//! server, exactly as before the adapter existed.

use super::{Caps, HarnessAdapter, Outcome, WorkerLaunch, brief_prompt, effort_levels};
use crate::{
    config::{Config, SessionSettings},
    launch::{LaunchPlan, find_program},
    model::{Harness, Session},
    team::{PLUGIN_NAME, ROLE_ENV, PARENT_ENV},
};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct ClaudeCode;

/// Built-in models/aliases for Claude Code (which has no `models` command).
pub const MODELS: &[&str] = &["opus", "sonnet", "fable", "haiku"];

/// Claude Code flags for a team session titled `title`, without `--bg`/`-p` and the prompt.
/// `settings` come from `config.team.for_skill(<skill>, <folder>)`.
pub fn session_args(files: &Path, title: &str, settings: &SessionSettings) -> Vec<String> {
    session_args_for_role(files, title, settings, None)
}

/// Claude Code flags for a team session titled `title`, with an optional role (e.g. `COORDINATOR`),
/// without `--bg`/`-p` and the prompt.
pub fn session_args_for_role(files: &Path, title: &str, settings: &SessionSettings, role: Option<&str>) -> Vec<String> {
    let file = |n: &str| files.join(n).to_string_lossy().into_owned();
    let mut args: Vec<String> = vec!["--model".into(), settings.model.clone()];
    args.extend(ClaudeCode.effort_args(&settings.effort).unwrap_or_default());
    args.extend([
        "--permission-mode".into(),
        settings.permission_mode.clone(),
        "--settings".into(),
        crate::team::settings_file_for_role(files, settings.compact_window, role).to_string_lossy().into_owned(),
        "--mcp-config".into(),
        file("mcp.json"),
        "--strict-mcp-config".into(),
        // Leave out user settings so other tools' global hooks (Liaison, …) never see this session.
        "--setting-sources".into(),
        "project,local".into(),
        "--plugin-dir".into(),
        file("plugin"),
        "-n".into(),
        title.to_string(),
    ]);
    // One flag per tool: the options take several values, and a pattern may hold spaces.
    for tool in &settings.allowed_tools {
        args.extend(["--allowedTools".into(), tool.clone()]);
    }
    for tool in &settings.disallowed_tools {
        args.extend(["--disallowedTools".into(), tool.clone()]);
    }
    args
}

impl HarnessAdapter for ClaudeCode {
    fn harness(&self) -> Harness {
        Harness::ClaudeCode
    }

    fn caps(&self) -> Caps {
        Caps {
            coordinator: true,
            registers_itself: true,
            permission_routing: true,
            mcp: true,
            effort_levels: effort_levels(Harness::ClaudeCode),
            reports_cost: true,
            run_timeout: None,
        }
    }

    fn find(&self, config: &Config) -> Result<PathBuf, String> {
        find_program("claude", config.commands.claude.as_deref())
    }

    /// The plugin, settings and MCP config are written by the CLI (`team::prepare_files`), which
    /// holds the skills.
    fn install(&self, _files: &Path) -> std::io::Result<()> {
        Ok(())
    }

    fn worker(&self, w: &WorkerLaunch) -> Result<LaunchPlan, String> {
        let prompt = format!("/{PLUGIN_NAME}:{} {}", w.skill, brief_prompt(w.brief, w.brief_file));
        let mut args = vec!["-p".to_string(), prompt];
        args.extend(["--session-id".into(), w.session_id.to_string(), "--output-format".into(), "json".into()]);
        args.extend(session_args(w.files, w.title, w.settings));
        Ok(LaunchPlan {
            program: w.program.clone(),
            args,
            cwd: Some(w.cwd.to_path_buf()),
            path_prepend: None,
            wait: true,
            log: None,
            env: w.env.clone(),
        })
    }

    fn resume(&self, s: &Session, prompt: &str, config: &Config) -> Result<LaunchPlan, String> {
        // A worker answers one message headless, then exits again (it never listens).
        let program = self.find(config)?;
        let cwd = s.cwd.as_deref().map(PathBuf::from).filter(|p| p.is_dir());
        let role = s.role.clone().unwrap_or_default();
        let title = s.title.clone().unwrap_or_else(|| role.clone());
        let mut args = vec!["-p".to_string(), "--resume".into(), s.external_id.clone(), prompt.to_string()];
        args.extend(["--output-format".into(), "json".into()]);
        let mut settings = config.team.for_skill(&role, cwd.as_deref().unwrap_or(Path::new(".")));
        if settings.harness != Harness::ClaudeCode {
            // The role runs elsewhere, so this worker ran on the fallback.
            settings = settings.on_fallback().unwrap_or(settings);
        }
        // The model and effort it really ran with, when recorded (PRD H7).
        if let Some(model) = &s.model {
            settings.model = model.clone();
        }
        if let Some(effort) = s.effort.as_ref().filter(|e| crate::config::EFFORT_LEVELS.contains(&e.as_str())) {
            settings.effort = effort.clone();
        }
        args.extend(session_args(&crate::team::files_dir(), &title, &settings));
        let mut env = vec![(ROLE_ENV.to_string(), role)];
        if let Some(parent) = &s.parent_id {
            env.push((PARENT_ENV.into(), parent.clone()));
        }
        let log = crate::paths::data_dir().join("logs").join(format!("{}.log", s.id));
        let path_prepend = program.parent().map(Path::to_path_buf);
        Ok(LaunchPlan { program, args, cwd, path_prepend, wait: false, log: Some(log), env })
    }

    /// `result`, `is_error`, `total_cost_usd` and `usage` of `claude -p --output-format json`.
    fn parse_result(&self, stdout: &str, _stderr: &str, ok: bool) -> Outcome {
        let parsed: Option<Value> = stdout.lines().rev().find_map(|l| serde_json::from_str(l).ok());
        match parsed {
            Some(v) => {
                let tokens = |k: &str| v.get("usage").and_then(|u| u.get(k)).and_then(Value::as_i64);
                Outcome {
                    ok: ok && !v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    text: v.get("result").and_then(Value::as_str).unwrap_or("(no result)").to_string(),
                    cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
                    tokens_in: tokens("input_tokens"),
                    tokens_out: tokens("output_tokens"),
                    conversation: None,
                }
            }
            None => Outcome { ok: false, text: stdout.trim().to_string(), ..Outcome::default() },
        }
    }

    fn effort_args(&self, level: &str) -> Option<Vec<String>> {
        Some(vec!["--effort".into(), level.to_string()])
    }

    fn models(&self, _config: &Config) -> Result<Vec<String>, String> {
        Ok(MODELS.iter().map(|&s| s.to_string()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_builtin_models() {
        assert_eq!(ClaudeCode.models(&Config::default()).unwrap(), ["opus", "sonnet", "fable", "haiku"]);
    }

    #[test]
    fn workers_start_as_before_the_adapter() {
        let settings = Config::default().team.for_skill("tester", Path::new("/nonexistent"));
        let w = WorkerLaunch {
            program: "/bin/claude".into(),
            exe: "/bin/jg-team-dev".into(),
            files: Path::new("/f"),
            skill: "tester",
            title: "tester · x",
            brief: "check it",
            brief_file: Path::new("/b.md"),
            settings: &settings,
            cwd: Path::new("/w"),
            session_id: "uuid",
            env: vec![(ROLE_ENV.into(), "tester".into())],
        };
        let plan = ClaudeCode.worker(&w).unwrap();
        let args = plan.args.join(" ");
        assert!(args.starts_with("-p /jg-team-dev:tester check it --session-id uuid --output-format json --model sonnet --effort low"), "{args}");
        assert!(args.ends_with("--plugin-dir /f/plugin -n tester · x"), "{args}");
        assert_eq!((plan.cwd.as_deref(), plan.path_prepend, plan.env.len()), (Some(Path::new("/w")), None, 1));
    }

    #[test]
    fn results_read_the_json() {
        let out = ClaudeCode.parse_result(
            r#"{"is_error":false,"result":"done","total_cost_usd":0.25,"usage":{"input_tokens":10,"output_tokens":3}}"#,
            "",
            true,
        );
        assert_eq!((out.ok, out.text.as_str(), out.cost_usd, out.tokens_in, out.tokens_out), (true, "done", Some(0.25), Some(10), Some(3)));
        let failed = ClaudeCode.parse_result(r#"{"is_error":true,"result":"You've hit your session limit"}"#, "", true);
        assert!(!failed.ok);
        let garbage = ClaudeCode.parse_result("Error: something broke", "", false);
        assert_eq!((garbage.ok, garbage.text.as_str()), (false, "Error: something broke"));
        // A clean JSON from a process that exited non-zero is still a failure.
        assert!(!ClaudeCode.parse_result(r#"{"is_error":false,"result":"x"}"#, "", false).ok);
    }
}
