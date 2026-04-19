//! Audit log errors.

/// Errors from audit log operations.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// Audit log is not healthy (quorum lost, initializing).
    #[error("audit log unavailable")]
    Unavailable,
}
