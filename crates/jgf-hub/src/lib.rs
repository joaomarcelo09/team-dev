//! Dark Factory hub: separate from the stable jg-team-dev app.

pub mod board;

pub use board::{Actor, Board, BoardError, BoardFilter, Column, Outcome, OutcomeStatus, Task};
