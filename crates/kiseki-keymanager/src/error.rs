//! Key manager errors.

use kiseki_common::error::{KisekiError, PermanentError, RetriableError};
use kiseki_common::tenancy::KeyEpoch;

/// Errors from key manager operations.
#[derive(Debug, thiserror::Error)]
pub enum KeyManagerError {
    /// Requested epoch not found in the key store.
    #[error("epoch not found: {0:?}")]
    EpochNotFound(KeyEpoch),

    /// Key generation failed (CSPRNG failure).
    #[error("key generation failed")]
    KeyGenerationFailed,

    /// Key manager is not healthy (quorum lost, initializing).
    #[error("key manager unavailable")]
    Unavailable,

    /// Rotation in progress — only one rotation at a time.
    #[error("rotation already in progress")]
    RotationInProgress,
}

impl From<KeyManagerError> for KisekiError {
    fn from(e: KeyManagerError) -> Self {
        match e {
            KeyManagerError::Unavailable | KeyManagerError::RotationInProgress => {
                KisekiError::Retriable(RetriableError::KeyManagerUnavailable)
            }
            KeyManagerError::EpochNotFound(epoch) => KisekiError::Permanent(
                PermanentError::InvariantViolation(format!("epoch {epoch:?} not found")),
            ),
            KeyManagerError::KeyGenerationFailed => KisekiError::Permanent(
                PermanentError::InvariantViolation("key generation failed".into()),
            ),
        }
    }
}
