//! Chunk ID derivation (I-K10, I-X2).
//!
//! - `CrossTenant`: `sha256(plaintext)` — cross-tenant dedup enabled.
//! - `TenantIsolated`: `HMAC-SHA256(plaintext, tenant_hmac_key)` — no
//!   cross-tenant dedup, zero co-occurrence leak.

use aws_lc_rs::digest;
use aws_lc_rs::hmac;
use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::DedupPolicy;

use crate::error::CryptoError;

/// Derive a chunk ID from plaintext according to the tenant's dedup policy.
///
/// For `CrossTenant`: `ChunkId = SHA-256(plaintext)`.
/// For `TenantIsolated`: `ChunkId = HMAC-SHA256(tenant_hmac_key, plaintext)`.
///
/// The `tenant_hmac_key` must be provided when `policy` is
/// `TenantIsolated`; it is ignored for `CrossTenant`.
#[tracing::instrument(skip(plaintext, tenant_hmac_key), fields(plaintext_len = plaintext.len(), policy = ?policy))]
pub fn derive_chunk_id(
    plaintext: &[u8],
    policy: DedupPolicy,
    tenant_hmac_key: Option<&[u8]>,
) -> Result<ChunkId, CryptoError> {
    match policy {
        DedupPolicy::CrossTenant => {
            let hash = digest::digest(&digest::SHA256, plaintext);
            let mut id = [0u8; 32];
            id.copy_from_slice(hash.as_ref());
            Ok(ChunkId(id))
        }
        DedupPolicy::TenantIsolated => {
            let key_bytes = tenant_hmac_key.ok_or_else(|| {
                tracing::warn!(
                    "derive_chunk_id: TenantIsolated dedup but no tenant HMAC key supplied",
                );
                CryptoError::InvalidEnvelope(
                    "tenant HMAC key required for TenantIsolated dedup".into(),
                )
            })?;
            let key = hmac::Key::new(hmac::HMAC_SHA256, key_bytes);
            let tag = hmac::sign(&key, plaintext);
            let mut id = [0u8; 32];
            id.copy_from_slice(tag.as_ref());
            Ok(ChunkId(id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_tenant_deterministic() {
        let data = b"hello world";
        let id1 = derive_chunk_id(data, DedupPolicy::CrossTenant, None);
        let id2 = derive_chunk_id(data, DedupPolicy::CrossTenant, None);
        assert!(id1.is_ok());
        assert_eq!(
            id1.unwrap_or_else(|_| unreachable!()),
            id2.unwrap_or_else(|_| unreachable!())
        );
    }

    #[test]
    fn cross_tenant_same_data_same_id() {
        let data = b"dedup me";
        let id1 = derive_chunk_id(data, DedupPolicy::CrossTenant, None);
        let id2 = derive_chunk_id(data, DedupPolicy::CrossTenant, None);
        assert_eq!(
            id1.unwrap_or_else(|_| unreachable!()),
            id2.unwrap_or_else(|_| unreachable!())
        );
    }

    #[test]
    fn tenant_isolated_different_keys_different_ids() {
        let data = b"same data";
        let key_a = b"tenant-a-hmac-key";
        let key_b = b"tenant-b-hmac-key";
        let id_a = derive_chunk_id(data, DedupPolicy::TenantIsolated, Some(key_a));
        let id_b = derive_chunk_id(data, DedupPolicy::TenantIsolated, Some(key_b));
        assert_ne!(
            id_a.unwrap_or_else(|_| unreachable!()),
            id_b.unwrap_or_else(|_| unreachable!())
        );
    }

    #[test]
    fn tenant_isolated_requires_key() {
        let data = b"test";
        let result = derive_chunk_id(data, DedupPolicy::TenantIsolated, None);
        assert!(result.is_err());
    }

    #[test]
    fn different_data_different_ids() {
        let id1 = derive_chunk_id(b"aaa", DedupPolicy::CrossTenant, None);
        let id2 = derive_chunk_id(b"bbb", DedupPolicy::CrossTenant, None);
        assert_ne!(
            id1.unwrap_or_else(|_| unreachable!()),
            id2.unwrap_or_else(|_| unreachable!())
        );
    }

    // ---------------------------------------------------------------
    // Scenario: Write a chunk with HMAC ID (opted-out tenant)
    // HMAC chunk ID is unique per tenant: same plaintext from two
    // tenants with different keys produces different IDs (I-K10).
    // Cross-tenant dedup cannot match.
    // ---------------------------------------------------------------
    #[test]
    fn hmac_chunk_id_unique_per_tenant_no_cross_dedup() {
        let plaintext = b"identical payload";
        let key_defense = b"org-defense-tenant-key-material!";
        let key_pharma = b"org-pharma-tenant-key-material!!";

        // Both tenants produce chunk IDs from the same plaintext.
        let id_defense =
            derive_chunk_id(plaintext, DedupPolicy::TenantIsolated, Some(key_defense)).unwrap();
        let id_pharma =
            derive_chunk_id(plaintext, DedupPolicy::TenantIsolated, Some(key_pharma)).unwrap();

        // Same plaintext, different tenant keys → different chunk IDs.
        assert_ne!(
            id_defense, id_pharma,
            "HMAC IDs must differ across tenants (cross-tenant dedup blocked)"
        );

        // Same tenant, same plaintext → deterministic.
        let id_defense_2 =
            derive_chunk_id(plaintext, DedupPolicy::TenantIsolated, Some(key_defense)).unwrap();
        assert_eq!(
            id_defense, id_defense_2,
            "HMAC ID must be deterministic within a tenant"
        );

        // Cross-tenant dedup (SHA-256) would match — HMAC does not.
        let id_cross = derive_chunk_id(plaintext, DedupPolicy::CrossTenant, None).unwrap();
        assert_ne!(id_cross, id_defense, "HMAC ID must differ from SHA-256 ID");
    }
}
