//! Internal KMS provider — baseline implementation of [`TenantKmsProvider`].
//!
//! Uses AES-256-GCM (via `aws-lc-rs`) for wrap/unwrap with AAD binding.
//! This is the default provider; Vault and AWS KMS are future
//! feature-gated additions.

use std::sync::Mutex;

use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};

use crate::provider::{KmsEpochId, KmsError, KmsHealth, TenantKmsProvider};

/// Nonce length for AES-256-GCM (96 bits).
const GCM_NONCE_LEN: usize = NONCE_LEN;

/// Internal KMS provider backed by an in-memory AES-256-GCM key.
///
/// Suitable for development, testing, and single-tenant deployments
/// where an external KMS is not required.
pub struct InternalProvider {
    inner: Mutex<InternalState>,
}

struct InternalState {
    key: Vec<u8>,
    epoch: u64,
}

impl InternalProvider {
    /// Create a new internal provider with the given 32-byte key.
    ///
    /// # Panics
    ///
    /// Panics if `key` is not exactly 32 bytes.
    #[must_use]
    pub fn new(key: Vec<u8>) -> Self {
        assert!(key.len() == 32, "key must be exactly 32 bytes");
        Self {
            inner: Mutex::new(InternalState { key, epoch: 1 }),
        }
    }

    /// Build an `LessSafeKey` from the current key material.
    fn make_aead_key(key_bytes: &[u8]) -> Result<LessSafeKey, KmsError> {
        let unbound = UnboundKey::new(&AES_256_GCM, key_bytes)
            .map_err(|_| KmsError::CryptoError("invalid key material".into()))?;
        Ok(LessSafeKey::new(unbound))
    }
}

impl core::fmt::Debug for InternalProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InternalProvider").finish_non_exhaustive()
    }
}

impl TenantKmsProvider for InternalProvider {
    fn wrap(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        let state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let sealing_key = Self::make_aead_key(&state.key)?;

        // Generate random nonce.
        let mut nonce_bytes = [0u8; GCM_NONCE_LEN];
        aws_lc_rs::rand::fill(&mut nonce_bytes)
            .map_err(|_| KmsError::CryptoError("nonce generation failed".into()))?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        // Encrypt in-place with appended tag.
        let mut in_out = plaintext.to_vec();
        sealing_key
            .seal_in_place_append_tag(nonce, Aad::from(aad), &mut in_out)
            .map_err(|_| KmsError::CryptoError("seal failed".into()))?;

        // Prepend nonce to ciphertext+tag.
        let mut result = Vec::with_capacity(GCM_NONCE_LEN + in_out.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&in_out);
        Ok(result)
    }

    fn unwrap(&self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        if ciphertext.len() < GCM_NONCE_LEN + 16 {
            return Err(KmsError::CryptoError("ciphertext too short".into()));
        }

        let state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let (nonce_bytes, ct_with_tag) = ciphertext.split_at(GCM_NONCE_LEN);
        let mut nonce_arr = [0u8; GCM_NONCE_LEN];
        nonce_arr.copy_from_slice(nonce_bytes);
        let nonce = Nonce::assume_unique_for_key(nonce_arr);

        let opening_key = Self::make_aead_key(&state.key)?;

        let mut in_out = ct_with_tag.to_vec();
        let plaintext = opening_key
            .open_in_place(nonce, Aad::from(aad), &mut in_out)
            .map_err(|_| KmsError::AadMismatch)?;

        Ok(plaintext.to_vec())
    }

    fn rotate(&self) -> Result<KmsEpochId, KmsError> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut new_key = vec![0u8; 32];
        aws_lc_rs::rand::fill(&mut new_key)
            .map_err(|_| KmsError::CryptoError("key generation failed".into()))?;

        state.epoch += 1;
        let epoch_id = format!("internal-epoch-{}", state.epoch);
        state.key = new_key;

        Ok(epoch_id)
    }

    fn health_check(&self) -> KmsHealth {
        KmsHealth::Healthy
    }

    fn name(&self) -> &'static str {
        "internal"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> Vec<u8> {
        vec![0xAB; 32]
    }

    #[test]
    fn wrap_unwrap_roundtrip() {
        let provider = InternalProvider::new(test_key());
        let plaintext = b"secret DEK parameter";
        let aad = b"tenant-123:chunk-456";

        let wrapped = provider.wrap(plaintext, aad).unwrap();
        let unwrapped = provider.unwrap(&wrapped, aad).unwrap();

        assert_eq!(unwrapped, plaintext);
    }

    #[test]
    fn aad_mismatch_fails() {
        let provider = InternalProvider::new(test_key());
        let plaintext = b"secret DEK parameter";
        let aad = b"tenant-123:chunk-456";

        let wrapped = provider.wrap(plaintext, aad).unwrap();
        let result = provider.unwrap(&wrapped, b"wrong-aad");

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), KmsError::AadMismatch));
    }

    #[test]
    fn rotate_returns_new_epoch() {
        let provider = InternalProvider::new(test_key());

        let epoch1 = provider.rotate().unwrap();
        let epoch2 = provider.rotate().unwrap();

        assert_ne!(epoch1, epoch2);
        assert!(epoch1.starts_with("internal-epoch-"));
        assert!(epoch2.starts_with("internal-epoch-"));
    }

    #[test]
    fn health_is_healthy() {
        let provider = InternalProvider::new(test_key());
        assert_eq!(provider.health_check(), KmsHealth::Healthy);
    }

    #[test]
    fn name_is_internal() {
        let provider = InternalProvider::new(test_key());
        assert_eq!(provider.name(), "internal");
    }

    #[test]
    fn wrap_unwrap_after_rotate_uses_new_key() {
        let provider = InternalProvider::new(test_key());
        let plaintext = b"pre-rotation data";
        let aad = b"aad";

        let wrapped_before = provider.wrap(plaintext, aad).unwrap();
        provider.rotate().unwrap();

        // Old wrapped data should NOT unwrap with the new key.
        let result = provider.unwrap(&wrapped_before, aad);
        assert!(result.is_err());

        // New wrap/unwrap should work.
        let wrapped_after = provider.wrap(plaintext, aad).unwrap();
        let unwrapped = provider.unwrap(&wrapped_after, aad).unwrap();
        assert_eq!(unwrapped, plaintext);
    }

    #[test]
    fn empty_plaintext_roundtrip() {
        let provider = InternalProvider::new(test_key());
        let wrapped = provider.wrap(b"", b"aad").unwrap();
        let unwrapped = provider.unwrap(&wrapped, b"aad").unwrap();
        assert!(unwrapped.is_empty());
    }
}
