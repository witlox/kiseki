//! AES-256-GCM authenticated encryption (I-K7).
//!
//! Uses `aws-lc-rs` for FIPS-validated AEAD. Nonces are 96-bit,
//! generated from a CSPRNG. The caller must ensure nonce uniqueness
//! per key — in practice, each (`master_key`, `chunk_id`) pair yields a
//! unique derived DEK, so a random nonce is safe (collision probability
//! negligible at chunk-level granularity).

use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use zeroize::Zeroizing;

use crate::error::CryptoError;

/// Nonce length for AES-256-GCM (96 bits / 12 bytes).
pub const GCM_NONCE_LEN: usize = NONCE_LEN;

/// Authentication tag length for AES-256-GCM (128 bits / 16 bytes).
pub const GCM_TAG_LEN: usize = 16;

/// AEAD wrapper around `aws-lc-rs` AES-256-GCM.
pub struct Aead {
    _private: (),
}

impl Aead {
    /// Create a new AEAD instance.
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Encrypt `plaintext` with `key`, returning `(ciphertext_with_tag, nonce)`.
    ///
    /// The ciphertext includes the 16-byte authentication tag appended.
    /// The `aad` (additional authenticated data) is authenticated but
    /// not encrypted — use it for the chunk ID and envelope metadata.
    pub fn seal(
        &self,
        key: &Zeroizing<[u8; 32]>,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<(Vec<u8>, [u8; GCM_NONCE_LEN]), CryptoError> {
        let unbound =
            UnboundKey::new(&AES_256_GCM, key.as_ref()).map_err(|_| CryptoError::HkdfFailed)?;
        let sealing_key = LessSafeKey::new(unbound);

        // Generate random nonce via aws-lc-rs CSPRNG.
        let mut nonce_bytes = [0u8; GCM_NONCE_LEN];
        aws_lc_rs::rand::fill(&mut nonce_bytes).map_err(|_| CryptoError::NonceGenerationFailed)?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        // Encrypt in-place: plaintext + space for tag.
        let mut in_out = plaintext.to_vec();
        sealing_key
            .seal_in_place_append_tag(nonce, Aad::from(aad), &mut in_out)
            .map_err(|_| CryptoError::AuthenticationFailed)?;

        Ok((in_out, nonce_bytes))
    }

    /// Decrypt `ciphertext_with_tag` using `key` and `nonce`.
    ///
    /// Returns the plaintext. Fails if the authentication tag does not
    /// verify (tampered ciphertext, wrong key, wrong AAD).
    pub fn open(
        &self,
        key: &Zeroizing<[u8; 32]>,
        nonce_bytes: &[u8; GCM_NONCE_LEN],
        ciphertext_with_tag: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let unbound =
            UnboundKey::new(&AES_256_GCM, key.as_ref()).map_err(|_| CryptoError::HkdfFailed)?;
        let opening_key = LessSafeKey::new(unbound);

        let nonce = Nonce::assume_unique_for_key(*nonce_bytes);

        let mut in_out = ciphertext_with_tag.to_vec();
        let plaintext = opening_key
            .open_in_place(nonce, Aad::from(aad), &mut in_out)
            .map_err(|_| CryptoError::AuthenticationFailed)?;

        Ok(plaintext.to_vec())
    }
}

impl Default for Aead {
    fn default() -> Self {
        Self::new()
    }
}
