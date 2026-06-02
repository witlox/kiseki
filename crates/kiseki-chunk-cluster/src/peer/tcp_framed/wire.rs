//! Wire-level message types for the fabric over TCP-framed-postcard.
//!
//! Why this exists: until 2026-06-01 the fabric (`FabricPeer`) ran
//! exclusively over tonic gRPC. The local 3-node loopback profile that
//! day measured `put_send.transport = 1 598 µs` per fragment vs ~115 µs
//! of actual receiver-side work — ~14× the receiver cost, all of it in
//! the gRPC/h2 stack. ADR-042 §2.2 introduced TCP-framed-postcard for
//! the gateway↔client edge for exactly this reason; the fabric edge
//! was overlooked. This module + its `client` / `server` siblings
//! close that gap.
//!
//! Wire layout reuses
//! [`kiseki_proto::native_contract::wire_tcp_framed`] unchanged — that
//! module's frame protocol is verb-agnostic. We only add the verb
//! catalogue (`FABRIC_VERB_*` consts) and the typed metadata + bulk
//! split for each verb.
//!
//! ## Verb catalogue
//!
//! | tag                  | request meta                                   | request bulk        | response meta             | response bulk      |
//! | -------------------- | ---------------------------------------------- | ------------------- | ------------------------- | ------------------ |
//! | `fb.put_fragment`    | [`PutFragmentMeta`]                            | `envelope.ciphertext` | [`PutFragmentResponse`] | (empty)            |
//! | `fb.get_fragment`    | [`GetFragmentMeta`]                            | (empty)             | [`GetFragmentResponseMeta`] | `envelope.ciphertext` |
//! | `fb.delete_fragment` | [`DeleteFragmentMeta`]                         | (empty)             | [`DeleteFragmentResponse`] | (empty)            |
//! | `fb.has_fragment`    | [`HasFragmentMeta`]                            | (empty)             | [`HasFragmentResponse`]   | (empty)            |
//!
//! `put_fragment` and `get_fragment` split the envelope's
//! `ciphertext` (the bulk Vec<u8>) onto the bulk lane — same V3
//! perf trick the gateway path already uses so we don't postcard-
//! encode/decode 64 KiB on every fragment.

use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use serde::{Deserialize, Serialize};

/// Verb tag for `FabricPeer::put_fragment`.
pub const FABRIC_VERB_PUT_FRAGMENT: &str = "fb.put_fragment";
/// Verb tag for `FabricPeer::get_fragment`.
pub const FABRIC_VERB_GET_FRAGMENT: &str = "fb.get_fragment";
/// Verb tag for `FabricPeer::delete_fragment`.
pub const FABRIC_VERB_DELETE_FRAGMENT: &str = "fb.delete_fragment";
/// Verb tag for `FabricPeer::has_fragment`.
pub const FABRIC_VERB_HAS_FRAGMENT: &str = "fb.has_fragment";

/// Envelope metadata MINUS the ciphertext bulk (V3-style split).
///
/// `ciphertext` rides on the wire's `bulk_bytes` lane — never inside
/// this postcard struct — so a 64 KiB fragment skips the postcard
/// serialize/deserialize memcopy on both sides. Reconstruct via
/// [`EnvelopeMeta::with_ciphertext`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeMeta {
    /// 16-byte AES-GCM authentication tag.
    pub auth_tag: [u8; 16],
    /// 12-byte AES-GCM nonce (ADR-044 convergent).
    pub nonce: [u8; 12],
    /// System key epoch used for DEK derivation.
    pub system_epoch: KeyEpoch,
    /// Optional tenant key epoch (set when the envelope is wrapped).
    pub tenant_epoch: Option<KeyEpoch>,
    /// Tenant KEK wraps the system DEK derivation material.
    pub tenant_wrapped_material: Option<Vec<u8>>,
    /// Content-addressed chunk id (clear for dedup + routing).
    pub chunk_id: ChunkId,
}

impl EnvelopeMeta {
    /// Decompose [`kiseki_crypto::envelope::Envelope`] into (meta, ciphertext).
    /// Used by the client right before writing a `put_fragment` frame.
    #[must_use]
    pub fn split_from(env: kiseki_crypto::envelope::Envelope) -> (Self, Vec<u8>) {
        let kiseki_crypto::envelope::Envelope {
            ciphertext,
            auth_tag,
            nonce,
            system_epoch,
            tenant_epoch,
            tenant_wrapped_material,
            chunk_id,
        } = env;
        (
            Self {
                auth_tag,
                nonce,
                system_epoch,
                tenant_epoch,
                tenant_wrapped_material,
                chunk_id,
            },
            ciphertext,
        )
    }

    /// Reassemble an [`Envelope`] from this meta + the bulk ciphertext.
    /// Used by the server's `put_fragment` handler and the client's
    /// `get_fragment` response handler.
    #[must_use]
    pub fn with_ciphertext(self, ciphertext: Vec<u8>) -> kiseki_crypto::envelope::Envelope {
        kiseki_crypto::envelope::Envelope {
            ciphertext,
            auth_tag: self.auth_tag,
            nonce: self.nonce,
            system_epoch: self.system_epoch,
            tenant_epoch: self.tenant_epoch,
            tenant_wrapped_material: self.tenant_wrapped_material,
            chunk_id: self.chunk_id,
        }
    }
}

// -- put_fragment ----------------------------------------------------

/// `fb.put_fragment` request meta. The envelope's ciphertext rides on
/// the bulk lane, the other envelope fields are in [`Self::envelope`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutFragmentMeta {
    /// Chunk id (also embedded in [`EnvelopeMeta::chunk_id`]; carried
    /// here too for early routing/inspection before bulk read).
    pub chunk_id: ChunkId,
    /// EC fragment index. `0` means whole-envelope (legacy
    /// Replication-N path, not a true EC shard).
    pub fragment_index: u32,
    /// Tenant scope for placement.
    pub tenant_id: OrgId,
    /// Affinity pool / class hint (ADR-045). Empty string = default.
    pub pool_id: String,
    /// Envelope metadata MINUS the ciphertext.
    pub envelope: EnvelopeMeta,
}

/// `fb.put_fragment` response meta. Returned via `meta_bytes`; bulk empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutFragmentResponse {
    /// `true` if the receiver stored a fresh copy, `false` on dedup.
    pub stored: bool,
}

// -- get_fragment ----------------------------------------------------

/// `fb.get_fragment` request meta. No bulk; response carries the
/// envelope split across meta + bulk lanes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetFragmentMeta {
    /// Chunk id of the fragment to fetch.
    pub chunk_id: ChunkId,
    /// EC fragment index.
    pub fragment_index: u32,
}

/// `fb.get_fragment` response meta. Bulk = envelope ciphertext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetFragmentResponseMeta {
    /// Envelope fields excluding the ciphertext (which is bulk).
    pub envelope: EnvelopeMeta,
}

// -- delete_fragment -------------------------------------------------

/// `fb.delete_fragment` request meta. Idempotent on the receiver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteFragmentMeta {
    /// Chunk id of the fragment to delete.
    pub chunk_id: ChunkId,
    /// EC fragment index.
    pub fragment_index: u32,
    /// Tenant scope (mirrors the gRPC service's authn check).
    pub tenant_id: OrgId,
}

/// `fb.delete_fragment` response meta.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteFragmentResponse {
    /// `true` if the fragment was present and deleted, `false` if
    /// absent (still success — idempotent).
    pub deleted: bool,
}

// -- has_fragment ----------------------------------------------------

/// `fb.has_fragment` request meta. Used by the repair scrub.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HasFragmentMeta {
    /// Chunk id of the fragment to probe.
    pub chunk_id: ChunkId,
    /// EC fragment index.
    pub fragment_index: u32,
}

/// `fb.has_fragment` response meta.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HasFragmentResponse {
    /// `true` iff the peer holds this `(chunk_id, fragment_index)`.
    pub present: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_meta_roundtrip() {
        let env = kiseki_crypto::envelope::Envelope {
            ciphertext: vec![0xab; 100],
            auth_tag: [0x42; 16],
            nonce: [0x33; 12],
            system_epoch: KeyEpoch(7),
            tenant_epoch: Some(KeyEpoch(3)),
            tenant_wrapped_material: Some(vec![0xcd; 40]),
            chunk_id: ChunkId([0x99; 32]),
        };
        let (meta, ct) = EnvelopeMeta::split_from(env.clone());
        let reconstructed = meta.with_ciphertext(ct);
        assert_eq!(env, reconstructed);
    }

    #[test]
    fn put_fragment_meta_postcard_roundtrip() {
        let meta = PutFragmentMeta {
            chunk_id: ChunkId([0x11; 32]),
            fragment_index: 3,
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            pool_id: "fast-nvme".into(),
            envelope: EnvelopeMeta {
                auth_tag: [0; 16],
                nonce: [0; 12],
                system_epoch: KeyEpoch(1),
                tenant_epoch: None,
                tenant_wrapped_material: None,
                chunk_id: ChunkId([0x11; 32]),
            },
        };
        let bytes = postcard::to_allocvec(&meta).unwrap();
        let decoded: PutFragmentMeta = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(meta, decoded);
    }
}
