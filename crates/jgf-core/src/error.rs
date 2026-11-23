//! Errors raised by the Dark Factory model and its persistence layer.

use std::path::PathBuf;

use thiserror::Error;

use crate::spec::LintIssue;

/// Errors produced while validating, loading or saving Dark Factory data.
#[derive(Debug, Error)]
pub enum ModelError {
    /// A project id does not follow the id rules.
    #[error("invalid project id {0:?}")]
    InvalidProjectId(String),

    /// A secret key does not follow the environment-variable name rules.
    #[error("invalid secret key {0:?}")]
    InvalidSecretKey(String),

    /// The requested model file does not exist.
    #[error("not found: {}", .0.display())]
    NotFound(PathBuf),

    /// Reading or writing `path` failed.
    #[error("io error at {}: {source}", path.display())]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A stored file is not valid TOML for its model.
    #[error("invalid toml at {}: {source}", path.display())]
    InvalidToml {
        /// Path of the invalid file.
        path: PathBuf,
        /// TOML parser error.
        #[source]
        source: toml::de::Error,
    },

    /// A model could not be serialized to TOML.
    #[error("could not serialize model: {0}")]
    Serialize(#[from] toml::ser::Error),

    /// A value could not be converted to or from JSON.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// An audit event field does not follow the field rules.
    #[error("invalid audit field {field}: {reason}")]
    InvalidAuditField {
        /// Name of the field.
        field: &'static str,
        /// Why the value is rejected.
        reason: String,
    },

    /// An audit log path is a symlink or another non-regular file.
    #[error("refusing unsafe audit log path {}", .0.display())]
    UnsafeAuditPath(PathBuf),

    /// An audit log is damaged and cannot be appended to or verified.
    #[error("audit log at {} is corrupt: {detail}", .path.display())]
    AuditCorrupt {
        /// Path of the log.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },

    /// A specification did not pass intake linting.
    #[error("spec at {} failed lint with {} issue(s)", .path.display(), .issues.len())]
    InvalidSpec {
        /// Path of the rejected source.
        path: PathBuf,
        /// All issues found by the linter.
        issues: Vec<LintIssue>,
    },

    /// A project already has a frozen specification, which is never replaced.
    #[error("spec at {} is already frozen", .0.display())]
    SpecAlreadyFrozen(PathBuf),

    /// A project has no frozen specification hash to verify against.
    #[error("spec at {} has no frozen hash", .0.display())]
    MissingSpecHash(PathBuf),

    /// A frozen specification no longer matches its stored hash.
    #[error("spec hash mismatch at {}: expected {expected}, got {actual}", .path.display())]
    SpecHashMismatch {
        /// Path of the frozen specification.
        path: PathBuf,
        /// Hash stored in the project.
        expected: String,
        /// Hash computed from the file.
        actual: String,
    },
}

impl ModelError {
    /// An I/O error that happened at `path`.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
