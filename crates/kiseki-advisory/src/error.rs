//! Advisory errors.

/// Errors from advisory operations.
#[derive(Debug, thiserror::Error)]
pub enum AdvisoryError {
    /// Workflow not found.
    #[error("workflow not found")]
    WorkflowNotFound,

    /// Budget exceeded for this workload.
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),

    /// Profile not allowed for this scope (I-WA7).
    #[error("profile not allowed: {0}")]
    ProfileNotAllowed(String),

    /// Phase advance is not monotonic (I-WA13).
    #[error("phase not monotonic: current {current}, requested {requested}")]
    PhaseNotMonotonic {
        /// Current phase value.
        current: u64,
        /// Requested (non-monotonic) phase value.
        requested: u64,
    },

    /// Advisory is disabled for this scope (I-WA12).
    #[error("advisory disabled")]
    AdvisoryDisabled,
}
