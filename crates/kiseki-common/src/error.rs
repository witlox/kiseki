//! Top-level error taxonomy.
//!
//! Every error in Kiseki belongs to exactly one of three categories —
//! `Retriable`, `Permanent`, `Security`. Callers use the category to
//! decide whether to back off and retry, surface to the operator, or
//! deny and audit. Per-context crates extend these with their own typed
//! variants and convert into [`KisekiError`] at crate boundaries via
//! `From`.
//!
//! Spec: `specs/architecture/error-taxonomy.md` §"Error categories".

use crate::ids::{ChunkId, ShardId};
use crate::tenancy::TenantScope;

/// Top-level typed error. Wraps one of the three categories.
#[derive(Debug, thiserror::Error)]
pub enum KisekiError {
    /// Transient failure; caller should back off and retry.
    #[error(transparent)]
    Retriable(#[from] RetriableError),
    /// Cannot succeed; report to operator or user. No amount of retry
    /// will change the outcome without operator intervention.
    #[error(transparent)]
    Permanent(#[from] PermanentError),
    /// Authentication, authorization, or tenant-boundary violation.
    /// Always audited, never retried by the caller.
    #[error(transparent)]
    Security(#[from] SecurityError),
}

impl KisekiError {
    /// Category discriminant for logging, metrics, and gRPC status mapping.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        match self {
            Self::Retriable(_) => ErrorCategory::Retriable,
            Self::Permanent(_) => ErrorCategory::Permanent,
            Self::Security(_) => ErrorCategory::Security,
        }
    }

    /// Whether a caller should retry. Equivalent to
    /// `matches!(self.category(), ErrorCategory::Retriable)`.
    #[must_use]
    pub const fn is_retriable(&self) -> bool {
        matches!(self, Self::Retriable(_))
    }
}

/// Error category discriminant, used for metrics and wire mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ErrorCategory {
    /// Retry with backoff.
    Retriable,
    /// Report to user/admin.
    Permanent,
    /// Deny and audit.
    Security,
}

/// Transient failures — retry with backoff.
#[derive(Debug, thiserror::Error)]
pub enum RetriableError {
    /// Raft quorum lost on a shard or leader election in progress.
    /// Spec: F-C1, F-C2, error-taxonomy §kiseki-log.
    #[error("shard unavailable: {0:?}")]
    ShardUnavailable(ShardId),

    /// System key manager unavailable; blocks chunk writes cluster-wide
    /// until the keyserver cluster is healthy (I-K12).
    #[error("system key manager unavailable")]
    KeyManagerUnavailable,

    /// Tenant KMS unreachable (external KMS / network partition).
    /// Spec: F-K1, error-taxonomy §kiseki-keymanager.
    #[error("tenant KMS unreachable: {0:?}")]
    TenantKmsUnavailable(TenantScope),

    /// Raft quorum explicitly lost for a shard. Distinct from
    /// `ShardUnavailable` in that it is observed by the Raft layer
    /// rather than by an upper-layer timeout.
    #[error("quorum lost: {0:?}")]
    QuorumLost(ShardId),

    /// Shard is in read-only maintenance mode; writes buffered or rejected.
    /// Spec: I-O6.
    #[error("maintenance mode: {0:?}")]
    MaintenanceMode(ShardId),

    /// Tenant quota exceeded at the named scope; caller should back off
    /// and retry within its quota window, or escalate to tenant admin.
    /// Spec: I-T2.
    #[error("quota exceeded: {0:?}")]
    QuotaExceeded(TenantScope),
}

/// Permanent failures — operator or user-visible, no retry.
#[derive(Debug, thiserror::Error)]
pub enum PermanentError {
    /// EC repair exhausted parity — data is genuinely lost. Spec: F-D5.
    #[error("chunk lost: {0}")]
    ChunkLost(ChunkId),

    /// Tenant KMS permanently unavailable — crypto-shred without
    /// recovery path. Spec: F-K2, I-K11.
    #[error("tenant KMS lost for scope {0:?}")]
    TenantKmsLost(TenantScope),

    /// Data corruption detected on a shard; e.g., `SSTable` CRC mismatch.
    /// Spec: F-C3, F-D5.
    #[error("data corruption: {0:?}")]
    DataCorruption(ShardId),

    /// An internal invariant was violated. This is a bug; the error
    /// carries a free-form description of which invariant was hit.
    /// Spec: project-wide `specs/invariants.md`.
    #[error("invariant violation: {0}")]
    InvariantViolation(String),
}

/// Security failures — always audited, never retried by the caller.
#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    /// mTLS handshake failed or certificate invalid. Spec: I-Auth1.
    #[error("authentication failed")]
    AuthenticationFailed,

    /// Request credentials don't match the expected tenant scope.
    /// Spec: I-T1.
    #[error("tenant access denied for scope {0:?}")]
    TenantAccessDenied(TenantScope),

    /// Cluster admin attempted to access tenant config/data without
    /// explicit tenant-admin approval. Spec: I-T4.
    #[error("cluster admin access denied")]
    ClusterAdminAccessDenied,

    /// The tenant has been crypto-shredded; all data under this scope is
    /// unreadable. Spec: I-K5.
    #[error("crypto-shred complete for scope {0:?}")]
    CryptoShredComplete(TenantScope),
}
