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

    /// Atomically writes `<folder>/.team/agents.json`.
    pub fn write(&self, folder: &Path) -> std::io::Result<()> {
        let path = folder.join(".team").join("agents.json");
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::board::write_atomic(&path, &text)
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
