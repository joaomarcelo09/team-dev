//! Per-role harness, model and reasoning-effort configuration.
//!
//! The settings live in one TOML file. A missing file means the built-in defaults, and a file
//! that names only some roles overrides only those roles.

use std::{
    collections::BTreeMap,
    fmt, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Default settings per role.
///
/// Every role defaults to Claude Code, the only harness the factory launches.
/// The model ids are stable aliases. Each is resolved to the provider's current model when the
/// configuration is written, so this table is the only place that names a model.
const DEFAULTS: [(FactoryRole, Harness, &str, Effort); 7] = [
    (
        FactoryRole::LabCoordinator,
        Harness::ClaudeCode,
        "sonnet",
        Effort::Medium,
    ),
    (
        FactoryRole::CoCoordinator,
        Harness::ClaudeCode,
        "sonnet",
        Effort::High,
    ),
    (
        FactoryRole::Coder,
        Harness::ClaudeCode,
        "sonnet",
        Effort::High,
    ),
    (
        FactoryRole::Tester,
        Harness::ClaudeCode,
        "sonnet",
        Effort::High,
    ),
    (
        FactoryRole::Reviewer,
        Harness::ClaudeCode,
        "opus",
        Effort::Medium,
    ),
    (
        FactoryRole::GitCoder,
        Harness::ClaudeCode,
        "haiku",
        Effort::Low,
    ),
    (
        FactoryRole::WakeUp,
        Harness::ClaudeCode,
        "haiku",
        Effort::Low,
    ),
];

/// Longest accepted model name, in bytes.
const MAX_MODEL_LEN: usize = 120;

/// A role of the Dark Factory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FactoryRole {
    /// The lab-wide coordinator that talks to the user.
    LabCoordinator,
    /// A coordinator that runs one project.
    CoCoordinator,
    /// Writes code.
    Coder,
    /// Runs checks and tests.
    Tester,
    /// Reviews the work.
    Reviewer,
    /// Performs git operations.
    GitCoder,
    /// The worker that resumes work after a pause.
    WakeUp,
}

impl FactoryRole {
    /// Every role, in the order used by the configuration file.
    pub const ALL: [Self; 7] = [
        Self::LabCoordinator,
        Self::CoCoordinator,
        Self::Coder,
        Self::Tester,
        Self::Reviewer,
        Self::GitCoder,
        Self::WakeUp,
    ];

    /// The role's stable name, as used in the configuration file.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LabCoordinator => "lab-coordinator",
            Self::CoCoordinator => "co-coordinator",
            Self::Coder => "coder",
            Self::Tester => "tester",
            Self::Reviewer => "reviewer",
            Self::GitCoder => "git-coder",
            Self::WakeUp => "wake-up",
        }
    }

    /// Parses a role name, rejecting anything outside [`Self::ALL`].
    pub fn parse(raw: &str) -> Result<Self, RoleError> {
        Self::ALL
            .into_iter()
            .find(|role| role.as_str() == raw)
            .ok_or_else(|| RoleError::UnknownRole(raw.to_string()))
    }
}

impl Serialize for FactoryRole {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FactoryRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A coding harness that can run a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    /// Claude Code.
    ClaudeCode,
    /// OpenCode.
    Opencode,
    /// Gemini.
    Gemini,
}

impl Harness {
    /// The harness's stable name, as used in the configuration file.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Opencode => "opencode",
            Self::Gemini => "gemini",
        }
    }

    /// Parses a harness name, rejecting anything outside this enum.
    pub fn parse(raw: &str) -> Result<Self, RoleError> {
        match raw {
            "claude-code" => Ok(Self::ClaudeCode),
            "opencode" => Ok(Self::Opencode),
            "gemini" => Ok(Self::Gemini),
            _ => Err(RoleError::UnknownHarness(raw.to_string())),
        }
    }
}

/// Reasoning effort of a role, from lowest to highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// Extra high.
    XHigh,
    /// Maximum.
    Max,
}

impl Effort {
    /// The effort's stable name, as used in the configuration file.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parses an effort name, rejecting anything outside this enum.
    pub fn parse(raw: &str) -> Result<Self, RoleError> {
        match raw {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            _ => Err(RoleError::UnknownEffort(raw.to_string())),
        }
    }
}

/// The harness, model and effort of one role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleSettings {
    /// Harness that runs the role.
    pub harness: Harness,
    /// Model the role runs on the harness.
    pub model: String,
    /// Reasoning effort of the role.
    pub effort: Effort,
}

impl<'de> Deserialize<'de> for RoleSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            harness: Harness,
            model: String,
            effort: Effort,
        }

        let raw = Raw::deserialize(deserializer)?;
        validate_model(&raw.model).map_err(serde::de::Error::custom)?;
        Ok(Self {
            harness: raw.harness,
            model: raw.model,
            effort: raw.effort,
        })
    }
}

/// Settings of every role, by role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RoleConfig {
    roles: BTreeMap<FactoryRole, RoleSettings>,
}

impl Default for RoleConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

impl RoleConfig {
    /// The built-in settings from the Dark Factory product rules.
    pub fn defaults() -> Self {
        let roles = DEFAULTS
            .into_iter()
            .map(|(role, harness, model, effort)| {
                let settings = RoleSettings {
                    harness,
                    model: model.to_string(),
                    effort,
                };
                (role, settings)
            })
            .collect();
        Self { roles }
    }

    /// The settings of `role`, when present.
    pub fn get(&self, role: FactoryRole) -> Option<&RoleSettings> {
        self.roles.get(&role)
    }

    /// Replaces the settings of `role`.
    pub fn set(&mut self, role: FactoryRole, settings: RoleSettings) {
        self.roles.insert(role, settings);
    }

    /// Every role's settings.
    pub fn roles(&self) -> &BTreeMap<FactoryRole, RoleSettings> {
        &self.roles
    }

    /// Checks that every role is present and that every model name is valid.
    ///
    /// Harness capability rules are deliberately not enforced: effort levels are passed through
    /// regardless of the harness, and the launcher decides what it can run.
    pub fn validate(&self) -> Result<(), RoleError> {
        for role in FactoryRole::ALL {
            let settings = self.roles.get(&role).ok_or(RoleError::MissingRole(role))?;
            validate_model(&settings.model).map_err(|reason| RoleError::InvalidModel {
                role,
                model: settings.model.clone(),
                reason,
            })?;
        }
        Ok(())
    }

    /// Resolves `role`, applying the first override for that role field by field.
    ///
    /// Overrides for other roles are ignored. The override values are applied as they are; use
    /// [`Self::try_resolve`] when the override comes from untrusted input, so a hostile model is
    /// a typed error instead of a silently kept base value.
    pub fn resolve(&self, role: FactoryRole, overrides: &[RunOverride]) -> RoleSettings {
        let mut settings = self
            .roles
            .get(&role)
            .cloned()
            .expect("role configuration has settings for every role");

        if let Some(over) = overrides.iter().find(|over| over.role == role) {
            if let Some(harness) = over.harness {
                settings.harness = harness;
            }
            if let Some(model) = &over.model {
                settings.model = model.clone();
            }
            if let Some(effort) = over.effort {
                settings.effort = effort;
            }
        }

        settings
    }

    /// Resolves `role` like [`Self::resolve`], after validating the matching override's model.
    pub fn try_resolve(
        &self,
        role: FactoryRole,
        overrides: &[RunOverride],
    ) -> Result<RoleSettings, RoleError> {
        if let Some(over) = overrides.iter().find(|over| over.role == role) {
            over.validate()?;
        }
        Ok(self.resolve(role, overrides))
    }

    /// Loads the configuration at `path`.
    ///
    /// A missing file is not an error: it means [`Self::defaults`]. A file that names only some
    /// roles keeps the defaults for the others; a field omitted inside a named role keeps that
    /// role's current value.
    pub fn load(path: &Path) -> Result<Self, RoleError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::defaults());
            }
            Err(source) => return Err(RoleError::io(path, source)),
        };
        Self::from_toml(path, &text)
    }

    /// Saves the configuration at `path`, replacing any existing file atomically.
    pub fn save(&self, path: &Path) -> Result<(), RoleError> {
        self.validate()?;
        let text = toml::to_string(self)?;
        write_atomic(path, &text)
    }

    fn from_toml(path: &Path, text: &str) -> Result<Self, RoleError> {
        let table: toml::Table = toml::from_str(text).map_err(|source| RoleError::InvalidToml {
            path: path.to_path_buf(),
            source,
        })?;

        let mut config = Self::defaults();
        for (raw_role, value) in table {
            let role =
                FactoryRole::parse(&raw_role).map_err(|_| RoleError::UnknownRole(raw_role))?;
            let settings = value
                .as_table()
                .ok_or_else(|| RoleError::InvalidRole(role, "expected a table".to_string()))?;
            let current = config
                .roles
                .get_mut(&role)
                .ok_or(RoleError::MissingRole(role))?;
            apply_table(role, current, settings)?;
        }

        config.validate()?;
        Ok(config)
    }
}

/// A per-run override of one role's settings.
///
/// Only the fields that are set win; the rest come from the role configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunOverride {
    /// Role this override applies to.
    pub role: FactoryRole,
    /// Model to use instead of the configured one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Effort to use instead of the configured one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Harness to use instead of the configured one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<Harness>,
}

impl<'de> Deserialize<'de> for RunOverride {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            role: FactoryRole,
            #[serde(default)]
            model: Option<String>,
            #[serde(default)]
            effort: Option<Effort>,
            #[serde(default)]
            harness: Option<Harness>,
        }

        let raw = Raw::deserialize(deserializer)?;
        let over = Self {
            role: raw.role,
            model: raw.model,
            effort: raw.effort,
            harness: raw.harness,
        };
        over.validate().map_err(serde::de::Error::custom)?;
        Ok(over)
    }
}

impl RunOverride {
    /// An override that changes nothing until fields are set.
    pub fn new(role: FactoryRole) -> Self {
        Self {
            role,
            model: None,
            effort: None,
            harness: None,
        }
    }

    /// Checks the override's model name.
    pub fn validate(&self) -> Result<(), RoleError> {
        if let Some(model) = &self.model {
            validate_model(model).map_err(|reason| RoleError::InvalidModel {
                role: self.role,
                model: model.clone(),
                reason,
            })?;
        }
        Ok(())
    }
}

/// Errors raised while validating, loading or saving role configuration.
#[derive(Debug, Error)]
pub enum RoleError {
    /// A role name is not one of [`FactoryRole::ALL`].
    #[error("unknown role {0:?}")]
    UnknownRole(String),

    /// A harness name is not one of the supported harnesses.
    #[error("unknown harness {0:?}")]
    UnknownHarness(String),

    /// An effort name is not one of the supported effort levels.
    #[error("unknown effort {0:?}")]
    UnknownEffort(String),

    /// A role's settings are not a table.
    #[error("invalid role {0}: {1}")]
    InvalidRole(FactoryRole, String),

    /// A role is missing from the configuration.
    #[error("missing role {0}")]
    MissingRole(FactoryRole),

    /// A field inside a role's table is unknown.
    #[error("invalid field {field:?} in role {role:?}")]
    UnknownField {
        /// Role whose table contains the field.
        role: FactoryRole,
        /// Name of the unknown field.
        field: String,
    },

    /// A field inside a role's table has the wrong type.
    #[error("invalid field {field:?} in role {role:?}: {reason}")]
    InvalidField {
        /// Role whose table contains the field.
        role: FactoryRole,
        /// Name of the field.
        field: &'static str,
        /// Why the value is rejected.
        reason: String,
    },

    /// A model name does not follow the model-name rules.
    #[error("invalid model {model:?} for role {role:?}: {reason}")]
    InvalidModel {
        /// Role whose model is invalid.
        role: FactoryRole,
        /// The rejected model name.
        model: String,
        /// Why the value is rejected.
        reason: String,
    },

    /// Reading or writing `path` failed.
    #[error("io error at {}: {source}", path.display())]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A stored file is not valid TOML.
    #[error("invalid toml at {}: {source}", path.display())]
    InvalidToml {
        /// Path of the invalid file.
        path: PathBuf,
        /// TOML parser error.
        #[source]
        source: toml::de::Error,
    },

    /// The configuration could not be serialized to TOML.
    #[error("could not serialize role configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl RoleError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Applies the fields of one role table over the role's current settings.
fn apply_table(
    role: FactoryRole,
    settings: &mut RoleSettings,
    table: &toml::Table,
) -> Result<(), RoleError> {
    for (field, value) in table {
        match field.as_str() {
            "harness" => settings.harness = parse_harness(role, value)?,
            "model" => settings.model = parse_model(role, value)?,
            "effort" => settings.effort = parse_effort(role, value)?,
            _ => {
                return Err(RoleError::UnknownField {
                    role,
                    field: field.clone(),
                });
            }
        }
    }
    Ok(())
}

fn parse_harness(role: FactoryRole, value: &toml::Value) -> Result<Harness, RoleError> {
    let raw = value
        .as_str()
        .ok_or_else(|| invalid_field(role, "harness", "expected a string"))?;
    Harness::parse(raw)
        .map_err(|_| invalid_field(role, "harness", format!("unknown harness {raw:?}")))
}

fn parse_effort(role: FactoryRole, value: &toml::Value) -> Result<Effort, RoleError> {
    let raw = value
        .as_str()
        .ok_or_else(|| invalid_field(role, "effort", "expected a string"))?;
    Effort::parse(raw).map_err(|_| invalid_field(role, "effort", format!("unknown effort {raw:?}")))
}

fn parse_model(role: FactoryRole, value: &toml::Value) -> Result<String, RoleError> {
    let raw = value
        .as_str()
        .ok_or_else(|| invalid_field(role, "model", "expected a string"))?;
    validate_model(raw).map_err(|reason| RoleError::InvalidModel {
        role,
        model: raw.to_string(),
        reason,
    })?;
    Ok(raw.to_string())
}

fn invalid_field(role: FactoryRole, field: &'static str, reason: impl Into<String>) -> RoleError {
    RoleError::InvalidField {
        role,
        field,
        reason: reason.into(),
    }
}

/// Validates a model name: non-empty, bounded, and safe to pass to a launcher.
///
/// The name may hold `provider/model` pairs, but no empty, `.` or `..` path segment, no spaces,
/// control characters or other shell-significant characters.
fn validate_model(model: &str) -> Result<(), String> {
    if model.is_empty() {
        return Err("model must not be empty".to_string());
    }
    if model.len() > MAX_MODEL_LEN {
        return Err(format!("model must be at most {MAX_MODEL_LEN} bytes"));
    }
    if !model
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
    {
        return Err("model must start with an ASCII letter or digit".to_string());
    }
    if let Some(invalid) = model.chars().find(|c| !is_model_char(*c)) {
        return Err(format!("model contains invalid character {invalid:?}"));
    }
    if model
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err("model must not contain empty, `.` or `..` path segments".to_string());
    }
    Ok(())
}

fn is_model_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')
}

/// Writes `contents` to a temporary sibling of `path`, then renames it into place.
fn write_atomic(path: &Path, contents: &str) -> Result<(), RoleError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| RoleError::io(parent, source))?;

    let temp = temp_path(path);
    let written = fs::write(&temp, contents)
        .and_then(|()| fs::rename(&temp, path))
        .map_err(|source| RoleError::io(path, source));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// Unique temporary sibling of `path`, so the rename stays on the same filesystem.
fn temp_path(path: &Path) -> PathBuf {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.{}.{suffix}.tmp", std::process::id()))
}

impl fmt::Display for FactoryRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    /// Temporary root removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jgf-core-roles-{}-{suffix}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_path(root: &TempRoot) -> PathBuf {
        root.path().join("roles.toml")
    }

    fn settings(harness: Harness, model: &str, effort: Effort) -> RoleSettings {
        RoleSettings {
            harness,
            model: model.to_string(),
            effort,
        }
    }

    #[test]
    fn defaults_match_the_product_table() {
        let config = RoleConfig::defaults();
        let expected = [
            (
                FactoryRole::LabCoordinator,
                Harness::ClaudeCode,
                "sonnet",
                Effort::Medium,
            ),
            (
                FactoryRole::CoCoordinator,
                Harness::ClaudeCode,
                "sonnet",
                Effort::High,
            ),
            (FactoryRole::Coder, Harness::ClaudeCode, "sonnet", Effort::High),
            (FactoryRole::Tester, Harness::ClaudeCode, "sonnet", Effort::High),
            (
                FactoryRole::Reviewer,
                Harness::ClaudeCode,
                "opus",
                Effort::Medium,
            ),
            (
                FactoryRole::GitCoder,
                Harness::ClaudeCode,
                "haiku",
                Effort::Low,
            ),
            (FactoryRole::WakeUp, Harness::ClaudeCode, "haiku", Effort::Low),
        ];

        assert_eq!(config.roles().len(), expected.len());
        for (role, harness, model, effort) in expected {
            assert_eq!(
                config.get(role),
                Some(&settings(harness, model, effort)),
                "role {role}"
            );
        }
    }

    #[test]
    fn roles_have_stable_names_and_parse_back() {
        let names = [
            "lab-coordinator",
            "co-coordinator",
            "coder",
            "tester",
            "reviewer",
            "git-coder",
            "wake-up",
        ];
        assert_eq!(FactoryRole::ALL.len(), names.len());
        for (role, name) in FactoryRole::ALL.into_iter().zip(names) {
            assert_eq!(role.as_str(), name);
            assert_eq!(FactoryRole::parse(name).expect("parse role"), role);
        }
        assert!(matches!(
            FactoryRole::parse("planner"),
            Err(RoleError::UnknownRole(raw)) if raw == "planner"
        ));
    }

    #[test]
    fn effort_levels_are_ordered() {
        let ordered = [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ];
        assert!(ordered.windows(2).all(|pair| pair[0] < pair[1]));
        for effort in ordered {
            assert_eq!(
                Effort::parse(effort.as_str()).expect("parse effort"),
                effort
            );
        }
    }

    #[test]
    fn missing_file_loads_defaults() {
        let root = TempRoot::new();
        let loaded = RoleConfig::load(&sample_path(&root)).expect("missing file is defaults");
        assert_eq!(loaded, RoleConfig::defaults());
    }

    #[test]
    fn partial_file_overrides_only_named_roles() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        fs::write(
            &path,
            "[tester]\nharness = \"claude-code\"\nmodel = \"sonnet\"\neffort = \"max\"\n",
        )
        .expect("write partial file");

        let loaded = RoleConfig::load(&path).expect("load partial file");
        assert_eq!(
            loaded.get(FactoryRole::Tester),
            Some(&settings(Harness::ClaudeCode, "sonnet", Effort::Max))
        );
        assert_eq!(
            loaded.get(FactoryRole::Coder),
            RoleConfig::defaults().get(FactoryRole::Coder)
        );
        assert_eq!(
            loaded.get(FactoryRole::LabCoordinator),
            RoleConfig::defaults().get(FactoryRole::LabCoordinator)
        );
    }

    #[test]
    fn an_omitted_field_keeps_the_role_default() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        fs::write(&path, "[git-coder]\nmodel = \"other-model\"\n").expect("write partial role");

        let loaded = RoleConfig::load(&path).expect("load partial role");
        assert_eq!(
            loaded.get(FactoryRole::GitCoder),
            Some(&settings(Harness::ClaudeCode, "other-model", Effort::Low))
        );
    }

    #[test]
    fn unknown_role_names_the_role() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        fs::write(&path, "[planner]\nmodel = \"x\"\n").expect("write file");

        let error = RoleConfig::load(&path).expect_err("unknown role");
        let message = error.to_string();
        assert!(matches!(error, RoleError::UnknownRole(raw) if raw == "planner"));
        assert!(message.contains("role"), "{message}");
        assert!(message.contains("planner"), "{message}");
    }

    #[test]
    fn unknown_harness_names_the_field() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        fs::write(&path, "[coder]\nharness = \"vim\"\n").expect("write file");

        let error = RoleConfig::load(&path).expect_err("unknown harness");
        let message = error.to_string();
        assert!(matches!(
            error,
            RoleError::InvalidField {
                field: "harness",
                ..
            }
        ));
        assert!(message.contains("harness"), "{message}");
        assert!(message.contains("vim"), "{message}");
    }

    #[test]
    fn unknown_effort_names_the_field() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        fs::write(&path, "[coder]\neffort = \"turbo\"\n").expect("write file");

        let error = RoleConfig::load(&path).expect_err("unknown effort");
        let message = error.to_string();
        assert!(matches!(
            error,
            RoleError::InvalidField {
                field: "effort",
                ..
            }
        ));
        assert!(message.contains("effort"), "{message}");
        assert!(message.contains("turbo"), "{message}");
    }

    #[test]
    fn unknown_field_names_the_field() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        fs::write(&path, "[coder]\nprovider = \"x\"\n").expect("write file");

        let error = RoleConfig::load(&path).expect_err("unknown field");
        let message = error.to_string();
        assert!(matches!(error, RoleError::UnknownField { field, .. } if field == "provider"));
        assert!(message.contains("provider"), "{message}");
    }

    #[test]
    fn a_non_table_role_names_the_role() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        fs::write(&path, "coder = \"sonnet\"\n").expect("write file");

        let error = RoleConfig::load(&path).expect_err("invalid role value");
        assert!(matches!(
            error,
            RoleError::InvalidRole(FactoryRole::Coder, _)
        ));
    }

    #[test]
    fn bad_models_are_rejected_with_the_model_field() {
        let huge = "a".repeat(MAX_MODEL_LEN + 1);
        let hostile = ["../x", "a b", "a\0b", huge.as_str()];
        for model in hostile {
            let mut config = RoleConfig::defaults();
            config.set(
                FactoryRole::Coder,
                settings(Harness::Opencode, model, Effort::High),
            );

            let error = config.validate().expect_err("hostile model");
            let message = error.to_string();
            assert!(matches!(error, RoleError::InvalidModel { .. }), "{model:?}");
            assert!(message.contains("model"), "{message}");
        }
    }

    #[test]
    fn hostile_models_in_a_file_are_rejected() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        fs::write(&path, "[coder]\nmodel = \"../x\"\n").expect("write file");

        let error = RoleConfig::load(&path).expect_err("hostile model");
        assert!(matches!(error, RoleError::InvalidModel { .. }));
    }

    #[test]
    fn effort_is_not_restricted_by_harness() {
        let mut config = RoleConfig::defaults();
        config.set(
            FactoryRole::Coder,
            settings(Harness::Gemini, "gemini-flash", Effort::Max),
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn save_and_load_round_trip() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        let mut config = RoleConfig::defaults();
        config.set(
            FactoryRole::Reviewer,
            settings(Harness::ClaudeCode, "opus", Effort::XHigh),
        );

        config.save(&path).expect("save");
        let loaded = RoleConfig::load(&path).expect("load");

        assert_eq!(loaded, config);
    }

    #[test]
    fn atomic_save_leaves_only_the_config_file() {
        let root = TempRoot::new();
        let path = sample_path(&root);

        RoleConfig::defaults().save(&path).expect("save");
        RoleConfig::defaults().save(&path).expect("overwrite");

        let names: Vec<String> = fs::read_dir(root.path())
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["roles.toml".to_string()]);
    }

    #[test]
    fn resolve_applies_override_fields_one_by_one() {
        let config = RoleConfig::defaults();
        let role = FactoryRole::Coder;

        let model_only = config.resolve(
            role,
            &[RunOverride {
                role,
                model: Some("other-model".to_string()),
                effort: None,
                harness: None,
            }],
        );
        assert_eq!(
            model_only,
            settings(Harness::ClaudeCode, "other-model", Effort::High)
        );

        let full = config.resolve(
            role,
            &[RunOverride {
                role,
                model: Some("opus".to_string()),
                effort: Some(Effort::Max),
                harness: Some(Harness::ClaudeCode),
            }],
        );
        assert_eq!(full, settings(Harness::ClaudeCode, "opus", Effort::Max));
    }

    #[test]
    fn resolve_ignores_an_override_for_another_role() {
        let config = RoleConfig::defaults();
        let overrides = [RunOverride {
            role: FactoryRole::Tester,
            model: Some("other-model".to_string()),
            effort: Some(Effort::Max),
            harness: Some(Harness::ClaudeCode),
        }];

        let resolved = config.resolve(FactoryRole::Coder, &overrides);
        assert_eq!(resolved, *config.get(FactoryRole::Coder).expect("coder"));
    }

    #[test]
    fn try_resolve_rejects_a_hostile_override_model() {
        let config = RoleConfig::defaults();
        let error = config
            .try_resolve(
                FactoryRole::Coder,
                &[RunOverride {
                    role: FactoryRole::Coder,
                    model: Some("../x".to_string()),
                    effort: None,
                    harness: None,
                }],
            )
            .expect_err("hostile override model");
        assert!(matches!(error, RoleError::InvalidModel { .. }));
    }

    #[test]
    fn run_override_deserialization_validates_the_model() {
        let error = toml::from_str::<RunOverride>("role = \"coder\"\nmodel = \"../x\"\n")
            .expect_err("hostile override model");
        assert!(error.to_string().contains("model"), "{error}");

        let over: RunOverride =
            toml::from_str("role = \"coder\"\nmodel = \"other-model\"\n").expect("valid override");
        assert_eq!(over.role, FactoryRole::Coder);
        assert_eq!(over.model.as_deref(), Some("other-model"));
    }

    #[test]
    fn save_rejects_an_invalid_configuration() {
        let root = TempRoot::new();
        let path = sample_path(&root);
        let mut config = RoleConfig::defaults();
        config.set(
            FactoryRole::Coder,
            settings(Harness::Opencode, "has space", Effort::High),
        );

        assert!(config.save(&path).is_err());
        assert!(!path.exists());
    }
}
