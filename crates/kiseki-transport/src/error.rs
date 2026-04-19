//! Transport-specific errors.

use kiseki_common::error::{KisekiError, RetriableError, SecurityError};
use kiseki_common::ids::ShardId;

/// Errors from transport operations.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// TLS handshake failed (invalid cert, CA mismatch, expired).
    #[error("TLS handshake failed: {0}")]
    TlsHandshakeFailed(String),

    /// Peer did not present a client certificate (mTLS required).
    #[error("client certificate required but not presented")]
    ClientCertRequired,

    /// Certificate does not chain to the Cluster CA.
    #[error("certificate not trusted: {0}")]
    CertNotTrusted(String),

    /// Connection refused or unreachable.
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    /// I/O error on an established connection.
    #[error("transport I/O error: {0}")]
    IoError(String),

    /// Connection pool exhausted.
    #[error("connection pool exhausted for target {0}")]
    PoolExhausted(String),

    /// Timeout waiting for connection or response.
    #[error("transport timeout: {0}")]
    Timeout(String),

    /// TLS configuration error (bad CA file, missing key).
    #[error("TLS configuration error: {0}")]
    ConfigError(String),
}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        Self::IoError(e.to_string())
    }
}

impl From<TransportError> for KisekiError {
    fn from(e: TransportError) -> Self {
        match e {
            TransportError::ClientCertRequired
            | TransportError::CertNotTrusted(_)
            | TransportError::TlsHandshakeFailed(_) => {
                KisekiError::Security(SecurityError::AuthenticationFailed)
            }
            TransportError::ConnectionFailed(_)
            | TransportError::IoError(_)
            | TransportError::Timeout(_)
            | TransportError::PoolExhausted(_) => {
                // Use a shard-unavailable with a sentinel — the caller
                // should wrap with the real shard context.
                KisekiError::Retriable(RetriableError::ShardUnavailable(ShardId(uuid::Uuid::nil())))
            }
            TransportError::ConfigError(msg) => KisekiError::Permanent(
                kiseki_common::error::PermanentError::InvariantViolation(msg),
            ),
        }
    }
}
