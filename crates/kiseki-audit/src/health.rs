//! Audit log health reporting.

/// Health status of the audit log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditStatus {
    /// Healthy — accepting events.
    Healthy,
    /// Unavailable — quorum lost, cannot serve requests.
    Unavailable,
}

/// Health report from the audit log.
#[derive(Clone, Debug)]
pub struct AuditHealth {
    /// Current status.
    pub status: AuditStatus,
    /// Total event count tracked by the state machine.
    pub event_count: u64,
}
