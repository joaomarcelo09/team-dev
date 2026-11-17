//! Claude DS: Claude Code pointed at another config dir, whose `settings.json` routes it to
//! another endpoint (DeepSeek). Team workers only; the Coordinator stays on Claude Code. A worker
//! runs exactly as a Claude Code worker (`ClaudeCode::worker`), started as the user's alias
//! `env -u ANTHROPIC_API_KEY CLAUDE_CONFIG_DIR="$HOME/.claude-opencode" claude`:
//!
//! - `CLAUDE_CONFIG_DIR` is the config dir (`[commands] claude_ds_config_dir`, default
//!   `~/.claude-opencode`); jg-team-dev never reads that dir's files, which hold the endpoint and
//!   its token.
//! - `ANTHROPIC_API_KEY` is removed, so the dir's own credentials apply.
//! - `--setting-sources user,project,local`: the user source is the config dir's `settings.json`
//!   (the real `~/.claude` is not loaded, since `CLAUDE_CONFIG_DIR` replaces it).
//! - The model `deepseek` (`MODEL`) passes no `--model`: the dir's `ANTHROPIC_MODEL` decides.
//!   Any other model name is passed as is.

use super::{Caps, ClaudeCode, HarnessAdapter, Outcome, WorkerLaunch, effort_levels};
use crate::{
    config::Config,
    launch::LaunchPlan,
    model::{Harness, Session},
};
use std::path::PathBuf;

pub struct ClaudeDs;

/// The one built-in model: whatever the config dir's `ANTHROPIC_MODEL` says (no `--model`).
pub const MODEL: &str = "deepseek";

/// The config dir when `[commands] claude_ds_config_dir` is not set, under the home folder.
pub const DEFAULT_CONFIG_DIR: &str = ".claude-opencode";

/// The variable that points Claude Code at the config dir.
pub const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Removed from the worker's environment so the config dir's credentials apply.
pub const REMOVED_ENV: &[&str] = &["ANTHROPIC_API_KEY"];

/// The config dir: `[commands] claude_ds_config_dir` (a leading `~/` is the home folder), else
/// `~/.claude-opencode`.
pub fn config_dir(config: &Config) -> PathBuf {
    let home = crate::paths::home_dir();
    match config.commands.claude_ds_config_dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(dir) => match dir.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(dir),
        },
        None => home.join(DEFAULT_CONFIG_DIR),
    }
}

/// Turns a Claude Code plan into the Claude DS one (see the module docs).
fn adapt(mut plan: LaunchPlan, config_dir: PathBuf) -> LaunchPlan {
    // Our flags follow the prompt, so the last occurrence is ours.
    if let Some(i) = plan.args.iter().rposition(|a| a == "--setting-sources") {
        if let Some(value) = plan.args.get_mut(i + 1) {
            *value = "user,project,local".into();
        }
    }
    if let Some(i) = plan.args.iter().rposition(|a| a == "--model") {
        if plan.args.get(i + 1).is_some_and(|m| m == MODEL) {
            plan.args.drain(i..i + 2);
        }
    }
    plan.env.retain(|(k, _)| k != CONFIG_DIR_ENV);
    plan.env.push((CONFIG_DIR_ENV.into(), config_dir.to_string_lossy().into_owned()));
    plan.env_remove.extend(REMOVED_ENV.iter().map(|k| k.to_string()));
    plan
}

impl HarnessAdapter for ClaudeDs {
    fn harness(&self) -> Harness {
        Harness::ClaudeDs
    }

    fn caps(&self) -> Caps {
        Caps { coordinator: false, effort_levels: effort_levels(Harness::ClaudeDs), ..ClaudeCode.caps() }
    }

    /// The same `claude` program, only where the config dir exists.
    fn find(&self, config: &Config) -> Result<PathBuf, String> {
        let dir = config_dir(config);
        if !dir.is_dir() {
            return Err(format!(
                "the Claude DS config dir {} does not exist; set [commands] claude_ds_config_dir",
                dir.display()
            ));
        }
        ClaudeCode.find(config)
    }

    fn install(&self, files: &std::path::Path) -> std::io::Result<()> {
        ClaudeCode.install(files)
    }

    fn worker(&self, w: &WorkerLaunch) -> Result<LaunchPlan, String> {
        Ok(adapt(ClaudeCode.worker(w)?, config_dir(&Config::load())))
    }

    fn resume(&self, s: &Session, prompt: &str, config: &Config) -> Result<LaunchPlan, String> {
        Ok(adapt(ClaudeCode.resume_on(Harness::ClaudeDs, s, prompt, config)?, config_dir(config)))
    }

    fn parse_result(&self, stdout: &str, stderr: &str, ok: bool) -> Outcome {
        ClaudeCode.parse_result(stdout, stderr, ok)
    }

    fn effort_args(&self, level: &str) -> Option<Vec<String>> {
        ClaudeCode.effort_args(level)
    }

    fn models(&self, _config: &Config) -> Result<Vec<String>, String> {
        Ok(vec![MODEL.to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::SessionSettings, team::ROLE_ENV};
    use std::path::Path;

    fn launch_plan_for(skill: &str, settings: &SessionSettings) -> LaunchPlan {
        let title = format!("{skill} · x");
        let w = WorkerLaunch {
            program: "/bin/claude".into(),
            exe: "/bin/jg-team-dev".into(),
            files: Path::new("/f"),
            skill,
            title: &title,
            brief: "do it",
            brief_file: Path::new("/b.md"),
            settings,
            cwd: Path::new("/w"),
            session_id: "uuid",
            env: vec![(ROLE_ENV.into(), skill.into())],
        };
        ClaudeDs.worker(&w).unwrap()
    }

    fn launch_plan(settings: &SessionSettings) -> LaunchPlan {
        launch_plan_for("coder", settings)
    }

    fn settings(model: &str) -> SessionSettings {
        let mut s = Config::default().team.for_skill("reviewer", Path::new("/nonexistent"));
        s.harness = Harness::ClaudeDs;
        s.model = model.into();
        s
    }

    #[test]
    fn harness_parses_and_labels() {
        assert_eq!("claude-ds".parse::<Harness>(), Ok(Harness::ClaudeDs));
        assert_eq!(Harness::ClaudeDs.as_str(), "claude-ds");
        assert_eq!(Harness::ClaudeDs.label(), "Claude DS");
        assert_eq!(Harness::ClaudeDs.session_harness(), Harness::ClaudeCode);
        assert_eq!(Harness::Opencode.session_harness(), Harness::Opencode);
        assert_eq!(ClaudeDs.models(&Config::default()).unwrap(), [MODEL]);
        assert!(!ClaudeDs.caps().coordinator);
    }

    #[test]
    fn workers_run_claude_code_on_the_config_dir() {
        let plan = launch_plan(&settings(MODEL));
        let args = plan.args.join(" ");
        // The placeholder model leaves the choice to the config dir.
        assert!(args.starts_with("-p /jg-team-dev:coder do it --session-id uuid --output-format json --effort medium"), "{args}");
        assert!(!plan.args.contains(&"--model".to_string()), "{args}");
        assert!(args.contains("--setting-sources user,project,local"), "{args}");
        assert!(args.contains("--plugin-dir /f/plugin -n coder · x"), "{args}");
        assert!(args.ends_with("--disallowedTools Bash(gh*)"), "{args}");
        assert!(plan.env.iter().any(|(k, v)| k == CONFIG_DIR_ENV && !v.is_empty()), "{:?}", plan.env);
        assert!(plan.env.contains(&(ROLE_ENV.into(), "coder".into())));
        assert_eq!(plan.env_remove, ["ANTHROPIC_API_KEY"]);
    }

    #[test]
    fn only_the_git_coder_may_run_git_writes_and_gh() {
        let mut git_coder = Config::default().team.for_skill("git-coder", Path::new("/nonexistent"));
        git_coder.harness = Harness::ClaudeDs;
        let args = launch_plan_for("git-coder", &git_coder).args.join(" ");
        for tool in crate::team::git_write_tools() {
            assert!(args.contains(&format!("--allowedTools {tool}")), "{args}");
        }
        for skill in ["coder", "reviewer"] {
            let mut settings = Config::default().team.for_skill(skill, Path::new("/nonexistent"));
            settings.harness = Harness::ClaudeDs;
            let args = launch_plan_for(skill, &settings).args.join(" ");
            for tool in crate::team::git_write_tools() {
                assert!(args.contains(&format!("--disallowedTools {tool}")), "{skill}: {args}");
            }
        }
    }

    #[test]
    fn other_model_names_pass_as_is() {
        let plan = launch_plan(&settings("deepseek-v4-pro"));
        assert!(plan.args.join(" ").contains("--model deepseek-v4-pro --effort medium"), "{:?}", plan.args);
    }

    #[test]
    fn config_dir_defaults_and_overrides() {
        let mut config = Config::default();
        assert_eq!(config_dir(&config), crate::paths::home_dir().join(".claude-opencode"));
        config.commands.claude_ds_config_dir = Some("~/.ds".into());
        assert_eq!(config_dir(&config), crate::paths::home_dir().join(".ds"));
        config.commands.claude_ds_config_dir = Some("/opt/ds".into());
        assert_eq!(config_dir(&config), PathBuf::from("/opt/ds"));
        let plan = adapt(launch_plan(&settings(MODEL)), config_dir(&config));
        assert_eq!(plan.env.iter().filter(|(k, _)| k == CONFIG_DIR_ENV).collect::<Vec<_>>(), [&(CONFIG_DIR_ENV.to_string(), "/opt/ds".to_string())]);
    }

    #[test]
    fn missing_config_dir_is_not_installed() {
        let mut config = Config::default();
        config.commands.claude_ds_config_dir = Some("/nonexistent/jgt-claude-ds".into());
        assert!(ClaudeDs.find(&config).unwrap_err().contains("does not exist"));
        assert!(!super::super::installed(&config).contains(&Harness::ClaudeDs));
    }
}
