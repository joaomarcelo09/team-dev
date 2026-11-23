//! Audit events and the append-only log that records them.
//!
//! A lab keeps one JSONL file at `<dir>/labs/<lab>/audit.jsonl`, where `dir` is the data
//! directory (normally `paths::data_dir()`). Each line is one [`AuditEvent`]. Events are
//! numbered from 1 and chained by SHA-256, so removing or editing a line breaks the chain.
//!
//! Secret values and environment values never enter an event: command arguments go through
//! [`redact_argv`], and the writer caps every free-form field.

mod event;
mod filter;
mod log;

pub use event::{AuditEvent, AuditKind, CheckOutcome, CheckStatus, redact_argv};
pub use filter::AuditFilter;
pub use log::AuditLog;
