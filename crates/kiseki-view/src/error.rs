//! View errors.

use kiseki_common::ids::ViewId;

/// Errors from view operations.
#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    /// View not found.
    #[error("view not found: {0:?}")]
    NotFound(ViewId),

    /// View was discarded, needs rebuild.
    #[error("view discarded: {0:?}")]
    Discarded(ViewId),

    /// MVCC read pin expired.
    #[error("read pin expired")]
    PinExpired,

    /// View staleness exceeds the configured bound.
    #[error("staleness violation on view {0:?}: lag_ms={1}")]
    StalenessViolation(ViewId, u64),
}
