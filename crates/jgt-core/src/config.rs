//! `~/.config/jg-team-dev/config.toml`, shared by the hub, the CLI and the app.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub notifications: Notifications,
    pub timeouts: Timeouts,
    pub permissions: Permissions,
    pub commands: Commands,
    pub desktop: Desktop,
    pub team: Team,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Team {
    /// Model for team sessions (`claude --model`): an alias such as `opus`, or a full model name.
    pub model: String,
    /// Effort level (`claude --effort`).
    pub effort: String,
    /// Permission mode (`claude --permission-mode`); only prompts it can't settle reach the app.
    pub permission_mode: String,
    /// Per-skill settings (`[team.skills.coder]`, `[team.skills.coordinator]`, …).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub skills: BTreeMap<String, SkillSettings>,
}

impl Default for Team {
    fn default() -> Self {
        Self { model: "opus".into(), effort: "medium".into(), permission_mode: "auto".into(), skills: BTreeMap::new() }
    }
}

/// Settings that override the team defaults, for one skill or for one project.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Tools allowed without asking (`claude --allowedTools`), e.g. `["Bash(cargo test:*)"]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Tools the session may not use (`claude --disallowedTools`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    /// Context size in tokens at which Claude Code compacts the session (`autoCompactWindow`,
    /// 100000 to 1000000); 0 leaves it to Claude Code. The Coordinator defaults to
    /// [`COORDINATOR_COMPACT_WINDOW`]: it lives for days, and every step re-reads its whole context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_window: Option<u64>,
}

/// Default `compact_window` of the Coordinator.
pub const COORDINATOR_COMPACT_WINDOW: u64 = 120_000;

/// The range Claude Code accepts for `autoCompactWindow`.
pub const COMPACT_WINDOW_RANGE: (u64, u64) = (100_000, 1_000_000);

impl SkillSettings {
    /// Fills what `self` leaves unset from `other`.
    fn or(self, other: &SkillSettings) -> SkillSettings {
        SkillSettings {
            model: self.model.or_else(|| other.model.clone()),
            effort: self.effort.or_else(|| other.effort.clone()),
            permission_mode: self.permission_mode.or_else(|| other.permission_mode.clone()),
            allowed_tools: self.allowed_tools.or_else(|| other.allowed_tools.clone()),
            disallowed_tools: self.disallowed_tools.or_else(|| other.disallowed_tools.clone()),
            compact_window: self.compact_window.or(other.compact_window),
        }
    }
}

/// `.team/config.toml` in a project: the same `[team]` table as the user config, applied on top.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ProjectConfig {
    team: ProjectTeam,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ProjectTeam {
    #[serde(flatten)]
    defaults: SkillSettings,
    skills: BTreeMap<String, SkillSettings>,
}

/// What a team session with one skill is started with.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSettings {
    pub model: String,
    pub effort: String,
    pub permission_mode: String,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// `autoCompactWindow` for the session; `None` leaves it to Claude Code.
    pub compact_window: Option<u64>,
}

/// The project config file that applies to `folder`: the nearest `.team/config.toml` above it.
/// Found from a coder's worktree too, since worktrees live in `.team/worktrees/`.
pub fn project_config_path(folder: &Path) -> Option<PathBuf> {
    folder.ancestors().map(|d| d.join(".team").join("config.toml")).find(|p| p.is_file())
}

impl Team {
    /// Settings for `skill` in `folder`. The most specific wins: the project's `[team.skills.<skill>]`,
    /// the project's `[team]`, the user's `[team.skills.<skill>]`, then the user's `[team]`.
    pub fn for_skill(&self, skill: &str, folder: &Path) -> SessionSettings {
        let project = project_config_path(folder)
            .and_then(|path| {
                let text = std::fs::read_to_string(&path).ok()?;
                toml::from_str::<ProjectConfig>(&text)
                    .map_err(|e| eprintln!("jg-team-dev: ignoring invalid {}: {e}", path.display()))
                    .ok()
            })
            .unwrap_or_default();
        self.resolve(skill, &project.team)
    }

    fn resolve(&self, skill: &str, project: &ProjectTeam) -> SessionSettings {
        let none = SkillSettings::default();
        let merged = project
            .skills
            .get(skill)
            .cloned()
            .unwrap_or_default()
            .or(&project.defaults)
            .or(self.skills.get(skill).unwrap_or(&none));
        SessionSettings {
            model: merged.model.unwrap_or_else(|| self.model.clone()),
            effort: merged.effort.unwrap_or_else(|| self.effort.clone()),
            permission_mode: merged.permission_mode.unwrap_or_else(|| self.permission_mode.clone()),
            allowed_tools: merged.allowed_tools.unwrap_or_default(),
            disallowed_tools: merged.disallowed_tools.unwrap_or_default(),
            compact_window: match merged.compact_window {
                Some(0) => None,
                Some(w) => Some(w.clamp(COMPACT_WINDOW_RANGE.0, COMPACT_WINDOW_RANGE.1)),
                None => (skill == crate::model::COORDINATOR).then_some(COORDINATOR_COMPACT_WINDOW),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Desktop {
    /// `system`, `light` or `dark`.
    pub theme: String,
    /// UI font family (a static, non-variable font); Noto Sans, DejaVu Sans or Liberation Sans when unset.
    pub font: Option<String>,
    /// Scales the whole window (1.0 = 100%).
    pub zoom: f32,
    /// Size of chat text in pixels; the rest of the text scales with it.
    pub text_size: u16,
    /// Show the jg-team-dev icon in the system tray (StatusNotifierItem), with a dot for unread messages.
    pub tray: bool,
}

impl Desktop {
    pub const ZOOM_RANGE: (f32, f32) = (0.8, 2.0);
    pub const TEXT_SIZE_RANGE: (u16, u16) = (12, 24);
    pub const DEFAULT_TEXT_SIZE: u16 = 16;
}

impl Default for Desktop {
    fn default() -> Self {
        Self { theme: "system".into(), font: None, zoom: 1.0, text_size: Self::DEFAULT_TEXT_SIZE, tray: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    /// When agents' permission prompts go to the app:
    /// `auto` — when nobody is at the agent's terminal (background session or listen mode on),
    /// `always`, or `never` (the terminal prompt only).
    pub route: String,
}

impl Default for Permissions {
    fn default() -> Self {
        Self { route: "auto".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    pub enabled: bool,
    /// Notify for plain chat messages too, not only notifications/questions/permissions.
    pub messages: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self { enabled: true, messages: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Timeouts {
    /// How long a permission request waits for you before falling back to the terminal prompt.
    pub permission_secs: u64,
    /// Default wait for `chat_ask`.
    pub question_secs: u64,
    /// How long a session in listen mode stays parked when its turn ends.
    pub listen_secs: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { permission_secs: 120, question_secs: 900, listen_secs: 3000 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Commands {
    /// Absolute path to `claude`; recorded by `jg-team-dev connect claude-code`.
    pub claude: Option<String>,
    /// Absolute path to `opencode`; recorded by `jg-team-dev connect opencode`.
    pub opencode: Option<String>,
    /// Terminal used to reopen sessions. `{cwd}` is replaced by the session folder and the
    /// agent command is appended, e.g. `["kitty", "--directory", "{cwd}", "--"]`.
    pub terminal: Option<Vec<String>>,
}

impl Config {
    pub fn load() -> Config {
        Self::load_from(&crate::paths::config_path())
    }

    pub fn load_from(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                eprintln!("jg-team-dev: ignoring invalid {}: {e}", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = crate::paths::config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_most_specific_setting_wins() {
        let user: Config = toml::from_str(
            r#"
            [team]
            model = "opus"
            [team.skills.coder]
            model = "sonnet"
            effort = "high"
            [team.skills.tester]
            allowed_tools = ["Bash(cargo test:*)"]
            "#,
        )
        .unwrap();
        let project: ProjectConfig = toml::from_str(
            r#"
            [team]
            effort = "low"
            [team.skills.coder]
            model = "haiku"
            "#,
        )
        .unwrap();
        let coder = user.team.resolve("coder", &project.team);
        assert_eq!((coder.model.as_str(), coder.effort.as_str()), ("haiku", "low"));
        let tester = user.team.resolve("tester", &ProjectTeam::default());
        assert_eq!((tester.model.as_str(), tester.effort.as_str()), ("opus", "medium"));
        assert_eq!(tester.allowed_tools, ["Bash(cargo test:*)"]);
        assert_eq!(tester.permission_mode, "auto");
        assert_eq!(tester.compact_window, None);
    }

    #[test]
    fn the_coordinator_compacts_early_unless_told_otherwise() {
        let none = ProjectTeam::default();
        let team = Config::default().team;
        assert_eq!(team.resolve("coordinator", &none).compact_window, Some(COORDINATOR_COMPACT_WINDOW));
        let user: Config = toml::from_str(
            "[team.skills.coordinator]\ncompact_window = 0\n[team.skills.coder]\ncompact_window = 5000\n",
        )
        .unwrap();
        assert_eq!(user.team.resolve("coordinator", &none).compact_window, None);
        assert_eq!(user.team.resolve("coder", &none).compact_window, Some(COMPACT_WINDOW_RANGE.0));
    }

    #[test]
    fn the_project_file_is_found_from_a_worktree() {
        let root = std::env::temp_dir().join(format!("jgt-cfg-{}", std::process::id()));
        let wt = root.join(".team/worktrees/fix");
        std::fs::create_dir_all(&wt).unwrap();
        assert_eq!(project_config_path(&wt), None);
        std::fs::write(root.join(".team/config.toml"), "[team.skills.coder]\nmodel = \"sonnet\"\n").unwrap();
        assert_eq!(project_config_path(&wt), Some(root.join(".team/config.toml")));
        assert_eq!(Config::default().team.for_skill("coder", &wt).model, "sonnet");
        assert_eq!(Config::default().team.for_skill("planner", &wt).model, "opus");
        let _ = std::fs::remove_dir_all(&root);
    }
}
