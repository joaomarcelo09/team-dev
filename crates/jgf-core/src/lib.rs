//! Dark Factory core: shared types and locations for the `jg-factory` app.
//!
//! Dark Factory is a separate product from the stable `jg-team-dev` app. It must never
//! use the stable state directory (`.team`, `~/.local/share/jg-team-dev`).

pub mod error;
pub mod lab;
pub mod paths;
pub mod project;
pub mod registry;
pub mod secrets;

pub use error::ModelError;
pub use lab::{GithubAction, GithubGrant, LabConfig};
pub use project::{EnvProfile, Project, ProjectId, ProjectStage, RepoSpec, RunRecipe};
pub use secrets::{FileSecretStore, Secret, SecretKey, SecretScope, SecretStore, env_for_project};
