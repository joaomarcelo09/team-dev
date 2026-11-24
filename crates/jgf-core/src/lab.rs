//! Lab configuration and the GitHub permissions the user grants it.

use serde::{Deserialize, Serialize};

/// Configuration of a Dark Factory lab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabConfig {
    /// Display name of the lab.
    pub name: String,
    /// GitHub permissions granted to the lab.
    pub github: GithubGrant,
}

impl LabConfig {
    /// A lab named `name` whose grant allows nothing until the user grants it.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            github: GithubGrant::default(),
        }
    }
}

/// GitHub permissions granted to a lab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubGrant {
    /// Owners (users or organizations) the lab may act on.
    pub owners: Vec<String>,
    /// Actions the lab may perform.
    pub actions: Vec<GithubAction>,
    /// Maximum number of pull requests the lab may open per day.
    pub max_prs_per_day: u32,
    /// Maximum number of pushes the lab may make per day.
    pub max_pushes_per_day: u32,
}

/// A GitHub action a lab can be allowed to perform.
///
/// This is the complete list: destructive or administrative operations are not representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubAction {
    /// Create a new repository.
    CreateRepo,
    /// Push a task branch.
    PushTaskBranch,
    /// Push a branch the factory owns during repository setup, such as the default or
    /// integration branch.
    PushSetupBranch,
    /// Open a pull request.
    OpenPr,
    /// Merge a pull request.
    MergePr,
    /// Read the CI status of a branch or pull request.
    ReadCiStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct ActionInput {
        action: GithubAction,
    }

    #[test]
    fn new_lab_grants_nothing() {
        let lab = LabConfig::new("alpha");
        assert_eq!(lab.name, "alpha");
        assert!(lab.github.owners.is_empty());
        assert!(lab.github.actions.is_empty());
        assert_eq!(lab.github.max_prs_per_day, 0);
        assert_eq!(lab.github.max_pushes_per_day, 0);
    }

    #[test]
    fn grants_use_snake_case() {
        let text = "action = \"push_task_branch\"";
        let input: ActionInput = toml::from_str(text).expect("known action");
        assert_eq!(input.action, GithubAction::PushTaskBranch);

        let text = "action = \"push_setup_branch\"";
        let input: ActionInput = toml::from_str(text).expect("known action");
        assert_eq!(input.action, GithubAction::PushSetupBranch);

        let text = "action = \"read_ci_status\"";
        let input: ActionInput = toml::from_str(text).expect("known action");
        assert_eq!(input.action, GithubAction::ReadCiStatus);
    }

    #[test]
    fn actions_outside_the_enum_are_rejected() {
        let text = "action = \"force_push\"";
        let result: Result<ActionInput, _> = toml::from_str(text);
        assert!(result.is_err());
    }
}
