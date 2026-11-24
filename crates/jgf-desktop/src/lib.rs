//! Dark Factory desktop app: view models over the hub and core state.
//!
//! The desktop shell in `main.rs` draws these models. This crate builds plain data for it: a
//! Board view, a lab view, a settings form and a read-only snapshot of the files under the data
//! directory. Everything here is pure: no GUI, no I/O except what a function is handed, and
//! every time-dependent function takes `now` in unix seconds.

pub mod board_view;
pub mod chat_view;
pub mod lab_view;
pub mod settings_view;
pub mod snapshot;
