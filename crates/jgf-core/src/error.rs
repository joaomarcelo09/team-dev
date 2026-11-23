//! Errors raised by the Dark Factory model and its persistence layer.

use std::path::PathBuf;

use thiserror::Error;

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
