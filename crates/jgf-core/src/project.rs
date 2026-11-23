//! Project model: what the factory builds and how it runs it.

use std::{fmt, path::PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ModelError;

/// Maximum length of a [`ProjectId`].
const MAX_ID_LEN: usize = 40;

/// Identifier of a project.
///
/// It has 1 to 40 characters, starts with a lowercase letter, and otherwise contains only
/// lowercase letters, digits and `-`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectId(String);

impl ProjectId {
    /// Parses and validates a project id.
    pub fn parse(raw: &str) -> Result<Self, ModelError> {
        if !Self::is_valid(raw) {
            return Err(ModelError::InvalidProjectId(raw.to_string()));
        }
        Ok(Self(raw.to_string()))
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid(raw: &str) -> bool {
        !raw.is_empty()
            && raw.len() <= MAX_ID_LEN
            && raw.starts_with(|c: char| c.is_ascii_lowercase())
            && raw
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ProjectId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProjectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A project the Dark Factory builds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Stable project id.
    pub id: ProjectId,
    /// Display name.
    pub name: String,
    /// Path of the project specification.
    pub spec_path: PathBuf,
    /// SHA-256 of the specification, once it has been hashed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_sha256: Option<String>,
    /// Repositories that make up the project.
    pub repos: Vec<RepoSpec>,
    /// Stage the project is in.
    pub stage: ProjectStage,
    /// Environment profile of the project.
    pub env_profile: EnvProfile,
    /// Commands the factory may run.
    pub recipes: Vec<RunRecipe>,
}

/// A repository that belongs to a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSpec {
    /// Repository name.
    pub name: String,
    /// Git remote URL, when the repository exists remotely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Branch the repository starts from.
    pub default_branch: String,
}

/// Stage of a project, from first product to production.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStage {
    /// The first minimal product.
    Mvp,
    /// Production.
    Production,
}

/// Names of environment variables a project may use.
///
/// Only key names are stored here; values are never part of the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvProfile {
    /// Allowed environment variable names.
    pub allowed_keys: Vec<String>,
}

/// A command the factory may run in a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecipe {
    /// Recipe name.
    pub name: String,
    /// Command to run.
    pub command: String,
    /// Working directory relative to the repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    struct IdInput {
        id: ProjectId,
    }

    #[test]
    fn valid_ids_parse() {
        let max = "a".repeat(MAX_ID_LEN);
        for raw in ["a", "a1", "alpha-beta-2", max.as_str()] {
            let id = ProjectId::parse(raw).expect("valid id");
            assert_eq!(id.as_str(), raw);
        }
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let too_long = "a".repeat(MAX_ID_LEN + 1);
        for raw in ["../x", "Abc", "", "1a", too_long.as_str()] {
            assert!(
                ProjectId::parse(raw).is_err(),
                "accepted invalid id {raw:?}"
            );
        }
    }

    #[test]
    fn id_serializes_as_a_plain_string_and_validates_on_deserialize() {
        let id = ProjectId::parse("alpha").expect("valid id");
        let text = toml::to_string(&IdInput { id }).expect("serialize");
        assert_eq!(text, "id = \"alpha\"\n");

        let input: IdInput = toml::from_str("id = \"alpha\"").expect("deserialize");
        assert_eq!(input.id.as_str(), "alpha");

        let result: Result<IdInput, _> = toml::from_str("id = \"../x\"");
        assert!(result.is_err());
    }
}
