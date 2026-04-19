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
}
