//! Gateway errors.

use kiseki_common::error::{KisekiError, PermanentError, SecurityError};
use kiseki_common::ids::ShardId;

/// Errors from gateway operations.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Authentication failed (mTLS or protocol-level auth).
    #[error("gateway authentication failed: {0}")]
    AuthenticationFailed(String),

    /// Operation not supported by this protocol.
    #[error("operation not supported: {0}")]
    OperationNotSupported(String),

    /// Protocol-level error (malformed request).
    #[error("protocol error: {0}")]
    ProtocolError(String),

    /// Upstream error from the view or composition layer.
    #[error("upstream error: {0}")]
    Upstream(String),

    /// View is stale — watermark too far behind (I-K9).
    #[error("view stale: lag {lag_ms}ms exceeds bound")]
    StaleView {
        /// How far behind the view is (milliseconds).
        lag_ms: u64,
    },

    /// Delta's hashed_key is outside the target shard's key range (ADR-033).
    /// Gateway should refresh shard map and retry with the correct shard.
    #[error("key out of range for shard {shard_id:?}")]
    KeyOutOfRange {
        /// The shard that rejected the key.
        shard_id: ShardId,
    },
}

impl From<GatewayError> for KisekiError {
    fn from(e: GatewayError) -> Self {
        match e {
            GatewayError::AuthenticationFailed(_) => {
                KisekiError::Security(SecurityError::AuthenticationFailed)
            }
            _ => KisekiError::Permanent(PermanentError::InvariantViolation(e.to_string())),
        }
    }
}
