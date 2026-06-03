//! Chunk ID derivation (I-K10, I-X2).
//!
//! - `CrossTenant`: `H(plaintext)` where `H` is selected by
//!   [`chunk_hash_algorithm()`] (BLAKE3 by default; SHA-256 when
//!   `KISEKI_CHUNK_HASH=sha256` is set). Cross-tenant dedup enabled.
//! - `TenantIsolated`: a keyed hash over `plaintext` with the
//!   tenant-specific 32-byte key — no cross-tenant dedup, zero
//!   co-occurrence leak.
//!
//! ## Algorithm choice
//!
//! ADR-044 rev-2 (2026-06-03) sets BLAKE3 as the default content-
//! addressed dedup hash. The single-thread cost of SHA-256 was 21 %
//! of single-thread 4 KiB PUT time on the in-process-persistent
//! flamegraph (2026-06-03 dev box); BLAKE3 with its AVX2 + SSE4.1
//! vectorised single-stream path drops this to roughly 4 % on the
//! same hardware. Both produce 32-byte outputs, so [`ChunkId`]'s
//! `[u8; 32]` shape is unchanged.
//!
//! **Pre-prod schema churn note** (memory `feedback_no_backcompat`):
//! a cluster that wrote chunks under SHA-256 cannot dedupe them
//! against new BLAKE3-keyed writes, and vice versa. The fix at
//! cutover is wipe-and-redeploy; production deployments stay on
//! whichever algorithm they were initialised with via the env var.
//!
//! ## FIPS deployments
//!
//! BLAKE3 is not on the FIPS 140-2/3 approved primitive list. Set
//! `KISEKI_CHUNK_HASH=sha256` at process start to force SHA-256 +
//! HMAC-SHA256. The selection is read once at first call and cached
//! in a [`std::sync::OnceLock`] — the hot path becomes a single
//! atomic load.

use std::sync::OnceLock;

use aws_lc_rs::digest;
use aws_lc_rs::hmac;
use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::DedupPolicy;

use crate::error::CryptoError;

/// Selected hash algorithm for `derive_chunk_id`. See module-level
/// docs.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ChunkHash {
    /// BLAKE3 (default). `CrossTenant`: `blake3::hash(plaintext)`.
    /// `TenantIsolated`: `blake3::keyed_hash(tenant_key_32, plaintext)`.
    Blake3,
    /// SHA-256 + HMAC-SHA256 (FIPS-friendly). `CrossTenant`:
    /// `sha256(plaintext)`. `TenantIsolated`:
    /// `HMAC-SHA256(tenant_key, plaintext)`.
    Sha256,
}

static SELECTED_HASH: OnceLock<ChunkHash> = OnceLock::new();

/// Resolve the configured chunk-hash algorithm. Reads the
/// `KISEKI_CHUNK_HASH` env var on first call and caches the
/// result. Subsequent calls are a single atomic load.
///
/// Recognised values (case-insensitive):
/// - `blake3` (default — used when the env var is unset, empty, or
///   any unrecognised string)
/// - `sha256` / `sha-256` / `sha_256`
#[must_use]
pub fn chunk_hash_algorithm() -> ChunkHash {
    *SELECTED_HASH.get_or_init(|| {
        let raw = std::env::var("KISEKI_CHUNK_HASH").unwrap_or_default();
        let normalised = raw.trim().to_ascii_lowercase();
        match normalised.as_str() {
            "sha256" | "sha-256" | "sha_256" => ChunkHash::Sha256,
            "" | "blake3" => ChunkHash::Blake3,
            other => {
                tracing::warn!(
                    value = other,
                    "KISEKI_CHUNK_HASH: unrecognised value, defaulting to blake3",
                );
                ChunkHash::Blake3
            }
        }
    })
}

/// Derive a 32-byte tenant key for BLAKE3 keyed-hash from any-length
/// HMAC key material. BLAKE3's `keyed_hash` requires exactly 32
/// bytes; the tenant HMAC key shape (per `kiseki-keymanager`) is
/// variable-length. Use BLAKE3's built-in KDF mode with a stable
/// context string so the derivation is deterministic AND domain-
/// separated from any other use of the tenant key.
fn derive_blake3_tenant_key(tenant_hmac_key: &[u8]) -> [u8; 32] {
    const CONTEXT: &str = "kiseki 2026-06-03 tenant chunk-id v1";
    blake3::derive_key(CONTEXT, tenant_hmac_key)
}

/// Derive a chunk ID from plaintext according to the tenant's dedup
/// policy and the cluster-selected chunk-hash algorithm.
///
/// For `CrossTenant`: `ChunkId = H(plaintext)`.
/// For `TenantIsolated`: `ChunkId = keyed_H(tenant_key, plaintext)`.
///
/// `H` is BLAKE3 by default, SHA-256 when `KISEKI_CHUNK_HASH=sha256`.
///
/// The `tenant_hmac_key` must be provided when `policy` is
/// `TenantIsolated`; it is ignored for `CrossTenant`.
pub fn derive_chunk_id(
    plaintext: &[u8],
    policy: DedupPolicy,
    tenant_hmac_key: Option<&[u8]>,
) -> Result<ChunkId, CryptoError> {
    match (chunk_hash_algorithm(), policy) {
        (ChunkHash::Blake3, DedupPolicy::CrossTenant) => {
            let hash = blake3::hash(plaintext);
            Ok(ChunkId(*hash.as_bytes()))
        }
        (ChunkHash::Blake3, DedupPolicy::TenantIsolated) => {
            let key_bytes = tenant_hmac_key.ok_or_else(|| {
                CryptoError::InvalidEnvelope(
                    "tenant HMAC key required for TenantIsolated dedup".into(),
                )
            })?;
            let derived = derive_blake3_tenant_key(key_bytes);
            let hash = blake3::keyed_hash(&derived, plaintext);
            Ok(ChunkId(*hash.as_bytes()))
        }
        (ChunkHash::Sha256, DedupPolicy::CrossTenant) => {
            let hash = digest::digest(&digest::SHA256, plaintext);
            let mut id = [0u8; 32];
            id.copy_from_slice(hash.as_ref());
            Ok(ChunkId(id))
        }
        (ChunkHash::Sha256, DedupPolicy::TenantIsolated) => {
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

    // ---------------------------------------------------------------
    // Scenario: Write a chunk with a tenant-isolated ID (opted-out
    // tenant). Same plaintext from two tenants with different keys
    // produces different IDs (I-K10). Cross-tenant dedup cannot match.
    // ---------------------------------------------------------------
    #[test]
    fn keyed_chunk_id_unique_per_tenant_no_cross_dedup() {
        let plaintext = b"identical payload";
        let key_defense = b"org-defense-tenant-key-material!";
        let key_pharma = b"org-pharma-tenant-key-material!!";

        let id_defense =
            derive_chunk_id(plaintext, DedupPolicy::TenantIsolated, Some(key_defense)).unwrap();
        let id_pharma =
            derive_chunk_id(plaintext, DedupPolicy::TenantIsolated, Some(key_pharma)).unwrap();

        assert_ne!(
            id_defense, id_pharma,
            "tenant-keyed IDs must differ across tenants (cross-tenant dedup blocked)"
        );

        let id_defense_2 =
            derive_chunk_id(plaintext, DedupPolicy::TenantIsolated, Some(key_defense)).unwrap();
        assert_eq!(
            id_defense, id_defense_2,
            "tenant-keyed ID must be deterministic within a tenant"
        );

        let id_cross = derive_chunk_id(plaintext, DedupPolicy::CrossTenant, None).unwrap();
        assert_ne!(
            id_cross, id_defense,
            "tenant-keyed ID must differ from cross-tenant ID"
        );
    }

    #[test]
    fn blake3_default_when_env_var_unset() {
        // The OnceLock is process-global so this test only proves the
        // default branch is correct, not that the env var swap works
        // (a second test process would be needed to assert env-driven
        // switching). The OnceLock initialiser runs once per process.
        let raw = std::env::var("KISEKI_CHUNK_HASH").unwrap_or_default();
        if raw.is_empty() {
            assert_eq!(chunk_hash_algorithm(), ChunkHash::Blake3);
        }
    }
}
