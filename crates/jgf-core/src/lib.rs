//! Dark Factory core: shared types and locations for the `jg-factory` app.
//!
//! Dark Factory is a separate product from the stable `jg-team-dev` app. It must never
//! use the stable state directory (`.team`, `~/.local/share/jg-team-dev`).

pub mod audit;
pub mod error;
pub mod lab;
pub mod paths;
pub mod project;
pub mod registry;
pub mod roles;
pub mod secrets;
pub mod spec;

pub use audit::{
    AuditEvent, AuditFilter, AuditKind, AuditLog, CheckOutcome, CheckStatus, redact_argv,
};
pub use error::ModelError;
pub use lab::{GithubAction, GithubGrant, LabConfig};
pub use project::{EnvProfile, Project, ProjectId, ProjectStage, RepoSpec, RunRecipe};
pub use roles::{Effort, FactoryRole, Harness, RoleConfig, RoleError, RoleSettings, RunOverride};
pub use secrets::{FileSecretStore, Secret, SecretKey, SecretScope, SecretStore, env_for_project};
pub use spec::{LintIssue, Spec, SpecSection, freeze_spec, lint_spec, parse_spec, verify_spec};
