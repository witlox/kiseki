//! HKDF-SHA256 system DEK derivation (ADR-003).
//!
//! ```text
//! system_dek = HKDF-SHA256(
//!     key  = system_master_key[epoch],
//!     salt = chunk_id,
//!     info = "kiseki-chunk-dek-v1"
//! )
//! ```
//!
//! Derivation is **local** on storage nodes — the key manager never
//! sees individual chunk IDs (ADV-ARCH-01 fix).

use aws_lc_rs::hkdf::{Salt, HKDF_SHA256};
use kiseki_common::ids::ChunkId;
use zeroize::Zeroizing;

use crate::error::CryptoError;
use crate::keys::SystemMasterKey;

/// HKDF info string — versioned for crypto-agility.
const HKDF_INFO: &[u8] = b"kiseki-chunk-dek-v1";

/// HKDF info string for the convergent GCM nonce (ADR-044). Distinct
/// label from `HKDF_INFO` so the nonce and DEK are domain-separated
/// even though both derive from `(master, chunk_id)`.
const HKDF_NONCE_INFO: &[u8] = b"kiseki-chunk-nonce-v1";

/// Derive a per-chunk system DEK from the master key and chunk ID.
///
/// Deterministic: same `(master_key, chunk_id)` always yields the same
/// DEK. This is the core property that eliminates per-chunk key storage.
pub fn derive_system_dek(
    master: &SystemMasterKey,
    chunk_id: &ChunkId,
) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let salt = Salt::new(HKDF_SHA256, &chunk_id.0);
    let prk = salt.extract(master.material());

    let mut dek = Zeroizing::new([0u8; 32]);
    prk.expand(&[HKDF_INFO], HkdfLen)
        .and_then(|okm| okm.fill(&mut *dek))
        .map_err(|_| CryptoError::HkdfFailed)?;

    Ok(dek)
}

/// Derive the **convergent** GCM nonce for a chunk (ADR-044).
///
/// Deterministic in `(master, chunk_id)`, like [`derive_system_dek`], but
/// with a distinct info label. Because `chunk_id` is a content hash,
/// identical content yields the identical nonce — making AEAD sealing
/// convergent (identical content ⇒ identical ciphertext), which is the
/// property content-addressed dedup requires. GCM-safe: the DEK is
/// already unique per `chunk_id`, so each (key, nonce) pair encrypts
/// exactly one plaintext (see ADR-044 "Why this is GCM-safe").
pub fn derive_convergent_nonce(
    master: &SystemMasterKey,
    chunk_id: &ChunkId,
) -> Result<[u8; 12], CryptoError> {
    let salt = Salt::new(HKDF_SHA256, &chunk_id.0);
    let prk = salt.extract(master.material());

    let mut nonce = [0u8; 12];
    prk.expand(&[HKDF_NONCE_INFO], NonceLen)
        .and_then(|okm| okm.fill(&mut nonce))
        .map_err(|_| CryptoError::HkdfFailed)?;

    Ok(nonce)
}

/// Helper type for HKDF output length.
struct HkdfLen;

impl aws_lc_rs::hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        32
    }
}

/// Helper type for the 12-byte GCM nonce HKDF output.
struct NonceLen;

impl aws_lc_rs::hkdf::KeyType for NonceLen {
    fn len(&self) -> usize {
        12
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::tenancy::KeyEpoch;

    #[test]
    fn deterministic_derivation() {
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let chunk_id = ChunkId([0xab; 32]);

        let dek1 = derive_system_dek(&master, &chunk_id);
        let dek2 = derive_system_dek(&master, &chunk_id);

        assert!(dek1.is_ok());
        assert!(dek2.is_ok());
        assert_eq!(
            *dek1.unwrap_or_else(|_| unreachable!()),
            *dek2.unwrap_or_else(|_| unreachable!())
        );
    }

    #[test]
    fn different_chunk_ids_yield_different_deks() {
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let id_a = ChunkId([0xaa; 32]);
        let id_b = ChunkId([0xbb; 32]);

        let dek_a = derive_system_dek(&master, &id_a).unwrap_or_else(|_| unreachable!());
        let dek_b = derive_system_dek(&master, &id_b).unwrap_or_else(|_| unreachable!());

        assert_ne!(*dek_a, *dek_b);
    }

    #[test]
    fn different_master_keys_yield_different_deks() {
        let master_a = SystemMasterKey::new([0x01; 32], KeyEpoch(1));
        let master_b = SystemMasterKey::new([0x02; 32], KeyEpoch(2));
        let chunk_id = ChunkId([0xcc; 32]);

        let dek_a = derive_system_dek(&master_a, &chunk_id).unwrap_or_else(|_| unreachable!());
        let dek_b = derive_system_dek(&master_b, &chunk_id).unwrap_or_else(|_| unreachable!());

        assert_ne!(*dek_a, *dek_b);
    }

    // ADR-044 convergent nonce.

    #[test]
    fn convergent_nonce_is_deterministic() {
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let chunk_id = ChunkId([0xab; 32]);
        let n1 = derive_convergent_nonce(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
        let n2 = derive_convergent_nonce(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(n1, n2, "same (master, chunk_id) must yield the same nonce");
    }

    #[test]
    fn convergent_nonce_differs_per_chunk_id() {
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let na = derive_convergent_nonce(&master, &ChunkId([0xaa; 32]))
            .unwrap_or_else(|_| unreachable!());
        let nb = derive_convergent_nonce(&master, &ChunkId([0xbb; 32]))
            .unwrap_or_else(|_| unreachable!());
        assert_ne!(na, nb, "distinct chunk_ids must yield distinct nonces");
    }

    #[test]
    fn convergent_nonce_is_domain_separated_from_dek() {
        // The nonce derivation must not leak the DEK: deriving a nonce
        // and a DEK from the same (master, chunk_id) uses distinct info
        // labels, so the 12-byte nonce must not be a prefix of the DEK.
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let chunk_id = ChunkId([0xcd; 32]);
        let nonce = derive_convergent_nonce(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
        let dek = derive_system_dek(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
        assert_ne!(
            &nonce[..],
            &dek[..12],
            "nonce must be domain-separated from the DEK"
        );
    }
}
