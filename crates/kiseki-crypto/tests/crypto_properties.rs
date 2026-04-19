//! Property tests for cryptographic invariants.
//!
//! Verifies:
//! 1. Encrypt/decrypt round-trip for arbitrary plaintext (I-K7).
//! 2. HKDF derivation is deterministic (ADR-003).
//! 3. Different inputs yield different outputs (no collisions in practice).
//! 4. Tampered ciphertext always fails authentication.
//! 5. Tenant wrap/unwrap round-trip for arbitrary plaintext.
//! 6. Cross-tenant chunk IDs differ for tenant-isolated policy (I-K10).

use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::{DedupPolicy, KeyEpoch};
use kiseki_crypto::aead::Aead;
use kiseki_crypto::chunk_id::derive_chunk_id;
use kiseki_crypto::envelope::{open_envelope, seal_envelope, unwrap_tenant, wrap_for_tenant};
use kiseki_crypto::hkdf::derive_system_dek;
use kiseki_crypto::keys::{MasterKeyCache, SystemMasterKey, TenantKek};

use proptest::prelude::*;

fn master_key_strategy() -> impl Strategy<Value = [u8; 32]> {
    proptest::collection::vec(any::<u8>(), 32).prop_map(|v| {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&v);
        arr
    })
}

fn chunk_id_strategy() -> impl Strategy<Value = ChunkId> {
    proptest::collection::vec(any::<u8>(), 32).prop_map(|v| {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&v);
        ChunkId(arr)
    })
}

proptest! {
    /// Encrypt → decrypt always recovers the original plaintext.
    #[test]
    fn seal_open_roundtrip(
        key_bytes in master_key_strategy(),
        chunk_id in chunk_id_strategy(),
        plaintext in proptest::collection::vec(any::<u8>(), 0..4096),
    ) {
        let aead = Aead::new();
        let master = SystemMasterKey::new(key_bytes, KeyEpoch(1));
        let envelope = seal_envelope(&aead, &master, &chunk_id, &plaintext)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let recovered = open_envelope(&aead, &master, &envelope)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_eq!(recovered, plaintext);
    }

    /// HKDF is deterministic: same inputs → same output.
    #[test]
    fn hkdf_deterministic(
        key_bytes in master_key_strategy(),
        chunk_id in chunk_id_strategy(),
    ) {
        let master = SystemMasterKey::new(key_bytes, KeyEpoch(1));
        let dek1 = derive_system_dek(&master, &chunk_id)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let dek2 = derive_system_dek(&master, &chunk_id)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_eq!(*dek1, *dek2);
    }

    /// Different chunk IDs yield different DEKs (with overwhelming probability).
    #[test]
    fn hkdf_different_inputs_different_outputs(
        key_bytes in master_key_strategy(),
        id_a in chunk_id_strategy(),
        id_b in chunk_id_strategy(),
    ) {
        prop_assume!(id_a != id_b);
        let master = SystemMasterKey::new(key_bytes, KeyEpoch(1));
        let dek_a = derive_system_dek(&master, &id_a)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let dek_b = derive_system_dek(&master, &id_b)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_ne!(*dek_a, *dek_b);
    }

    /// Flipping any ciphertext bit causes authentication failure.
    #[test]
    fn tampered_ciphertext_rejected(
        key_bytes in master_key_strategy(),
        chunk_id in chunk_id_strategy(),
        plaintext in proptest::collection::vec(any::<u8>(), 1..1024),
        flip_pos in any::<usize>(),
    ) {
        let aead = Aead::new();
        let master = SystemMasterKey::new(key_bytes, KeyEpoch(1));
        let mut envelope = seal_envelope(&aead, &master, &chunk_id, &plaintext)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let total_len = envelope.ciphertext.len();
        if total_len > 0 {
            let pos = flip_pos % total_len;
            envelope.ciphertext[pos] ^= 0x01;
            let result = open_envelope(&aead, &master, &envelope);
            prop_assert!(result.is_err(), "tampered ciphertext must fail");
        }
    }

    /// Tenant wrap → unwrap round-trip recovers plaintext.
    #[test]
    fn tenant_wrap_unwrap_roundtrip(
        master_bytes in master_key_strategy(),
        tenant_bytes in master_key_strategy(),
        chunk_id in chunk_id_strategy(),
        plaintext in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let aead = Aead::new();
        let master = SystemMasterKey::new(master_bytes, KeyEpoch(1));
        let tenant_kek = TenantKek::new(tenant_bytes, KeyEpoch(1));

        let mut envelope = seal_envelope(&aead, &master, &chunk_id, &plaintext)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        wrap_for_tenant(&aead, &mut envelope, &tenant_kek)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;

        let mut cache = MasterKeyCache::new();
        cache.insert(SystemMasterKey::new(master_bytes, KeyEpoch(1)));

        let recovered = unwrap_tenant(&aead, &envelope, &tenant_kek, &cache)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_eq!(recovered, plaintext);
    }

    /// Cross-tenant dedup: same data → same chunk ID.
    #[test]
    fn cross_tenant_same_id(
        plaintext in proptest::collection::vec(any::<u8>(), 1..1024),
    ) {
        let id1 = derive_chunk_id(&plaintext, DedupPolicy::CrossTenant, None)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let id2 = derive_chunk_id(&plaintext, DedupPolicy::CrossTenant, None)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_eq!(id1, id2);
    }

    /// Tenant-isolated: different HMAC keys → different chunk IDs for same data.
    #[test]
    fn tenant_isolated_different_keys_different_ids(
        plaintext in proptest::collection::vec(any::<u8>(), 1..1024),
        key_a in proptest::collection::vec(any::<u8>(), 32..33),
        key_b in proptest::collection::vec(any::<u8>(), 32..33),
    ) {
        prop_assume!(key_a != key_b);
        let id_a = derive_chunk_id(&plaintext, DedupPolicy::TenantIsolated, Some(&key_a))
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        let id_b = derive_chunk_id(&plaintext, DedupPolicy::TenantIsolated, Some(&key_b))
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_ne!(id_a, id_b);
    }
}
