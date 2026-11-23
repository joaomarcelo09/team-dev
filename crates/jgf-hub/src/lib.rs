//! Dark Factory hub: separate from the stable jg-team-dev app.

pub mod board;
pub mod routing;

pub use board::{Actor, Board, BoardError, BoardFilter, Column, Outcome, OutcomeStatus, Task};
pub use routing::{
    CoordinatorConfig, MAX_BODY_BYTES, Message, MessageKind, OutgoingMessage, Role, Router,
    RoutingError, WorkerKind,
};
