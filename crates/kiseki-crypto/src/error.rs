//! Crypto-specific errors.
//!
//! All variants map to `KisekiError` categories at crate boundaries.

use kiseki_common::error::{KisekiError, PermanentError, RetriableError, SecurityError};
use kiseki_common::tenancy::TenantScope;

/// Errors from cryptographic operations.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// AEAD decryption failed — authentication tag mismatch.
    #[error("AEAD authentication failed")]
    AuthenticationFailed,

    /// Nonce generation failed (CSPRNG failure).
    #[error("nonce generation failed")]
    NonceGenerationFailed,

    /// HKDF derivation failed.
    #[error("HKDF derivation failed")]
    HkdfFailed,

    /// Tenant KEK wrapping/unwrapping failed.
    #[error("tenant key operation failed")]
    TenantKeyFailed,

    /// Tenant KEK not available (not configured or cache expired).
    #[error("tenant KEK unavailable for scope {0:?}")]
    TenantKekUnavailable(TenantScope),

    /// Invalid envelope structure (wrong nonce length, missing fields).
    #[error("invalid envelope: {0}")]
    InvalidEnvelope(String),

    /// Chunk ID length mismatch.
    #[error("invalid chunk ID length: expected 32, got {0}")]
    InvalidChunkIdLength(usize),

    /// Key material memory protection failed (mlock).
    #[error("memory protection failed: {0}")]
    MemoryProtectionFailed(String),

    /// Compression failed.
    #[cfg(feature = "compression")]
    #[error("compression failed: {0}")]
    CompressionFailed(String),
}

impl From<CryptoError> for KisekiError {
    fn from(e: CryptoError) -> Self {
        match e {
            CryptoError::AuthenticationFailed => KisekiError::Permanent(
                PermanentError::InvariantViolation("AEAD authentication failed".into()),
            ),
            CryptoError::TenantKekUnavailable(scope) => {
                KisekiError::Retriable(RetriableError::TenantKmsUnavailable(scope))
            }
            CryptoError::TenantKeyFailed => {
                KisekiError::Security(SecurityError::AuthenticationFailed)
            }
            _ => KisekiError::Permanent(PermanentError::InvariantViolation(e.to_string())),
        }
    }
}
