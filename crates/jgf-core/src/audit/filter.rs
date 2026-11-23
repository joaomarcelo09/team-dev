//! Selection of audit events while reading a log.

use super::event::AuditEvent;

/// Selects events when reading an audit log.
///
/// Every set condition must match: the project, worker and kind tag must be equal, and
/// `at_ms` must fall inside the inclusive time range. A filter with no condition matches
/// every event. Builders set one condition at a time:
///
/// ```text
/// let filter = AuditFilter::new().project("demo").kind("command").tail(10);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    project: Option<String>,
    worker: Option<String>,
    kind: Option<String>,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    tail: Option<usize>,
}

impl AuditFilter {
    /// A filter that matches every event.
    pub fn new() -> Self {
        Self::default()
    }

    /// Only events that belong to `project`.
    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Only events caused by `worker`.
    pub fn worker(mut self, worker: impl Into<String>) -> Self {
        self.worker = Some(worker.into());
        self
    }

    /// Only events whose kind tag is `kind`, as returned by
    /// [`crate::audit::AuditKind::tag`].
    pub fn kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// Only events at or after `from_ms`.
    pub fn from_ms(mut self, from_ms: u64) -> Self {
        self.from_ms = Some(from_ms);
        self
    }

    /// Only events at or before `to_ms`.
    pub fn to_ms(mut self, to_ms: u64) -> Self {
        self.to_ms = Some(to_ms);
        self
    }

    /// Keep only the last `count` matching events.
    pub fn tail(mut self, count: usize) -> Self {
        self.tail = Some(count);
        self
    }

    /// Whether `event` passes every condition of this filter.
    pub fn matches(&self, event: &AuditEvent) -> bool {
        self.project
            .as_deref()
            .is_none_or(|project| event.project.as_deref() == Some(project))
            && self
                .worker
                .as_deref()
                .is_none_or(|worker| event.worker.as_deref() == Some(worker))
            && self
                .kind
                .as_deref()
                .is_none_or(|kind| event.kind.tag() == kind)
            && self.from_ms.is_none_or(|from| event.at_ms >= from)
            && self.to_ms.is_none_or(|to| event.at_ms <= to)
    }

    /// Drops all but the last `tail` events of `events`.
    pub(crate) fn apply_tail(&self, mut events: Vec<AuditEvent>) -> Vec<AuditEvent> {
        if let Some(count) = self.tail
            && events.len() > count
        {
            events.drain(..events.len() - count);
        }
        events
    }
}
