//! Dark Factory hub: separate from the stable jg-team-dev app.

pub mod board;
pub mod broker;
pub mod broker_exec;
pub mod ci_watch;
pub mod git_flow;
pub mod routing;
pub mod scheduler;
pub mod usage;

pub use board::{Actor, Board, BoardError, BoardFilter, Column, Outcome, OutcomeStatus, Task};
pub use broker::{
    Approved, Broker, BrokerDenied, CommitError, DEFAULT_TASK_BRANCH_PREFIX, GithubRequest,
    clean_pr_text, validate_commit_message,
};
pub use broker_exec::{
    BackendError, BackendOp, BackendOutput, BrokerError, GITHUB_TOKEN_KEY, GhCliBackend,
    GithubBackend, testing::FakeBackend,
};
pub use ci_watch::{CiError, CiState, CiWatch, Decision, MAX_FIX_CYCLES, parse_ci_runs};
pub use git_flow::{
    DEFAULT_INTEGRATION_BRANCH, MergeAllowed, MergeContext, MergeDenied, MergeTarget,
    merge_decision, pr_base_branch,
};
pub use routing::{
    CoordinatorConfig, MAX_BODY_BYTES, Message, MessageKind, OutgoingMessage, Role, Router,
    RoutingError, WorkerKind,
};
pub use scheduler::{
    CoderGrant, CoderSlot, Limits, MAX_CODERS, MAX_TEAMS, Mode, ReviewTicket, Scheduler,
    SchedulerError, SlotId, Switch, WaitReason,
};
pub use usage::{
    HARD_PERCENT, LabState, Level, Parked, Policy, Provider, RESUME_DELAY_SECS, Reading,
    ReadingState, ResumeQueue, SOFT_PERCENT, STALE_SECS, Throttle, UsageError, UsageMeter, Window,
    lab_state,
};
