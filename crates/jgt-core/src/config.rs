//! `~/.config/jg-team-dev/config.toml`, shared by the hub, the CLI and the app.

pub use crate::agents::{agents_path, AgentSetting, AgentSettings, TeamAgents};
use crate::model::Harness;
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
    pub git: Git,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Team {
    /// Harness for team sessions: `claude-code`, `opencode` or `antigravity`. Below each role's
    /// built-in default (`role_defaults`), so it applies to roles without one; else `claude-code`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub harness: String,
    /// Model for team sessions: an alias such as `opus` (Claude Code), `provider/model` (OpenCode),
    /// or a full model name, on this table's harness. Below each role's built-in default; else `opus`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Effort level (`EFFORT_LEVELS`). Below each role's built-in default; else `medium`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// Permission mode (`claude --permission-mode`); only prompts it can't settle reach the app.
    pub permission_mode: String,
    /// Per-skill settings (`[team.skills.coder]`, `[team.skills.coordinator]`, …).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub skills: BTreeMap<String, SkillSettings>,
    /// How many Board tasks the team works at once (Working, Testing, Review), 1 to 3 (K12).
    pub wip_limit: u8,
}

impl Default for Team {
    fn default() -> Self {
        Self {
            harness: String::new(),
            model: String::new(),
            effort: String::new(),
            permission_mode: "auto".into(),
            skills: BTreeMap::new(),
            wip_limit: 1,
        }
    }
}

/// The range a WIP limit is clamped to: at least one task, at most the 3-worker limit.
pub const WIP_LIMIT_RANGE: (u8, u8) = (1, 3);

/// Model and effort (on `claude-code`) when neither the config nor the role's built-in default sets them.
pub const DEFAULT_MODEL: &str = "opus";
pub const DEFAULT_EFFORT: &str = "medium";

/// Every effort level a role may name, lowest first. Each harness supports some of them
/// (`harness::effort_levels`).
pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Settings that override the team defaults, for one skill or for one project.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillSettings {
    /// `claude-code`, `opencode` or `antigravity`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
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
    /// What a worker runs on instead when `harness` cannot run here (its program is missing):
    /// `[team.skills.coder.fallback]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<FallbackSettings>,
}

/// `[team.skills.<role>.fallback]`: harness, model and effort used when the role's harness
/// cannot run. Unset fields come from the role's resolved settings on `claude-code`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FallbackSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Default `compact_window` of the Coordinator.
pub const COORDINATOR_COMPACT_WINDOW: u64 = 120_000;

/// The range Claude Code accepts for `autoCompactWindow`.
pub const COMPACT_WINDOW_RANGE: (u64, u64) = (100_000, 1_000_000);

impl SkillSettings {
    /// Fills what `self` leaves unset from `other`.
    fn or(self, other: &SkillSettings) -> SkillSettings {
        SkillSettings {
            harness: self.harness.or_else(|| other.harness.clone()),
            model: self.model.or_else(|| other.model.clone()),
            effort: self.effort.or_else(|| other.effort.clone()),
            permission_mode: self.permission_mode.or_else(|| other.permission_mode.clone()),
            allowed_tools: self.allowed_tools.or_else(|| other.allowed_tools.clone()),
            disallowed_tools: self.disallowed_tools.or_else(|| other.disallowed_tools.clone()),
            compact_window: self.compact_window.or(other.compact_window),
            fallback: self.fallback.or_else(|| other.fallback.clone()),
        }
    }
}

/// The built-in settings of a role (PRD E1). A role setting in the user's or the project's config
/// replaces their harness, model and effort as a whole; a `[team]` value does not (`run_unit`).
///
/// The coder's default runs on Antigravity; where `agy` is missing it falls back to Claude Code
/// with `sonnet`. `opus` is Claude Code's alias for its newest Opus model (Opus 5.5 today).
pub fn role_defaults(role: &str) -> SkillSettings {
    let set = |harness: Option<&str>, model: &str, effort: &str| SkillSettings {
        harness: harness.map(str::to_owned),
        model: Some(model.into()),
        effort: Some(effort.into()),
        ..SkillSettings::default()
    };
    match role {
        crate::model::COORDINATOR => set(Some("claude-code"), "opus", "medium"),
        "planner" => set(Some("claude-code"), "opus", "medium"),
        "coder" => SkillSettings {
            fallback: Some(FallbackSettings {
                harness: Some("claude-code".into()),
                model: Some("sonnet".into()),
                effort: Some("medium".into()),
            }),
            ..set(Some("antigravity"), "gemini-3.8-flash-high", "high")
        },
        "reviewer" => set(Some("claude-code"), "opus", "medium"),
        "git-coder" => set(Some("claude-code"), "sonnet", "low"),
        _ => SkillSettings::default(),
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
    wip_limit: Option<u8>,
}

/// What a team session with one skill is started with.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSettings {
    pub harness: Harness,
    pub model: String,
    pub effort: String,
    pub permission_mode: String,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// `autoCompactWindow` for the session; `None` leaves it to Claude Code.
    pub compact_window: Option<u64>,
    /// What to run on when `harness` cannot run here.
    pub fallback: Option<Fallback>,
}

/// A resolved `[team.skills.<role>.fallback]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Fallback {
    pub harness: Harness,
    pub model: String,
    pub effort: String,
}

impl SessionSettings {
    /// These settings on the fallback harness, if the role has one.
    pub fn on_fallback(&self) -> Option<SessionSettings> {
        let f = self.fallback.as_ref()?;
        Some(SessionSettings { harness: f.harness, model: f.model.clone(), effort: f.effort.clone(), fallback: None, ..self.clone() })
    }

    /// `coder · Antigravity · gemini-3.8-flash-high · high`, for chat headers and logs.
    pub fn describe(&self, role: &str) -> String {
        format!("{role} · {} · {} · {}", self.harness.label(), self.model, self.effort)
    }
}

/// The project config file that applies to `folder`: the nearest `.team/config.toml` above it.
/// Found from a coder's worktree too, since worktrees live in `.team/worktrees/`.
pub fn project_config_path(folder: &Path) -> Option<PathBuf> {
    folder.ancestors().map(|d| d.join(".team").join("config.toml")).find(|p| p.is_file())
}

/// The `[team]` table of the project config that applies to `folder` (empty when none).
fn project_team(folder: &Path) -> ProjectTeam {
    project_config_path(folder)
        .and_then(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            toml::from_str::<ProjectConfig>(&text)
                .map_err(|e| eprintln!("jg-team-dev: ignoring invalid {}: {e}", path.display()))
                .ok()
        })
        .unwrap_or_default()
        .team
}

/// A non-empty string, or `None`.
fn set(value: &str) -> Option<String> {
    Some(value.trim().to_string()).filter(|v| !v.is_empty())
}

impl Team {
    /// Settings for `skill` in `folder`. When `<folder>/.team/agents.json` exists (saved when the
    /// team started), its harness, model and effort replace the resolved ones. Otherwise harness,
    /// model and effort come whole from the first of: the project's `[team.skills.<skill>]`, the
    /// user's `[team.skills.<skill>]`, the role's built-in default (`role_defaults`), the
    /// project's `[team]`, the user's `[team]`, then `claude-code` with `DEFAULT_MODEL` and
    /// `DEFAULT_EFFORT` (`run_unit`). The other settings: the most specific wins, field by field.
    /// Invalid values are replaced (see `check`).
    pub fn for_skill(&self, skill: &str, folder: &Path) -> SessionSettings {
        self.resolve_for_folder(skill, folder).0
    }

    /// Like `for_skill`, with a warning for each setting that was invalid and replaced.
    pub fn for_skill_checked(&self, skill: &str, folder: &Path) -> (SessionSettings, Vec<String>) {
        self.resolve_for_folder(skill, folder)
    }

    /// Settings for `skill` resolved from the user config alone without any project folder
    /// (no workspace `agents.json` and no project `config.toml`).
    /// The Coordinator is kept on Claude Code.
    pub fn for_user(&self, skill: &str) -> SessionSettings {
        self.resolve_without_project(skill).0
    }

    /// Like `for_user`, with any warnings for invalid settings.
    pub fn resolve_without_project(&self, skill: &str) -> (SessionSettings, Vec<String>) {
        self.resolve(skill, &ProjectTeam::default())
    }

    pub(crate) fn resolve_unlocked(&self, skill: &str, folder: &Path) -> (SessionSettings, Vec<String>) {
        self.resolve(skill, &project_team(folder))
    }

    fn resolve_for_folder(&self, skill: &str, folder: &Path) -> (SessionSettings, Vec<String>) {
        let (mut settings, mut warnings) = self.resolve_unlocked(skill, folder);
        if let Some(saved) = TeamAgents::load(folder).and_then(|mut a| a.roles.remove(skill)) {
            let harness = if skill == crate::model::COORDINATOR && saved.harness != Harness::ClaudeCode {
                warnings.push(format!(
                    "coordinator: the Coordinator runs on Claude Code only, not {}; using claude-code",
                    saved.harness.label()
                ));
                Harness::ClaudeCode
            } else {
                saved.harness
            };
            let effort = check_effort(skill, harness, Some(&saved.effort), &mut warnings);
            settings.harness = harness;
            settings.model = saved.model;
            settings.effort = effort;
        }
        (settings, warnings)
    }

    /// The warnings of every role in `folder` (`None`: the user config alone), for the app's
    /// Settings and for `jg-team-dev worker`.
    pub fn warnings(&self, folder: Option<&Path>) -> Vec<String> {
        let project = folder.map(project_team).unwrap_or_default();
        let mut out = Vec::new();
        for role in roles() {
            for w in self.resolve(role, &project).1 {
                if !out.contains(&w) {
                    out.push(w);
                }
            }
        }
        out
    }

    /// The WIP limit for the workspace `folder`: the project's `[team] wip_limit`, else the
    /// user's, clamped to `WIP_LIMIT_RANGE`.
    pub fn wip_limit(&self, folder: &Path) -> u8 {
        self.resolve_wip_limit(&project_team(folder))
    }

    fn resolve_wip_limit(&self, project: &ProjectTeam) -> u8 {
        project.wip_limit.unwrap_or(self.wip_limit).clamp(WIP_LIMIT_RANGE.0, WIP_LIMIT_RANGE.1)
    }

    /// The user's `[team]` table as settings, for the precedence chain.
    fn user_defaults(&self) -> SkillSettings {
        SkillSettings {
            harness: set(&self.harness),
            model: set(&self.model),
            effort: set(&self.effort),
            permission_mode: set(&self.permission_mode),
            ..SkillSettings::default()
        }
    }

    fn resolve(&self, skill: &str, project: &ProjectTeam) -> (SessionSettings, Vec<String>) {
        let none = SkillSettings::default();
        let builtin = role_defaults(skill);
        let user_skill = self.skills.get(skill).unwrap_or(&none);
        let project_skill = project.skills.get(skill).unwrap_or(&none);
        let user_defaults = self.user_defaults();
        // Harness, model and effort come as one unit from one layer (`run_unit`); the other
        // settings merge field by field, the most specific first.
        let unit = run_unit(&[project_skill, user_skill, &builtin, &project.defaults, &user_defaults]);
        let merged = project_skill
            .clone()
            .or(&project.defaults)
            .or(user_skill)
            .or(&user_defaults)
            .or(&builtin);
        let mut warnings = Vec::new();
        let harness = check_harness(skill, unit.harness.as_deref(), &mut warnings);
        let model = unit.model.unwrap_or_else(|| DEFAULT_MODEL.into());
        let effort = check_effort(skill, harness, unit.effort.as_deref(), &mut warnings);
        let fallback = merged.fallback.map(|f| {
            let harness = check_harness(skill, f.harness.as_deref().or(Some("claude-code")), &mut warnings);
            Fallback {
                harness,
                model: f.model.unwrap_or_else(|| DEFAULT_MODEL.into()),
                effort: check_effort(skill, harness, f.effort.as_deref(), &mut warnings),
            }
        });
        let settings = SessionSettings {
            harness,
            model,
            effort,
            permission_mode: merged.permission_mode.unwrap_or_else(|| "auto".into()),
            allowed_tools: merged.allowed_tools.unwrap_or_default(),
            disallowed_tools: merged.disallowed_tools.unwrap_or_default(),
            compact_window: match merged.compact_window {
                Some(0) => None,
                Some(w) => Some(w.clamp(COMPACT_WINDOW_RANGE.0, COMPACT_WINDOW_RANGE.1)),
                None => (skill == crate::model::COORDINATOR).then_some(COORDINATOR_COMPACT_WINDOW),
            },
            fallback,
        };
        (settings, warnings)
    }
}

/// The harness, model and effort of a role, from `layers`, most specific first. The first layer
/// that sets any of them decides the harness: its own, else `claude-code`, never a lower layer's.
/// A model or effort it leaves unset comes from the next layer on that same harness, else the
/// built-in `[team]` (`DEFAULT_MODEL`, `DEFAULT_EFFORT`).
fn run_unit(layers: &[&SkillSettings]) -> SkillSettings {
    let harness_of = |l: &SkillSettings| l.harness.clone().unwrap_or_else(|| "claude-code".into());
    let sets_any = |l: &&&SkillSettings| l.harness.is_some() || l.model.is_some() || l.effort.is_some();
    let Some(first) = layers.iter().position(|l| sets_any(&l)) else { return SkillSettings::default() };
    let harness = harness_of(layers[first]);
    let same = || layers[first..].iter().filter(|l| sets_any(l) && harness_of(l).trim() == harness.trim());
    SkillSettings {
        model: same().find_map(|l| l.model.clone()),
        effort: same().find_map(|l| l.effort.clone()),
        harness: Some(harness),
        ..SkillSettings::default()
    }
}

/// Every role the config may set: the Coordinator and the worker skills.
pub fn roles() -> impl Iterator<Item = &'static str> {
    std::iter::once(crate::model::COORDINATOR).chain(crate::team::WORKER_SKILLS.iter().copied())
}

/// A known worker harness, else `claude-code` with a warning. The Coordinator is Claude Code
/// only (PRD D2/H2).
fn check_harness(role: &str, value: Option<&str>, warnings: &mut Vec<String>) -> Harness {
    let Some(name) = value.map(str::trim).filter(|v| !v.is_empty()) else { return Harness::ClaudeCode };
    match name.parse::<Harness>() {
        Ok(Harness::Custom) | Err(_) => {
            warnings.push(format!(
                "{role}: unknown harness `{name}` (use claude-code, opencode or antigravity); using claude-code"
            ));
            Harness::ClaudeCode
        }
        Ok(h) if role == crate::model::COORDINATOR && h != Harness::ClaudeCode => {
            warnings.push(format!("coordinator: the Coordinator runs on Claude Code only, not {}; using claude-code", h.label()));
            Harness::ClaudeCode
        }
        Ok(h) => h,
    }
}

/// The effort as the harness takes it. An unknown level falls back to `medium` with a warning;
/// a known level the harness lacks maps to the nearest lower one it has (PRD H3).
fn check_effort(role: &str, harness: Harness, value: Option<&str>, warnings: &mut Vec<String>) -> String {
    let level = value.map(str::trim).filter(|v| !v.is_empty()).unwrap_or(DEFAULT_EFFORT);
    if !EFFORT_LEVELS.contains(&level) {
        warnings.push(format!(
            "{role}: unknown effort `{level}` (use {}); using {DEFAULT_EFFORT}",
            EFFORT_LEVELS.join(", ")
        ));
        return DEFAULT_EFFORT.into();
    }
    match crate::harness::nearest_effort(harness, level) {
        Some(l) if l != level => {
            warnings.push(format!("{role}: {} has no effort `{level}`; using {l}", harness.label()));
            l.into()
        }
        _ => level.into(),
    }
}

fn default_legacy_style() -> String {
    "default".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Desktop {
    /// `system`, `light` or `dark`.
    pub theme: String,
    /// `jg-violet`, `default`, `tokyo-night` or `dracula`.
    #[serde(default = "default_legacy_style")]
    pub style: String,
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
        Self {
            theme: "system".into(),
            style: "jg-violet".into(),
            font: None,
            zoom: 1.0,
            text_size: Self::DEFAULT_TEXT_SIZE,
            tray: true,
        }
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
    /// Absolute path to Antigravity's `agy`, for workers on the `antigravity` harness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub antigravity: Option<String>,
    /// Terminal used to reopen sessions. `{cwd}` is replaced by the session folder and the
    /// agent command is appended, e.g. `["kitty", "--directory", "{cwd}", "--"]`.
    pub terminal: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Git {
    pub auto_commit: bool,
    pub auto_merge: bool,
    pub auto_push: bool,
    pub allow_main: bool,
}

impl Git {
    pub fn policy_text(&self) -> String {
        let commit = if self.auto_commit {
            "auto-commit: ON (commit without asking)"
        } else {
            "auto-commit: OFF (ask first)"
        };
        let merge = if self.auto_merge {
            "auto-merge: ON (merge without asking)"
        } else {
            "auto-merge: OFF (ask first)"
        };
        let push = if self.auto_push {
            "auto-push: ON (push without asking)"
        } else {
            "auto-push: OFF (ask first)"
        };
        let main = if self.allow_main {
            "main branch: ON (the ON actions also apply to main)"
        } else {
            "main branch: OFF (ask before any commit, merge or push that touches main, even if the action is ON)"
        };
        format!("{commit}\n{merge}\n{push}\n{main}\nAlways: reviewer approval and Done before merge; never force-push.")
    }
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
    use crate::model::COORDINATOR;

    #[test]
    fn the_most_specific_setting_wins() {
        let user: Config = toml::from_str(
            r#"
            [team]
            model = "opus"
            [team.skills.coder]
            model = "sonnet"
            effort = "high"
            [team.skills.git-coder]
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
        let coder = user.team.resolve("coder", &project.team).0;
        // The project's coder model, the user's coder effort (both on claude-code); `[team]` is
        // below the role.
        assert_eq!((coder.harness, coder.model.as_str(), coder.effort.as_str()), (Harness::ClaudeCode, "haiku", "high"));
        // `allowed_tools` alone does not replace the role's built-in harness, model and effort.
        let git_coder = user.team.resolve("git-coder", &ProjectTeam::default()).0;
        assert_eq!((git_coder.model.as_str(), git_coder.effort.as_str()), ("sonnet", "low"));
        assert_eq!(git_coder.allowed_tools, ["Bash(cargo test:*)"]);
        assert_eq!(git_coder.permission_mode, "auto");
        assert_eq!(git_coder.compact_window, None);
    }

    #[test]
    fn the_coordinator_compacts_early_unless_told_otherwise() {
        let none = ProjectTeam::default();
        let team = Config::default().team;
        assert_eq!(team.resolve("coordinator", &none).0.compact_window, Some(COORDINATOR_COMPACT_WINDOW));
        let user: Config = toml::from_str(
            "[team.skills.coordinator]\ncompact_window = 0\n[team.skills.coder]\ncompact_window = 5000\n",
        )
        .unwrap();
        assert_eq!(user.team.resolve("coordinator", &none).0.compact_window, None);
        assert_eq!(user.team.resolve("coder", &none).0.compact_window, Some(COMPACT_WINDOW_RANGE.0));
    }

    #[test]
    fn roles_get_their_built_in_defaults() {
        let team = Config::default().team;
        let none = ProjectTeam::default();
        let get = |role: &str| team.resolve(role, &none).0;
        let coder = get("coder");
        assert_eq!(coder.describe("coder"), "coder · Antigravity · gemini-3.8-flash-high · high");
        let fallback = coder.on_fallback().unwrap();
        assert_eq!(fallback.describe("coder"), "coder · Claude Code · sonnet · medium");
        assert_eq!(get(COORDINATOR).describe("c"), "c · Claude Code · opus · medium");
        assert_eq!(get("planner").describe("p"), "p · Claude Code · opus · medium");
        assert_eq!(get("reviewer").describe("r"), "r · Claude Code · opus · medium");
        assert_eq!(get("git-coder").describe("g"), "g · Claude Code · sonnet · low");
        assert!(get("git-coder").on_fallback().is_none());
        assert!(team.warnings(None).is_empty(), "{:?}", team.warnings(None));
    }

    #[test]
    fn harness_model_and_effort_resolve_as_one_unit() {
        let none = ProjectTeam::default();
        // `[team]` is below the role default: the coder stays on Antigravity with the agy model.
        let user: Config = toml::from_str("[team]\nmodel = \"opus\"\neffort = \"low\"\n").unwrap();
        let coder = user.team.resolve("coder", &none).0;
        assert_eq!(coder.describe("coder"), "coder · Antigravity · gemini-3.8-flash-high · high");
        // Without agy the role's fallback is taken whole, not mixed with `[team]`.
        assert_eq!(coder.on_fallback().unwrap().describe("coder"), "coder · Claude Code · sonnet · medium");
        assert_eq!(user.team.resolve("git-coder", &none).0.describe("g"), "g · Claude Code · sonnet · low");
        // A role model without a harness runs on claude-code, never on the default's Antigravity.
        let user: Config = toml::from_str("[team.skills.coder]\nmodel = \"x\"\n").unwrap();
        let coder = user.team.resolve("coder", &none).0;
        assert_eq!((coder.harness, coder.model.as_str(), coder.effort.as_str()), (Harness::ClaudeCode, "x", "medium"));
        // A project role effort without a harness does not inherit the user's coder model on agy.
        let user: Config = toml::from_str("[team.skills.coder]\nharness = \"antigravity\"\nmodel = \"m\"\n").unwrap();
        let project: ProjectConfig = toml::from_str("[team.skills.coder]\neffort = \"low\"\n").unwrap();
        let coder = user.team.resolve("coder", &project.team).0;
        assert_eq!(coder.describe("c"), "c · Claude Code · opus · low");
        // A role without a built-in default takes `[team]`.
        let user: Config = toml::from_str("[team]\nharness = \"opencode\"\nmodel = \"p/m\"\n").unwrap();
        assert_eq!(user.team.resolve("other", &none).0.describe("o"), "o · opencode · p/m · medium");
    }

    #[test]
    fn invalid_settings_fall_back_with_a_warning() {
        let user: Config = toml::from_str(
            r#"
            [team.skills.planner]
            effort = "hihg"
            [team.skills.coordinator]
            harness = "opencode"
            [team.skills.git-coder]
            harness = "codex"
            [team.skills.reviewer]
            harness = "opencode"
            effort = "max"
            "#,
        )
        .unwrap();
        let none = ProjectTeam::default();
        let (planner, w) = user.team.resolve("planner", &none);
        assert_eq!(planner.effort, "medium");
        assert_eq!(w, ["planner: unknown effort `hihg` (use low, medium, high, xhigh, max); using medium"]);
        let (coordinator, w) = user.team.resolve(COORDINATOR, &none);
        assert_eq!(coordinator.harness, Harness::ClaudeCode);
        assert!(w.iter().any(|w| w.contains("Claude Code only")), "{w:?}");
        let (git_coder, w) = user.team.resolve("git-coder", &none);
        assert_eq!(git_coder.harness, Harness::ClaudeCode);
        assert!(w.iter().any(|w| w.contains("unknown harness `codex`")), "{w:?}");
        // OpenCode has no effort we can map: the level stays, the flag is dropped at launch.
        let (reviewer, w) = user.team.resolve("reviewer", &none);
        assert_eq!((reviewer.harness, reviewer.effort.as_str()), (Harness::Opencode, "max"));
        assert!(w.iter().all(|w| !w.starts_with("reviewer")), "{w:?}");
        let all = user.team.warnings(None);
        assert!(all.iter().any(|w| w.starts_with("coordinator:")) && all.iter().any(|w| w.starts_with("git-coder:")));
    }

    #[test]
    fn legacy_verifier_and_tester_skills_in_config_do_not_break_loading() {
        let toml_str = r#"
        [team]
        model = "opus"
        [team.skills.verifier]
        model = "haiku"
        [team.skills.tester]
        model = "haiku"
        "#;
        let user: Config = toml::from_str(toml_str).unwrap();
        assert!(user.team.skills.contains_key("verifier"));
        assert!(user.team.skills.contains_key("tester"));
        let warnings = user.team.warnings(None);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn wip_limit_defaults_to_one_and_the_project_overrides_it() {
        let user = Config::default();
        assert_eq!(user.team.resolve_wip_limit(&ProjectTeam::default()), 1);
        let user: Config = toml::from_str("[team]\nwip_limit = 2\n").unwrap();
        assert_eq!(user.team.resolve_wip_limit(&ProjectTeam::default()), 2);
        let project: ProjectConfig = toml::from_str("[team]\nwip_limit = 3\neffort = \"low\"\n").unwrap();
        assert_eq!(user.team.resolve_wip_limit(&project.team), 3);
        assert_eq!(project.team.defaults.effort.as_deref(), Some("low"));
        let too_many: ProjectConfig = toml::from_str("[team]\nwip_limit = 9\n").unwrap();
        assert_eq!(user.team.resolve_wip_limit(&too_many.team), 3);
        let zero: Config = toml::from_str("[team]\nwip_limit = 0\n").unwrap();
        assert_eq!(zero.team.resolve_wip_limit(&ProjectTeam::default()), 1);
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

    #[test]
    fn desktop_style_round_trips_and_defaults() {
        let toml_str = "[desktop]\ntheme = \"dark\"\nstyle = \"dracula\"\n";
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.desktop.style, "dracula");
        let serialized = toml::to_string(&cfg).unwrap();
        let round_tripped: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(round_tripped.desktop.style, "dracula");

        let toml_str = "[desktop]\nstyle = \"tokyo-night\"\n";
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.desktop.style, "tokyo-night");

        let toml_str = "[desktop]\nstyle = \"jg-violet\"\n";
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.desktop.style, "jg-violet");
        let serialized = toml::to_string(&cfg).unwrap();
        let round_tripped: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(round_tripped.desktop.style, "jg-violet");

        let old_toml = "[desktop]\ntheme = \"light\"\nzoom = 1.2\n";
        let old_cfg: Config = toml::from_str(old_toml).unwrap();
        assert_eq!(old_cfg.desktop.style, "default");

        assert_eq!(Config::default().desktop.style, "jg-violet");
    }

    #[test]
    fn with_no_agents_file_result_is_unchanged() {
        let root = std::env::temp_dir().join(format!("jgt-no-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        assert_eq!(agents_path(&root), None);
        assert_eq!(TeamAgents::load(&root), None);

        let cfg = Config::default();
        let coder = cfg.team.for_skill("coder", &root);
        assert_eq!(coder.describe("coder"), "coder · Antigravity · gemini-3.8-flash-high · high");

        let coord = cfg.team.for_skill(COORDINATOR, &root);
        assert_eq!(coord.describe(COORDINATOR), "coordinator · Claude Code · opus · medium");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saved_agents_file_wins_over_changed_user_config() {
        let root = std::env::temp_dir().join(format!("jgt-agents-win-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let mut agents = TeamAgents::default();
        agents.roles.insert(
            "coder".into(),
            AgentSetting {
                harness: Harness::Opencode,
                model: "qwen-coder".into(),
                effort: "medium".into(),
            },
        );
        agents.write(&root).unwrap();

        let user: Config = toml::from_str(
            r#"
            [team.skills.coder]
            harness = "claude-code"
            model = "sonnet"
            effort = "low"
            allowed_tools = ["Bash(cargo test:*)"]
            "#,
        )
        .unwrap();

        let coder = user.team.for_skill("coder", &root);
        // The saved file wins for harness, model and effort:
        assert_eq!(coder.harness, Harness::Opencode);
        assert_eq!(coder.model, "qwen-coder");
        assert_eq!(coder.effort, "medium");
        // Non-agent settings still come from the config:
        assert_eq!(coder.allowed_tools, ["Bash(cargo test:*)"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn agents_file_is_found_from_a_worktree() {
        let root = std::env::temp_dir().join(format!("jgt-agents-wt-{}", std::process::id()));
        let wt = root.join(".team/worktrees/fix-jgt");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&wt).unwrap();

        assert_eq!(agents_path(&wt), None);

        let mut agents = TeamAgents::default();
        agents.roles.insert(
            "coder".into(),
            AgentSetting {
                harness: Harness::ClaudeCode,
                model: "haiku".into(),
                effort: "low".into(),
            },
        );
        agents.write(&root).unwrap();

        assert_eq!(agents_path(&wt), Some(root.join(".team/agents.json")));
        assert_eq!(TeamAgents::load(&wt).unwrap().roles["coder"].model, "haiku");

        let coder = Config::default().team.for_skill("coder", &wt);
        assert_eq!(coder.harness, Harness::ClaudeCode);
        assert_eq!(coder.model, "haiku");
        assert_eq!(coder.effort, "low");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn coordinator_is_claude_code_only_even_if_saved_otherwise() {
        let root = std::env::temp_dir().join(format!("jgt-agents-coord-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let mut agents = TeamAgents::default();
        agents.roles.insert(
            COORDINATOR.into(),
            AgentSetting {
                harness: Harness::Opencode,
                model: "other-model".into(),
                effort: "medium".into(),
            },
        );
        agents.write(&root).unwrap();

        let (coord, warnings) = Config::default().team.for_skill_checked(COORDINATOR, &root);
        assert_eq!(coord.harness, Harness::ClaudeCode);
        assert_eq!(coord.model, "other-model");
        assert!(warnings.iter().any(|w| w.contains("Claude Code only")), "{warnings:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn team_agents_resolve_and_roundtrip() {
        let root = std::env::temp_dir().join(format!("jgt-agents-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let team = Config::default().team;
        let resolved = TeamAgents::resolve(&team, &root);
        assert_eq!(resolved.roles.len(), 5);
        assert_eq!(resolved.roles[COORDINATOR].harness, Harness::ClaudeCode);
        assert_eq!(resolved.roles["coder"].harness, Harness::Antigravity);

        let desc = resolved.describe();
        assert!(desc.starts_with("Agents: coordinator · Claude Code · opus · medium, planner · "));
        assert!(desc.contains("coder · Antigravity · gemini-3.8-flash-high · high"));

        resolved.write(&root).unwrap();
        let loaded = TeamAgents::load(&root).unwrap();
        assert_eq!(loaded, resolved);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn for_user_ignores_project_and_agents_and_forces_coordinator_to_claude_code() {
        let root = std::env::temp_dir().join(format!("jgt-user-alone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".team")).unwrap();

        let mut agents = TeamAgents::default();
        agents.roles.insert(
            "coder".into(),
            AgentSetting {
                harness: Harness::Antigravity,
                model: "gemini-saved".into(),
                effort: "high".into(),
            },
        );
        agents.write(&root).unwrap();

        std::fs::write(root.join(".team/config.toml"), "[team.skills.coder]\nmodel = \"project-model\"\n").unwrap();

        let user: Config = toml::from_str(
            r#"
            [team.skills.coder]
            harness = "claude-code"
            model = "user-model"
            effort = "low"

            [team.skills.coordinator]
            harness = "opencode"
            model = "user-coord"
            effort = "medium"
            "#,
        )
        .unwrap();

        // for_skill reads the workspace's agents.json
        assert_eq!(user.team.for_skill("coder", &root).model, "gemini-saved");

        // for_user reads the USER config alone, ignoring agents.json and project config.toml
        let coder = user.team.for_user("coder");
        assert_eq!(coder.harness, Harness::ClaudeCode);
        assert_eq!(coder.model, "user-model");
        assert_eq!(coder.effort, "low");

        // Coordinator is forced to Claude Code
        let (coord, warnings) = user.team.resolve_without_project(COORDINATOR);
        assert_eq!(coord.harness, Harness::ClaudeCode);
        assert_eq!(coord.model, "user-coord");
        assert!(warnings.iter().any(|w| w.contains("Claude Code only")), "{warnings:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_defaults_are_all_off() {
        let git = Git::default();
        assert!(!git.auto_commit);
        assert!(!git.auto_merge);
        assert!(!git.auto_push);
        assert!(!git.allow_main);

        let cfg = Config::default();
        assert_eq!(cfg.git, Git::default());
    }

    #[test]
    fn git_toml_round_trip() {
        let toml_str = r#"
        [git]
        auto_commit = true
        auto_merge = false
        auto_push = true
        allow_main = true
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            cfg.git,
            Git {
                auto_commit: true,
                auto_merge: false,
                auto_push: true,
                allow_main: true,
            }
        );
        let serialized = toml::to_string(&cfg).unwrap();
        let round_tripped: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(round_tripped.git, cfg.git);
    }

    #[test]
    fn config_without_git_loads_defaults() {
        let toml_str = r#"
        [desktop]
        theme = "dark"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.git, Git::default());

        let empty_cfg: Config = toml::from_str("").unwrap();
        assert_eq!(empty_cfg.git, Git::default());
    }

    #[test]
    fn git_policy_text_reflects_each_flag() {
        let off = Git::default();
        assert_eq!(
            off.policy_text(),
            "auto-commit: OFF (ask first)\n\
             auto-merge: OFF (ask first)\n\
             auto-push: OFF (ask first)\n\
             main branch: OFF (ask before any commit, merge or push that touches main, even if the action is ON)\n\
             Always: reviewer approval and Done before merge; never force-push."
        );

        let on = Git {
            auto_commit: true,
            auto_merge: true,
            auto_push: true,
            allow_main: true,
        };
        assert_eq!(
            on.policy_text(),
            "auto-commit: ON (commit without asking)\n\
             auto-merge: ON (merge without asking)\n\
             auto-push: ON (push without asking)\n\
             main branch: ON (the ON actions also apply to main)\n\
             Always: reviewer approval and Done before merge; never force-push."
        );

        let commit_only = Git { auto_commit: true, ..Git::default() };
        assert!(commit_only.policy_text().contains("auto-commit: ON (commit without asking)"));
        assert!(commit_only.policy_text().contains("auto-merge: OFF (ask first)"));

        let merge_only = Git { auto_merge: true, ..Git::default() };
        assert!(merge_only.policy_text().contains("auto-commit: OFF (ask first)"));
        assert!(merge_only.policy_text().contains("auto-merge: ON (merge without asking)"));

        let push_only = Git { auto_push: true, ..Git::default() };
        assert!(push_only.policy_text().contains("auto-commit: OFF (ask first)"));
        assert!(push_only.policy_text().contains("auto-push: ON (push without asking)"));

        let main_only = Git { allow_main: true, ..Git::default() };
        assert!(main_only.policy_text().contains("main branch: ON (the ON actions also apply to main)"));
        assert!(main_only.policy_text().contains("auto-commit: OFF (ask first)"));
    }
}
