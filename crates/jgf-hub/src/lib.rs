//! Dark Factory hub: separate from the stable jg-team-dev app.

pub mod board;
pub mod routing;
pub mod scheduler;

pub use board::{Actor, Board, BoardError, BoardFilter, Column, Outcome, OutcomeStatus, Task};
pub use routing::{
    CoordinatorConfig, MAX_BODY_BYTES, Message, MessageKind, OutgoingMessage, Role, Router,
    RoutingError, WorkerKind,
};
pub use scheduler::{
    CoderGrant, CoderSlot, Limits, MAX_CODERS, MAX_TEAMS, Mode, ReviewTicket, Scheduler,
    SchedulerError, SlotId, Switch, WaitReason,
};
