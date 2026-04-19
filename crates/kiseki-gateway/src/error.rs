//! Gateway errors.

use kiseki_common::error::{KisekiError, PermanentError, SecurityError};

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
