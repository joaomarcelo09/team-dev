//! `jg-team-dev agents`: view and update team agent settings (`.team/agents.json`).

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use jgt_core::{
    Harness, TeamAgents, agents_path,
    config::{self, Config},
};
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct AgentsArgs {
    /// Workspace folder. Default: current folder (or nearest project folder).
    #[arg(long, global = true)]
    pub folder: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<AgentsCommand>,
}

#[derive(Subcommand, Debug)]
pub enum AgentsCommand {
    /// Change the harness, model or effort for a role.
    Set {
        /// The role to change (coordinator, coder, reviewer).
        role: String,
        /// Harness (claude-code, opencode, antigravity).
        #[arg(long, value_parser = crate::parse_harness)]
        harness: Option<Harness>,
        /// Model name or alias.
        #[arg(long)]
        model: Option<String>,
        /// Effort level.
        #[arg(long)]
        effort: Option<String>,
    },
}

pub fn resolve_folder(folder: Option<PathBuf>) -> Result<PathBuf> {
    let folder = folder.map(Ok).unwrap_or_else(std::env::current_dir)?;
    let folder = folder
        .canonicalize()
        .with_context(|| format!("{} does not exist", folder.display()))?;
    if let Some(proj) = agents_path(&folder).and_then(|p| p.parent()?.parent().map(Path::to_path_buf)) {
        return Ok(proj);
    }
    Ok(folder)
}

pub async fn run(args: AgentsArgs) -> Result<()> {
    let folder = resolve_folder(args.folder)?;
    execute(&folder, args.command)
}

pub fn execute(folder: &Path, command: Option<AgentsCommand>) -> Result<()> {
    match command {
        None => {
            let agents = TeamAgents::load(folder)
                .unwrap_or_else(|| TeamAgents::resolve(&Config::load().team, folder));
            for role in config::roles() {
                if let Some(agent) = agents.roles.get(role) {
                    println!("{}", agent.describe(role));
                }
            }
            Ok(())
        }
        Some(AgentsCommand::Set {
            role,
            harness,
            model,
            effort,
        }) => {
            let mut agents = TeamAgents::load(folder)
                .unwrap_or_else(|| TeamAgents::resolve(&Config::load().team, folder));
            agents
                .set_role(&role, harness, model, effort)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            agents
                .write(folder)
                .with_context(|| format!("writing .team/agents.json for {}", folder.display()))?;
            if let Some(agent) = agents.roles.get(&role) {
                println!("{}", agent.describe(&role));
            }
            if role == jgt_core::COORDINATOR {
                println!("the Coordinator row applies after a reset");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jgt-cli-agents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_agents_execute_list_and_set() {
        let dir = temp_dir("list-set");

        // Execute list on fresh folder (no agents.json yet)
        assert!(execute(&dir, None).is_ok());

        // Set coder to custom model and effort
        let set_cmd = AgentsCommand::Set {
            role: "coder".into(),
            harness: Some(Harness::ClaudeCode),
            model: Some("haiku".into()),
            effort: Some("low".into()),
        };
        assert!(execute(&dir, Some(set_cmd)).is_ok());

        // Verify agents.json was created and reflects the update
        let loaded = TeamAgents::load(&dir).expect("agents.json should exist");
        let coder = loaded.roles.get("coder").unwrap();
        assert_eq!(coder.harness, Harness::ClaudeCode);
        assert_eq!(coder.model, "haiku");
        assert_eq!(coder.effort, "low");

        // Partial update: update effort only
        let set_cmd2 = AgentsCommand::Set {
            role: "coder".into(),
            harness: None,
            model: None,
            effort: Some("high".into()),
        };
        assert!(execute(&dir, Some(set_cmd2)).is_ok());
        let loaded = TeamAgents::load(&dir).unwrap();
        let coder = loaded.roles.get("coder").unwrap();
        assert_eq!(coder.model, "haiku");
        assert_eq!(coder.effort, "high");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_agents_set_coordinator() {
        let dir = temp_dir("coord");

        let set_cmd = AgentsCommand::Set {
            role: "coordinator".into(),
            harness: None,
            model: Some("sonnet".into()),
            effort: Some("low".into()),
        };
        assert!(execute(&dir, Some(set_cmd)).is_ok());

        let loaded = TeamAgents::load(&dir).unwrap();
        let coord = loaded.roles.get("coordinator").unwrap();
        assert_eq!(coord.harness, Harness::ClaudeCode);
        assert_eq!(coord.model, "sonnet");
        assert_eq!(coord.effort, "low");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_agents_set_invalid_role_fails() {
        let dir = temp_dir("invalid-role");
        let set_cmd = AgentsCommand::Set {
            role: "planner".into(),
            harness: None,
            model: Some("opus".into()),
            effort: None,
        };
        let err = execute(&dir, Some(set_cmd)).unwrap_err();
        assert!(err.to_string().contains("unknown role `planner`"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_agents_set_invalid_effort_fails() {
        let dir = temp_dir("invalid-effort");
        let set_cmd = AgentsCommand::Set {
            role: "coder".into(),
            harness: Some(Harness::ClaudeCode),
            model: Some("opus".into()),
            effort: Some("turbo".into()),
        };
        let err = execute(&dir, Some(set_cmd)).unwrap_err();
        assert!(err.to_string().contains("unknown effort `turbo`"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_agents_set_coordinator_non_claude_fails() {
        let dir = temp_dir("coord-non-claude");
        let set_cmd = AgentsCommand::Set {
            role: "coordinator".into(),
            harness: Some(Harness::Antigravity),
            model: Some("gemini".into()),
            effort: None,
        };
        let err = execute(&dir, Some(set_cmd)).unwrap_err();
        assert!(
            err.to_string().contains("the Coordinator runs on Claude Code only"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_agents_set_empty_model_fails() {
        let dir = temp_dir("empty-model");
        let set_cmd = AgentsCommand::Set {
            role: "coder".into(),
            harness: None,
            model: Some("".into()),
            effort: None,
        };
        let err = execute(&dir, Some(set_cmd)).unwrap_err();
        assert!(err.to_string().contains("model cannot be empty"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
