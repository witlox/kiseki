//! Client errors.

/// Errors from native client operations.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No seed endpoints reachable (ADR-008).
    #[error("no seeds reachable")]
    NoSeedsReachable,

    /// Tenant KMS unavailable — key cache expired.
    #[error("tenant key unavailable")]
    TenantKeyUnavailable,

    /// Transport error.
    #[error("transport error: {0}")]
    Transport(String),

    /// I/O error (returned as EIO via FUSE).
    #[error("I/O error: {0}")]
    Io(String),
}
