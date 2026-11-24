//! Settings view model: one row per role and edits that only land when the config stays valid.

use jgf_core::roles::{Effort, FactoryRole, Harness, RoleConfig, RoleError, RoleSettings};
use thiserror::Error;

/// The settings screen: one row per role, in configuration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsForm {
    /// Rows for every role that has settings.
    pub rows: Vec<SettingsRow>,
}

/// One role's editable settings, as plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsRow {
    /// Role this row edits.
    pub role: FactoryRole,
    /// Plain-text role label.
    pub label: &'static str,
    /// Selected harness name.
    pub harness: String,
    /// Model name.
    pub model: String,
    /// Selected effort name.
    pub effort: String,
}

/// An edit of one field of a role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldEdit {
    /// Replace the harness, parsed with [`Harness::parse`].
    Harness(String),
    /// Replace the model name; it is validated with [`RoleConfig::validate`].
    Model(String),
    /// Replace the effort, parsed with [`Effort::parse`].
    Effort(String),
}

/// Errors raised by [`apply_edit`].
#[derive(Debug, Error)]
pub enum SettingsError {
    /// The configuration has no settings for the role.
    #[error("missing role {0}")]
    MissingRole(FactoryRole),

    /// The harness name is not one of the supported harnesses.
    #[error("unknown harness {0:?}")]
    UnknownHarness(String),

    /// The effort name is not one of the supported effort levels.
    #[error("unknown effort {0:?}")]
    UnknownEffort(String),

    /// The model name does not follow the model-name rules.
    #[error("invalid model for role {role}: {reason}")]
    InvalidModel {
        /// Role whose model was rejected.
        role: FactoryRole,
        /// Why the model is rejected.
        reason: String,
    },

    /// A configuration error outside the model-name rules.
    #[error("invalid role configuration: {0}")]
    InvalidConfig(String),
}

impl SettingsForm {
    /// Builds the form from `config`, one row per [`FactoryRole::ALL`] entry that has settings.
    pub fn from_config(config: &RoleConfig) -> Self {
        Self {
            rows: FactoryRole::ALL
                .into_iter()
                .filter_map(|role| {
                    config
                        .get(role)
                        .map(|settings| SettingsRow::new(role, settings))
                })
                .collect(),
        }
    }
}

impl SettingsRow {
    fn new(role: FactoryRole, settings: &RoleSettings) -> Self {
        Self {
            role,
            label: role_label(role),
            harness: settings.harness.as_str().to_string(),
            model: settings.model.clone(),
            effort: settings.effort.as_str().to_string(),
        }
    }
}

/// The plain-text label of `role`.
pub fn role_label(role: FactoryRole) -> &'static str {
    match role {
        FactoryRole::LabCoordinator => "Lab Coordinator",
        FactoryRole::CoCoordinator => "Co-Coordinator",
        FactoryRole::Coder => "Coder",
        FactoryRole::Tester => "Tester",
        FactoryRole::Reviewer => "Reviewer",
        FactoryRole::GitCoder => "Git Coder",
        FactoryRole::WakeUp => "Wake-Up",
    }
}

/// Harness names for the harness dropdown.
pub fn harness_options() -> &'static [&'static str] {
    &["claude-code", "opencode", "gemini"]
}

/// Effort names for the effort dropdown.
pub fn effort_options() -> &'static [&'static str] {
    &["low", "medium", "high", "xhigh", "max"]
}

/// Applies `edit` to `role` in `config`, leaving the config unchanged on error.
///
/// The model is checked with [`RoleConfig::validate`], so it follows the same rules as loading
/// the configuration from disk.
pub fn apply_edit(
    config: &mut RoleConfig,
    role: FactoryRole,
    edit: FieldEdit,
) -> Result<(), SettingsError> {
    let Some(mut settings) = config.get(role).cloned() else {
        return Err(SettingsError::MissingRole(role));
    };

    match edit {
        FieldEdit::Harness(raw) => {
            settings.harness =
                Harness::parse(&raw).map_err(|_| SettingsError::UnknownHarness(raw))?;
        }
        FieldEdit::Model(model) => settings.model = model,
        FieldEdit::Effort(raw) => {
            settings.effort = Effort::parse(&raw).map_err(|_| SettingsError::UnknownEffort(raw))?;
        }
    }

    let mut candidate = config.clone();
    candidate.set(role, settings);
    candidate.validate()?;
    *config = candidate;
    Ok(())
}

impl From<RoleError> for SettingsError {
    fn from(error: RoleError) -> Self {
        match error {
            RoleError::MissingRole(role) => Self::MissingRole(role),
            RoleError::InvalidModel { role, reason, .. } => Self::InvalidModel { role, reason },
            other => Self::InvalidConfig(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_has_one_row_per_role() {
        let config = RoleConfig::defaults();
        let form = SettingsForm::from_config(&config);
        assert_eq!(form.rows.len(), FactoryRole::ALL.len());

        let cases = [
            (FactoryRole::LabCoordinator, "Lab Coordinator"),
            (FactoryRole::CoCoordinator, "Co-Coordinator"),
            (FactoryRole::Coder, "Coder"),
            (FactoryRole::Tester, "Tester"),
            (FactoryRole::Reviewer, "Reviewer"),
            (FactoryRole::GitCoder, "Git Coder"),
            (FactoryRole::WakeUp, "Wake-Up"),
        ];
        for (role, label) in cases {
            let row = form
                .rows
                .iter()
                .find(|row| row.role == role)
                .expect("row for role");
            let settings = config.get(role).expect("settings for role");
            assert_eq!(row.label, label);
            assert_eq!(row.harness, settings.harness.as_str());
            assert_eq!(row.model, settings.model);
            assert_eq!(row.effort, settings.effort.as_str());
        }
    }

    #[test]
    fn valid_edits_change_the_config() {
        type ConfigCheck = fn(&RoleConfig) -> bool;
        let cases: [(FieldEdit, ConfigCheck); 3] = [
            (
                FieldEdit::Harness("gemini".to_string()),
                |config: &RoleConfig| {
                    config.get(FactoryRole::Coder).unwrap().harness.as_str() == "gemini"
                },
            ),
            (
                FieldEdit::Model("provider/model-1".to_string()),
                |config: &RoleConfig| {
                    config.get(FactoryRole::Coder).unwrap().model == "provider/model-1"
                },
            ),
            (
                FieldEdit::Effort("max".to_string()),
                |config: &RoleConfig| {
                    config.get(FactoryRole::Coder).unwrap().effort.as_str() == "max"
                },
            ),
        ];
        for (edit, check) in cases {
            let mut config = RoleConfig::defaults();
            apply_edit(&mut config, FactoryRole::Coder, edit).expect("valid edit");
            assert!(check(&config));
        }
    }

    #[test]
    fn invalid_edits_leave_the_config_unchanged() {
        let long_model = "a".repeat(121);
        let cases = [
            FieldEdit::Harness("vim".to_string()),
            FieldEdit::Effort("ultra".to_string()),
            FieldEdit::Model(String::new()),
            FieldEdit::Model(long_model),
            FieldEdit::Model("-leading-dash".to_string()),
            FieldEdit::Model("provider//model".to_string()),
        ];
        for edit in cases {
            let mut config = RoleConfig::defaults();
            let before = config.clone();
            assert!(apply_edit(&mut config, FactoryRole::Coder, edit).is_err());
            assert_eq!(config, before);
        }
    }

    #[test]
    fn options_lists_match_the_enums() {
        let harnesses: Vec<&str> = harness_options().to_vec();
        assert_eq!(harnesses, ["claude-code", "opencode", "gemini"]);
        for name in harnesses {
            assert!(Harness::parse(name).is_ok());
        }

        let efforts: Vec<&str> = effort_options().to_vec();
        assert_eq!(efforts, ["low", "medium", "high", "xhigh", "max"]);
        for name in efforts {
            assert!(Effort::parse(name).is_ok());
        }
    }

    #[test]
    fn save_and_load_round_trip_in_a_temp_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("roles.toml");

        let mut config = RoleConfig::defaults();
        apply_edit(
            &mut config,
            FactoryRole::Coder,
            FieldEdit::Model("provider/model-1".to_string()),
        )
        .expect("valid edit");
        config.save(&path).expect("save config");

        let loaded = RoleConfig::load(&path).expect("load config");
        assert_eq!(loaded, config);
    }
}
