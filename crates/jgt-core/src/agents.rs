//! Per-team saved agent settings (`<folder>/.team/agents.json`).
//!
//! When a team starts, the resolved harness, model and effort of every role are saved so that
//! workers and restarts continue with the same settings even if `config.toml` is edited later.

use crate::{
    config::{self, Team},
    model::Harness,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// The harness, model and effort saved for one role in a running team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSetting {
    pub harness: Harness,
    pub model: String,
    pub effort: String,
}

pub type AgentSettings = AgentSetting;

impl AgentSetting {
    /// `coder · Antigravity · gemini-3.8-flash-medium · medium`, matching `SessionSettings::describe`.
    pub fn describe(&self, role: &str) -> String {
        format!("{role} · {} · {} · {}", self.harness.label(), self.model, self.effort)
    }
}

/// The settings saved when a team starts (`<folder>/.team/agents.json`).
///
/// Workers and Coordinator restarts in that folder (or any worktree under it) use these settings
/// rather than re-reading changes to `~/.config/jg-team-dev/config.toml` or `.team/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamAgents {
    pub roles: BTreeMap<String, AgentSetting>,
}

/// The agents file that applies to `folder`: the nearest `.team/agents.json` above it.
/// Found from a coder's worktree too, since worktrees live in `.team/worktrees/`.
pub fn agents_path(folder: &Path) -> Option<PathBuf> {
    folder.ancestors().map(|d| d.join(".team").join("agents.json")).find(|p| p.is_file())
}

impl TeamAgents {
    /// Resolves the current configured settings for every role in `roles()`.
    pub fn resolve(team: &Team, folder: &Path) -> TeamAgents {
        let mut roles = BTreeMap::new();
        for role in config::roles() {
            let (settings, _) = team.resolve_unlocked(role, folder);
            roles.insert(
                role.to_string(),
                AgentSetting {
                    harness: settings.harness,
                    model: settings.model,
                    effort: settings.effort,
                },
            );
        }
        TeamAgents { roles }
    }

    /// Loads the saved agents from `agents_path(folder)`, if present and valid.
    pub fn load(folder: &Path) -> Option<TeamAgents> {
        let path = agents_path(folder)?;
        let text = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str::<TeamAgents>(&text)
            .map_err(|e| eprintln!("jg-team-dev: ignoring invalid {}: {e}", path.display()))
            .ok()
    }

    /// Atomically writes `<folder>/.team/agents.json`, dropping any stale roles not in `config::roles()`.
    pub fn write(&self, folder: &Path) -> std::io::Result<()> {
        let mut pruned = self.clone();
        pruned.prune();
        let path = folder.join(".team").join("agents.json");
        let text = serde_json::to_string_pretty(&pruned).map_err(std::io::Error::other)?;
        crate::board::write_atomic(&path, &text)
    }

    /// Removes any roles not in `config::roles()`, such as stale `planner` entries.
    pub fn prune(&mut self) {
        let valid: std::collections::HashSet<&'static str> = config::roles().collect();
        self.roles.retain(|role, _| valid.contains(role.as_str()));
    }

    /// Updates the settings for `role`, validating harness, model, and effort.
    ///
    /// - `role` must be one of `config::roles()`.
    /// - The Coordinator runs on Claude Code only.
    /// - Harness `Custom` is not a valid agent harness.
    /// - Model cannot be empty.
    /// - Effort must be one of `harness::effort_levels(harness)` when that list is non-empty
    ///   (for harnesses with no levels, the effort is kept/ignored).
    pub fn set_role(
        &mut self,
        role: &str,
        harness: Option<Harness>,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<(), String> {
        let valid_roles: Vec<&'static str> = config::roles().collect();
        if !valid_roles.contains(&role) {
            return Err(format!(
                "unknown role `{role}` (use {})",
                valid_roles.join(", ")
            ));
        }

        let current = self.roles.get(role);
        let default_settings = config::role_defaults(role);

        let new_harness = harness
            .or_else(|| current.map(|c| c.harness))
            .or_else(|| default_settings.harness.as_deref().and_then(|h| h.parse::<Harness>().ok()))
            .unwrap_or(Harness::ClaudeCode);

        if new_harness == Harness::Custom {
            return Err(format!(
                "unknown harness `{}` (use claude-code, opencode or antigravity)",
                new_harness.as_str()
            ));
        }

        if role == crate::model::COORDINATOR && new_harness != Harness::ClaudeCode {
            return Err(format!(
                "coordinator: the Coordinator runs on Claude Code only, not {}",
                new_harness.label()
            ));
        }

        let new_model = model
            .or_else(|| current.map(|c| c.model.clone()))
            .or(default_settings.model)
            .unwrap_or_else(|| config::DEFAULT_MODEL.to_string());
        let new_model = new_model.trim().to_string();
        if new_model.is_empty() {
            return Err("model cannot be empty".to_string());
        }

        let levels = crate::harness::effort_levels(new_harness);
        let new_effort = effort
            .or_else(|| current.map(|c| c.effort.clone()))
            .or(default_settings.effort)
            .unwrap_or_else(|| config::DEFAULT_EFFORT.to_string());
        let new_effort = new_effort.trim().to_string();

        if !levels.is_empty() && !levels.contains(&new_effort.as_str()) {
            return Err(format!(
                "unknown effort `{new_effort}` (use {})",
                levels.join(", ")
            ));
        }

        self.roles.insert(
            role.to_string(),
            AgentSetting {
                harness: new_harness,
                model: new_model,
                effort: new_effort,
            },
        );
        Ok(())
    }

    /// Formats the agents as `coordinator · Claude Code · opus · medium, coder · …`.
    pub fn summary(&self) -> String {
        let mut seen = std::collections::HashSet::new();
        let mut parts = Vec::new();
        for role in config::roles() {
            if let Some(agent) = self.roles.get(role) {
                seen.insert(role);
                parts.push(agent.describe(role));
            }
        }
        for (role, agent) in &self.roles {
            if !seen.contains(role.as_str()) {
                parts.push(agent.describe(role));
            }
        }
        parts.join(", ")
    }

    /// Formats as `Agents: coordinator · Claude Code · opus · medium, coder · …`.
    pub fn describe(&self) -> String {
        format!("Agents: {}", self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_role_valid_updates() {
        let mut agents = TeamAgents::default();
        agents
            .set_role(
                "coder",
                Some(Harness::Antigravity),
                Some("gemini-1.5-pro".into()),
                Some("high".into()),
            )
            .unwrap();
        let coder = agents.roles.get("coder").unwrap();
        assert_eq!(coder.harness, Harness::Antigravity);
        assert_eq!(coder.model, "gemini-1.5-pro");
        assert_eq!(coder.effort, "high");

        // Partial update: change only model
        agents
            .set_role("coder", None, Some("gemini-2.5-pro".into()), None)
            .unwrap();
        let coder = agents.roles.get("coder").unwrap();
        assert_eq!(coder.harness, Harness::Antigravity);
        assert_eq!(coder.model, "gemini-2.5-pro");
        assert_eq!(coder.effort, "high");
    }

    #[test]
    fn test_set_role_unknown_role_rejected() {
        let mut agents = TeamAgents::default();
        let err = agents
            .set_role("planner", Some(Harness::ClaudeCode), Some("opus".into()), None)
            .unwrap_err();
        assert!(err.contains("unknown role `planner`"), "{err}");
    }

    #[test]
    fn test_set_role_coordinator_only_claude_code() {
        let mut agents = TeamAgents::default();
        let err = agents
            .set_role(
                crate::model::COORDINATOR,
                Some(Harness::Opencode),
                Some("opus".into()),
                None,
            )
            .unwrap_err();
        assert!(
            err.contains("the Coordinator runs on Claude Code only"),
            "{err}"
        );
    }

    #[test]
    fn test_set_role_empty_model_rejected() {
        let mut agents = TeamAgents::default();
        let err = agents
            .set_role("coder", Some(Harness::ClaudeCode), Some("  ".into()), None)
            .unwrap_err();
        assert!(err.contains("model cannot be empty"), "{err}");
    }

    #[test]
    fn test_set_role_effort_validation() {
        let mut agents = TeamAgents::default();
        // Invalid effort on ClaudeCode
        let err = agents
            .set_role(
                "coder",
                Some(Harness::ClaudeCode),
                Some("opus".into()),
                Some("turbo".into()),
            )
            .unwrap_err();
        assert!(err.contains("unknown effort `turbo`"), "{err}");

        // Opencode has no effort levels, so arbitrary effort is allowed
        agents
            .set_role(
                "coder",
                Some(Harness::Opencode),
                Some("qwen".into()),
                Some("any-effort".into()),
            )
            .unwrap();
        assert_eq!(agents.roles.get("coder").unwrap().effort, "any-effort");
    }

    #[test]
    fn test_prune_and_write_drops_stale_roles() {
        let dir = std::env::temp_dir().join(format!("jgt-agents-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut agents = TeamAgents::default();
        agents.roles.insert(
            "planner".into(),
            AgentSetting {
                harness: Harness::ClaudeCode,
                model: "opus".into(),
                effort: "medium".into(),
            },
        );
        agents.roles.insert(
            "coder".into(),
            AgentSetting {
                harness: Harness::ClaudeCode,
                model: "sonnet".into(),
                effort: "low".into(),
            },
        );

        // write prunes automatically
        agents.write(&dir).unwrap();

        let loaded = TeamAgents::load(&dir).unwrap();
        assert!(!loaded.roles.contains_key("planner"));
        assert!(loaded.roles.contains_key("coder"));

        // prune() directly
        agents.prune();
        assert!(!agents.roles.contains_key("planner"));
        assert!(agents.roles.contains_key("coder"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
