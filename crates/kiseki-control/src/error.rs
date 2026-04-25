//! Control plane errors.

/// Control plane error type.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// Entity already exists.
    #[error("{0} already exists")]
    AlreadyExists(String),

    /// Entity not found.
    #[error("{0} not found")]
    NotFound(String),

    /// Quota validation failed.
    #[error("quota: {0}")]
    QuotaExceeded(String),

    /// Operation rejected (e.g., maintenance mode, policy violation).
    #[error("{0}")]
    Rejected(String),

    /// Permission denied (e.g., cross-tenant access).
    #[error("permission denied: {0}")]
    NotPermitted(String),
}
