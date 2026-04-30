//! Composition errors.

use kiseki_common::error::{KisekiError, PermanentError};
use kiseki_common::ids::{CompositionId, NamespaceId, ShardId};

/// Errors from composition operations.
#[derive(Debug, thiserror::Error)]
pub enum CompositionError {
    /// Namespace not found.
    #[error("namespace not found: {0:?}")]
    NamespaceNotFound(NamespaceId),

    /// Composition not found.
    #[error("composition not found: {0:?}")]
    CompositionNotFound(CompositionId),

    /// Cross-shard rename — return `EXDEV` (I-L8).
    #[error("cross-shard rename: source {0:?}, target {1:?}")]
    CrossShardRename(ShardId, ShardId),

    /// Namespace is read-only.
    #[error("namespace is read-only: {0:?}")]
    ReadOnlyNamespace(NamespaceId),

    /// Multipart upload not found.
    #[error("multipart upload not found: {0}")]
    MultipartNotFound(String),

    /// Multipart not finalized — parts still pending.
    #[error("multipart not finalized: {0}")]
    MultipartNotFinalized(String),

    /// Version not found.
    #[error("version not found: {0:?} v{1}")]
    VersionNotFound(CompositionId, u64),

    /// Underlying persistent-storage failure (ADR-040). The string
    /// carries the typed kind from `PersistentStoreError` for
    /// metric/log fan-out; opaque to callers above the gateway.
    #[error("composition storage: {0}")]
    Storage(String),
}

impl From<CompositionError> for KisekiError {
    fn from(e: CompositionError) -> Self {
        KisekiError::Permanent(PermanentError::InvariantViolation(e.to_string()))
    }
}
