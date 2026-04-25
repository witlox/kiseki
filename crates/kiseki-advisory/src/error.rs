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

    /// Scope violation: hint references a composition outside the caller's workload (I-WA3).
    #[error("scope violation")]
    ScopeViolation,

    /// Scope not found — indistinguishable from unauthorized (I-WA6).
    #[error("scope not found")]
    ScopeNotFound,

    /// Child budget exceeds parent ceiling (I-WA7).
    #[error("child exceeds parent ceiling: {0}")]
    ChildExceedsParentCeiling(String),

    /// Retention policy conflict — hint cannot bypass a retention hold (I-WA14).
    #[error("retention policy conflict")]
    RetentionPolicyConflict,

    /// Priority not allowed — hint attempts to exceed policy-allowed max (I-WA14).
    #[error("priority not allowed")]
    PriorityNotAllowed,

    /// Prefetch budget exceeded — hint exceeds `declared_prefetch_bytes` (I-WA16).
    #[error("prefetch budget exceeded")]
    PrefetchBudgetExceeded,

    /// Forbidden target field in hint (I-WA11).
    #[error("forbidden target field: {0}")]
    ForbiddenTargetField(String),

    /// Profile revoked mid-workflow (I-WA18).
    #[error("profile revoked")]
    ProfileRevoked,

    /// Priority revoked mid-workflow (I-WA18).
    #[error("priority revoked")]
    PriorityRevoked,

    /// TTL expired.
    #[error("ttl expired")]
    TtlExpired,

    /// Workflow unknown (already ended or never existed).
    #[error("workflow unknown")]
    WorkflowUnknown,
}
