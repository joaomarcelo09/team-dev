//! `~/.config/jg-team-dev/config.toml`, shared by the hub, the CLI and the app.

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
        "verifier" | "reviewer" => set(Some("claude-code"), "opus", "medium"),
        "tester" | "git-coder" => set(Some("claude-code"), "sonnet", "low"),
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
    /// Settings for `skill` in `folder`. Harness, model and effort come whole from the first of:
    /// the project's `[team.skills.<skill>]`, the user's `[team.skills.<skill>]`, the role's
    /// built-in default (`role_defaults`), the project's `[team]`, the user's `[team]`, then
    /// `claude-code` with `DEFAULT_MODEL` and `DEFAULT_EFFORT` (`run_unit`). The other settings:
    /// the most specific wins, field by field. Invalid values are replaced (see `check`).
    pub fn for_skill(&self, skill: &str, folder: &Path) -> SessionSettings {
        self.resolve(skill, &project_team(folder)).0
    }

    /// Like `for_skill`, with a warning for each setting that was invalid and replaced.
    pub fn for_skill_checked(&self, skill: &str, folder: &Path) -> (SessionSettings, Vec<String>) {
        self.resolve(skill, &project_team(folder))
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
    /// Absolute path to Antigravity's `agy`, for workers on the `antigravity` harness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub antigravity: Option<String>,
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
        let coder = user.team.resolve("coder", &project.team).0;
        // The project's coder model, the user's coder effort (both on claude-code); `[team]` is
        // below the role.
        assert_eq!((coder.harness, coder.model.as_str(), coder.effort.as_str()), (Harness::ClaudeCode, "haiku", "high"));
        // `allowed_tools` alone does not replace the role's built-in harness, model and effort.
        let tester = user.team.resolve("tester", &ProjectTeam::default()).0;
        assert_eq!((tester.model.as_str(), tester.effort.as_str()), ("sonnet", "low"));
        assert_eq!(tester.allowed_tools, ["Bash(cargo test:*)"]);
        assert_eq!(tester.permission_mode, "auto");
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
        assert_eq!(get("verifier").describe("v"), "v · Claude Code · opus · medium");
        assert_eq!(get("reviewer").describe("r"), "r · Claude Code · opus · medium");
        assert_eq!(get("tester").describe("t"), "t · Claude Code · sonnet · low");
        assert_eq!(get("git-coder").describe("g"), "g · Claude Code · sonnet · low");
        assert!(get("tester").on_fallback().is_none());
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
        assert_eq!(user.team.resolve("tester", &none).0.describe("t"), "t · Claude Code · sonnet · low");
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
            [team.skills.tester]
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
        let (tester, w) = user.team.resolve("tester", &none);
        assert_eq!(tester.harness, Harness::ClaudeCode);
        assert!(w.iter().any(|w| w.contains("unknown harness `codex`")), "{w:?}");
        // OpenCode has no effort we can map: the level stays, the flag is dropped at launch.
        let (reviewer, w) = user.team.resolve("reviewer", &none);
        assert_eq!((reviewer.harness, reviewer.effort.as_str()), (Harness::Opencode, "max"));
        assert!(w.iter().all(|w| !w.starts_with("reviewer")), "{w:?}");
        let all = user.team.warnings(None);
        assert!(all.iter().any(|w| w.starts_with("coordinator:")) && all.iter().any(|w| w.starts_with("tester:")));
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
}
