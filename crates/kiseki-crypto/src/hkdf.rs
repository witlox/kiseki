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

/// Helper type for HKDF output length.
struct HkdfLen;

impl aws_lc_rs::hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        32
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
}
