//! Git flow and merge policy of the Dark Factory.
//!
//! The policy is pure: it reads a [`MergeContext`] and a target branch and decides whether a
//! pull request may merge there. Task branches integrate into the project's integration branch
//! (`develop` or `stg`), which may merge autonomously. An MVP project has no production yet, so
//! it may also merge into its default branch; a project in production may merge into the default
//! branch only with the user's explicit approval. Every other target is refused.
//!
//! Branch names are compared exactly: no trimming, no case folding and no `refs/heads/` prefix.
//! Names are validated with the broker's branch rules, so an empty name, whitespace or a refspec
//! is an error rather than a target.

use jgf_core::ProjectStage;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::broker::branch_is_valid;

/// Integration branch a project uses when it does not choose one.
pub const DEFAULT_INTEGRATION_BRANCH: &str = "develop";

/// Integration branches a project may use.
const INTEGRATION_BRANCHES: [&str; 2] = ["develop", "stg"];

/// What the factory knows about a project's branches and the user's word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeContext {
    /// Stage of the project.
    pub stage: ProjectStage,
    /// Integration branch task branches merge into, or `None` when the project has none.
    pub integration_branch: Option<String>,
    /// The repository's default branch.
    pub default_branch: String,
    /// Whether the user explicitly approved a merge into the default branch.
    pub user_approved: bool,
}

impl Default for MergeContext {
    /// An MVP context integrating on `develop` and falling back to `main`.
    fn default() -> Self {
        Self {
            stage: ProjectStage::Mvp,
            integration_branch: Some(DEFAULT_INTEGRATION_BRANCH.to_string()),
            default_branch: "main".to_string(),
            user_approved: false,
        }
    }
}

/// The branch class a merge targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeTarget {
    /// The project's integration branch.
    Integration,
    /// The project's default branch.
    DefaultBranch,
}

/// A merge the policy allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeAllowed {
    /// Branch class the merge targets.
    pub target: MergeTarget,
}

/// Why the merge policy refuses a target, or cannot decide.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MergeDenied {
    /// The target is not a valid branch name.
    #[error("invalid merge target branch {branch:?}")]
    InvalidTargetBranch {
        /// Branch the caller wanted to merge into.
        branch: String,
    },

    /// The configured integration branch is not `develop` or `stg`.
    #[error("integration branch {branch:?} must be develop or stg")]
    InvalidIntegrationBranch {
        /// Branch the context named.
        branch: String,
    },

    /// The configured default branch is not a valid branch name.
    #[error("invalid default branch {branch:?}")]
    InvalidDefaultBranch {
        /// Branch the context named.
        branch: String,
    },

    /// Merging into the default branch needs the user's explicit word.
    #[error("merging into the default branch needs the user's explicit approval")]
    NeedsUserApproval,

    /// The target is neither the integration nor the default branch.
    #[error("merging into this branch is not allowed")]
    TargetNotAllowed,

    /// The project has no integration branch and is not an MVP.
    #[error("the project has no integration branch")]
    NoIntegrationBranch,
}

/// Decides whether a pull request may merge into `target_branch`.
///
/// The integration branch, when it differs from the default branch, always allows the merge.
/// The default branch allows the merge for an MVP project, and for a production project only
/// when the user approved it. Any other target is refused.
pub fn merge_decision(
    ctx: &MergeContext,
    target_branch: &str,
) -> Result<MergeAllowed, MergeDenied> {
    let default = checked_default(ctx)?;
    let integration = checked_integration(ctx)?;
    if !branch_is_valid(target_branch) {
        return Err(MergeDenied::InvalidTargetBranch {
            branch: target_branch.to_string(),
        });
    }
    if let Some(integration) = integration
        && integration != default
        && integration == target_branch
    {
        return Ok(MergeAllowed {
            target: MergeTarget::Integration,
        });
    }
    if target_branch != default {
        return Err(MergeDenied::TargetNotAllowed);
    }
    match ctx.stage {
        ProjectStage::Mvp => Ok(MergeAllowed {
            target: MergeTarget::DefaultBranch,
        }),
        ProjectStage::Production if ctx.user_approved => Ok(MergeAllowed {
            target: MergeTarget::DefaultBranch,
        }),
        ProjectStage::Production => Err(MergeDenied::NeedsUserApproval),
    }
}

/// Branch task branches are cut from and pull requests target.
///
/// This is the project's integration branch. A project without one falls back to its default
/// branch only while it is an MVP; a production project without an integration branch is an
/// error rather than an implicit merge into the default branch.
pub fn pr_base_branch(ctx: &MergeContext) -> Result<&str, MergeDenied> {
    let default = checked_default(ctx)?;
    let integration = checked_integration(ctx)?;
    match integration {
        Some(integration) => Ok(integration),
        None if ctx.stage == ProjectStage::Mvp => Ok(default),
        None => Err(MergeDenied::NoIntegrationBranch),
    }
}

/// Returns the default branch after checking its name.
fn checked_default(ctx: &MergeContext) -> Result<&str, MergeDenied> {
    if branch_is_valid(&ctx.default_branch) {
        Ok(&ctx.default_branch)
    } else {
        Err(MergeDenied::InvalidDefaultBranch {
            branch: ctx.default_branch.clone(),
        })
    }
}

/// Returns the integration branch after checking it is `develop` or `stg`.
fn checked_integration(ctx: &MergeContext) -> Result<Option<&str>, MergeDenied> {
    match ctx.integration_branch.as_deref() {
        None => Ok(None),
        Some(branch) if INTEGRATION_BRANCHES.contains(&branch) => Ok(Some(branch)),
        Some(branch) => Err(MergeDenied::InvalidIntegrationBranch {
            branch: branch.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(
        stage: ProjectStage,
        integration: Option<&str>,
        user_approved: bool,
    ) -> MergeContext {
        MergeContext {
            stage,
            integration_branch: integration.map(str::to_string),
            default_branch: "main".to_string(),
            user_approved,
        }
    }

    fn allowed(target: MergeTarget) -> Result<MergeAllowed, MergeDenied> {
        Ok(MergeAllowed { target })
    }

    #[test]
    fn merge_decision_matrix() {
        let cases = [
            (
                ProjectStage::Mvp,
                false,
                "develop",
                allowed(MergeTarget::Integration),
            ),
            (
                ProjectStage::Mvp,
                true,
                "develop",
                allowed(MergeTarget::Integration),
            ),
            (
                ProjectStage::Mvp,
                false,
                "main",
                allowed(MergeTarget::DefaultBranch),
            ),
            (
                ProjectStage::Mvp,
                true,
                "main",
                allowed(MergeTarget::DefaultBranch),
            ),
            (
                ProjectStage::Mvp,
                false,
                "other",
                Err(MergeDenied::TargetNotAllowed),
            ),
            (
                ProjectStage::Mvp,
                true,
                "other",
                Err(MergeDenied::TargetNotAllowed),
            ),
            (
                ProjectStage::Production,
                false,
                "develop",
                allowed(MergeTarget::Integration),
            ),
            (
                ProjectStage::Production,
                true,
                "develop",
                allowed(MergeTarget::Integration),
            ),
            (
                ProjectStage::Production,
                false,
                "main",
                Err(MergeDenied::NeedsUserApproval),
            ),
            (
                ProjectStage::Production,
                true,
                "main",
                allowed(MergeTarget::DefaultBranch),
            ),
            (
                ProjectStage::Production,
                false,
                "other",
                Err(MergeDenied::TargetNotAllowed),
            ),
            (
                ProjectStage::Production,
                true,
                "other",
                Err(MergeDenied::TargetNotAllowed),
            ),
        ];
        for (stage, user_approved, target, expected) in cases {
            let ctx = context(stage, Some(DEFAULT_INTEGRATION_BRANCH), user_approved);
            assert_eq!(
                merge_decision(&ctx, target),
                expected,
                "{stage:?} {target:?}"
            );
        }
    }

    #[test]
    fn invalid_branch_names_are_refused_for_every_stage() {
        for stage in [ProjectStage::Mvp, ProjectStage::Production] {
            for user_approved in [false, true] {
                let ctx = context(stage, Some(DEFAULT_INTEGRATION_BRANCH), user_approved);
                for target in ["", "refs/heads/main", "main "] {
                    assert!(
                        matches!(
                            merge_decision(&ctx, target),
                            Err(MergeDenied::InvalidTargetBranch { .. })
                        ),
                        "{stage:?} {target:?}"
                    );
                }
                assert_eq!(
                    merge_decision(&ctx, "MAIN"),
                    Err(MergeDenied::TargetNotAllowed),
                    "branch names are compared exactly"
                );
            }
        }
    }

    #[test]
    fn an_unsupported_integration_branch_is_refused() {
        let ctx = context(ProjectStage::Mvp, Some("release"), false);
        assert_eq!(
            merge_decision(&ctx, "release"),
            Err(MergeDenied::InvalidIntegrationBranch {
                branch: "release".to_string()
            })
        );
        assert_eq!(
            merge_decision(&ctx, "main"),
            Err(MergeDenied::InvalidIntegrationBranch {
                branch: "release".to_string()
            })
        );
        assert_eq!(
            pr_base_branch(&ctx),
            Err(MergeDenied::InvalidIntegrationBranch {
                branch: "release".to_string()
            })
        );
    }

    #[test]
    fn stg_is_a_valid_integration_branch() {
        let ctx = context(ProjectStage::Production, Some("stg"), false);
        assert_eq!(
            merge_decision(&ctx, "stg"),
            allowed(MergeTarget::Integration)
        );
        assert_eq!(pr_base_branch(&ctx), Ok("stg"));
    }

    #[test]
    fn without_an_integration_branch_only_an_mvp_falls_back_to_default() {
        let mvp = context(ProjectStage::Mvp, None, false);
        assert_eq!(pr_base_branch(&mvp), Ok("main"));
        assert_eq!(
            merge_decision(&mvp, "main"),
            allowed(MergeTarget::DefaultBranch)
        );

        let production = context(ProjectStage::Production, None, false);
        assert_eq!(
            pr_base_branch(&production),
            Err(MergeDenied::NoIntegrationBranch)
        );
        assert_eq!(
            merge_decision(&production, "main"),
            Err(MergeDenied::NeedsUserApproval)
        );
        assert_eq!(
            merge_decision(&production, "develop"),
            Err(MergeDenied::TargetNotAllowed)
        );

        let approved = context(ProjectStage::Production, None, true);
        assert_eq!(
            merge_decision(&approved, "main"),
            allowed(MergeTarget::DefaultBranch)
        );
    }

    #[test]
    fn an_integration_branch_equal_to_default_uses_the_default_rule() {
        let ctx = MergeContext {
            stage: ProjectStage::Production,
            integration_branch: Some("develop".to_string()),
            default_branch: "develop".to_string(),
            user_approved: false,
        };
        assert_eq!(
            merge_decision(&ctx, "develop"),
            Err(MergeDenied::NeedsUserApproval)
        );

        let mvp = MergeContext {
            stage: ProjectStage::Mvp,
            ..ctx
        };
        assert_eq!(
            merge_decision(&mvp, "develop"),
            allowed(MergeTarget::DefaultBranch)
        );
    }

    #[test]
    fn a_default_branch_must_be_a_valid_branch_name() {
        let ctx = context(ProjectStage::Mvp, None, false);
        for default_branch in ["", "refs/heads/main", "main x"] {
            let ctx = MergeContext {
                default_branch: default_branch.to_string(),
                ..ctx.clone()
            };
            assert_eq!(
                merge_decision(&ctx, "main"),
                Err(MergeDenied::InvalidDefaultBranch {
                    branch: default_branch.to_string()
                })
            );
            assert_eq!(
                pr_base_branch(&ctx),
                Err(MergeDenied::InvalidDefaultBranch {
                    branch: default_branch.to_string()
                })
            );
        }
    }

    #[test]
    fn the_default_context_uses_develop() {
        let ctx = MergeContext::default();
        assert_eq!(ctx.integration_branch.as_deref(), Some("develop"));
        assert_eq!(pr_base_branch(&ctx), Ok("develop"));
    }
}
