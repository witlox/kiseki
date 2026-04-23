//! External KMS provider abstraction (ADR-028, I-K16).
//!
//! Callers never branch on provider type. Every provider handles
//! wrap, unwrap, rotate, and health checks identically.

use std::fmt::Debug;

/// Opaque provider-specific key version.
pub type KmsEpochId = String;

/// KMS health status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KmsHealth {
    /// Provider is fully operational.
    Healthy,
    /// Provider is operational but degraded (e.g., high latency).
    Degraded(String),
    /// Provider cannot service requests.
    Unavailable(String),
}

/// Provider abstraction for tenant key management (I-K16).
///
/// Callers never branch on provider type. Every provider handles
/// wrap, unwrap, rotate, and health checks identically.
pub trait TenantKmsProvider: Send + Sync + Debug {
    /// Wrap (encrypt) a DEK derivation parameter with AAD binding (I-K17).
    fn wrap(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError>;

    /// Unwrap (decrypt) a previously wrapped blob with AAD verification.
    fn unwrap(&self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError>;

    /// Rotate the tenant's key, returning the new epoch ID.
    fn rotate(&self) -> Result<KmsEpochId, KmsError>;

    /// Health check -- connectivity + basic operation test.
    fn health_check(&self) -> KmsHealth;

    /// Provider name for logging.
    fn name(&self) -> &'static str;
}

/// Errors from KMS provider operations.
#[derive(Debug, thiserror::Error)]
pub enum KmsError {
    /// KMS backend is unreachable.
    #[error("KMS unavailable: {0}")]
    Unavailable(String),

    /// Authentication to the KMS backend failed.
    #[error("KMS authentication failed: {0}")]
    AuthFailed(String),

    /// Wrap or unwrap cryptographic operation failed.
    #[error("wrap/unwrap failed: {0}")]
    CryptoError(String),

    /// AAD provided at unwrap does not match the AAD used at wrap time.
    #[error("AAD mismatch")]
    AadMismatch,

    /// The requested key ID does not exist in the provider.
    #[error("key not found: {0}")]
    KeyNotFound(String),

    /// The KEK has been destroyed and cannot be used.
    #[error("KEK destroyed")]
    KekDestroyed,
}
