//! On-disk record encoding for the chunk meta store (ADR-022 rev-4).
//!
//! Two record shapes — chunk and fragment — both wrapped in a
//! 1-byte schema-version prefix so a future incompatible format
//! change is fail-closed (binary too old). Mirror of
//! `kiseki-composition::persistent::encoding`; keeping the pattern
//! consistent across the two hot-path stores so swapping backends
//! again later is mechanical.
//!
//! Postcard for the payload bytes — same choice as composition.
//! Deterministic, compact, no schema metadata in the value.

use kiseki_common::ids::ChunkId;
use serde::{Deserialize, Serialize};

use crate::error::ChunkError;

/// Current on-disk schema version for both record shapes. Bumped on
/// any incompatible change. Records carrying `version > supported`
/// fail open with [`ChunkError::Io`] so an operator running an older
/// binary against a newer data dir gets a clear "binary too old"
/// surface instead of silent corruption.
pub const CHUNK_RECORD_SCHEMA_VERSION: u8 = 1;

// -- Chunk meta record (one per `chunk_id`) ----------------------------

/// Serialisable form of `ChunkEntry::envelope_meta + extents`. Kept
/// out of `persistent_store.rs` so the wire format is the single
/// source of truth — backend swaps don't reserialise rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// Content-addressed chunk identifier (HKDF-SHA256 over the
    /// plaintext) — 32 bytes, the same value [`ChunkId`] wraps.
    pub chunk_id: [u8; 32],
    /// How many compositions reference this chunk. The deduper
    /// increments on every PUT that resolves to an existing
    /// `chunk_id`; deletion is gated on `refcount` reaching zero.
    pub refcount: u64,
    /// Active retention holds that prevent deletion regardless of
    /// `refcount`. Each entry is a free-form hold tag (legal hold,
    /// compliance window) supplied by the gateway.
    pub retention_holds: Vec<String>,
    /// Affinity pool the chunk landed in (e.g. `"hot"`, `"cold"`,
    /// `"pool-fast"`). Drives placement on subsequent reads /
    /// scrubs.
    pub pool_name: String,
    /// Bytes used on the device, extent-aligned. Always ≥
    /// `data_bytes`; the difference is allocator slack.
    pub stored_bytes: u64,
    /// Actual data length in bytes (distinct from extent-aligned
    /// `stored_bytes`). Preserves the same `serde(default)` tolerance
    /// the JSON layout had so a record written by a binary that didn't
    /// know about the field still decodes.
    #[serde(default)]
    pub data_bytes: u64,
    /// Byte offset on the backing device of the primary (first)
    /// extent.
    pub extent_offset: u64,
    /// Length in bytes of the primary extent. Combined with
    /// `extra_extents` this covers the entire ciphertext.
    pub extent_length: u64,
    /// Additional extents holding the rest of the ciphertext, in
    /// order. Empty for single-extent chunks (the common case).
    /// Matches the JSON `extra_extents` field byte-for-byte at the
    /// postcard layer (Vec<(u64, u64)>).
    #[serde(default)]
    pub extra_extents: Vec<(u64, u64)>,
    /// AEAD nonce (12 B) used to seal this chunk. Persisted so a
    /// fresh process can decrypt without a server round-trip.
    pub nonce: [u8; 12],
    /// AEAD authentication tag (16 B) bound to the ciphertext + AD.
    pub auth_tag: [u8; 16],
    /// System DEK epoch in effect when the chunk was sealed.
    /// Drives unwrap-key selection during decrypt.
    pub system_epoch: u64,
    /// Tenant DEK epoch when the chunk was sealed under a
    /// per-tenant key (cross-tenant deduped chunks have `None`).
    pub tenant_epoch: Option<u64>,
    /// Tenant-key-wrapped DEK material (optional — present when
    /// `tenant_epoch` is `Some`). Unwrapped at read time via the
    /// tenant's KEK from the keymanager.
    pub tenant_wrapped_material: Option<Vec<u8>>,
}

/// `[1 byte: version][postcard payload]`. The version prefix gates
/// future incompatible format changes — see
/// [`CHUNK_RECORD_SCHEMA_VERSION`].
///
/// # Errors
/// [`ChunkError::Io`] if postcard encoding fails (allocation or
/// serializer fault).
pub fn encode_chunk(record: &ChunkRecord) -> Result<Vec<u8>, ChunkError> {
    let mut out = Vec::with_capacity(256);
    out.push(CHUNK_RECORD_SCHEMA_VERSION);
    let payload = postcard::to_stdvec(record)
        .map_err(|e| ChunkError::Io(format!("encode chunk record: {e}")))?;
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Reverse of [`encode_chunk`]. Strips the version prefix, rejects
/// payloads with `version > CHUNK_RECORD_SCHEMA_VERSION` (binary
/// too old), and postcard-decodes the rest into a [`ChunkRecord`].
///
/// # Errors
/// [`ChunkError::Io`] if the input is empty, the version is newer
/// than this binary supports, or the payload fails postcard decode.
pub fn decode_chunk(bytes: &[u8]) -> Result<ChunkRecord, ChunkError> {
    let Some((&version, payload)) = bytes.split_first() else {
        return Err(ChunkError::Io("empty chunk record".into()));
    };
    if version > CHUNK_RECORD_SCHEMA_VERSION {
        return Err(ChunkError::Io(format!(
            "chunk record schema too new: found={version} supported={CHUNK_RECORD_SCHEMA_VERSION}"
        )));
    }
    postcard::from_bytes(payload).map_err(|e| ChunkError::Io(format!("decode chunk record: {e}")))
}

// -- Fragment meta record (one per (`chunk_id`, `fragment_index`)) ----

/// Serialisable form of `FragmentEntry::meta + extent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentRecord {
    /// Parent chunk's content-addressed identifier.
    pub chunk_id: [u8; 32],
    /// EC fragment index — `0..k` for data shards, `k..k+m` for
    /// parity. Dense enumeration; gaps signal a lost shard.
    pub fragment_index: u32,
    /// Byte offset on the backing device of this fragment's
    /// extent.
    pub extent_offset: u64,
    /// Length in bytes of this fragment's extent.
    pub extent_length: u64,
    /// Actual ciphertext length carried inside the extent
    /// (extent length is allocator-aligned).
    pub data_bytes: u64,
}

/// `[1 byte: version][postcard payload]` — same layout as
/// [`encode_chunk`] but for [`FragmentRecord`].
///
/// # Errors
/// [`ChunkError::Io`] if postcard encoding fails (allocation or
/// serializer fault).
pub fn encode_fragment(record: &FragmentRecord) -> Result<Vec<u8>, ChunkError> {
    let mut out = Vec::with_capacity(64);
    out.push(CHUNK_RECORD_SCHEMA_VERSION);
    let payload = postcard::to_stdvec(record)
        .map_err(|e| ChunkError::Io(format!("encode fragment record: {e}")))?;
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Reverse of [`encode_fragment`]. Same version-gating discipline
/// as [`decode_chunk`].
///
/// # Errors
/// [`ChunkError::Io`] if the input is empty, the version is newer
/// than this binary supports, or the payload fails postcard decode.
pub fn decode_fragment(bytes: &[u8]) -> Result<FragmentRecord, ChunkError> {
    let Some((&version, payload)) = bytes.split_first() else {
        return Err(ChunkError::Io("empty fragment record".into()));
    };
    if version > CHUNK_RECORD_SCHEMA_VERSION {
        return Err(ChunkError::Io(format!(
            "fragment record schema too new: found={version} supported={CHUNK_RECORD_SCHEMA_VERSION}"
        )));
    }
    postcard::from_bytes(payload)
        .map_err(|e| ChunkError::Io(format!("decode fragment record: {e}")))
}

// -- Key encodings ----------------------------------------------------

/// Chunk key = the 32 raw bytes of the `ChunkId`. No length prefix —
/// every key is exactly 32 bytes, fjall's range scans + prefix
/// iterators are happy with the fixed shape.
#[must_use]
pub fn chunk_key(id: &ChunkId) -> [u8; 32] {
    id.0
}

/// Fragment key = `[chunk_id (32 B)] || [fragment_index (4 B big-endian)]`
/// = 36 bytes. Big-endian on the index so a per-chunk prefix scan
/// yields fragments in `fragment_index` order, matching what
/// `list_fragments` expects.
#[must_use]
pub fn fragment_key(id: &ChunkId, fragment_index: u32) -> [u8; 36] {
    let mut out = [0u8; 36];
    out[..32].copy_from_slice(&id.0);
    out[32..].copy_from_slice(&fragment_index.to_be_bytes());
    out
}

/// Reverse of [`fragment_key`]. Returns `(chunk_id, fragment_index)`.
pub fn decode_fragment_key(bytes: &[u8]) -> Result<(ChunkId, u32), ChunkError> {
    if bytes.len() != 36 {
        return Err(ChunkError::Io(format!(
            "fragment key wrong length: {}",
            bytes.len()
        )));
    }
    let mut id_buf = [0u8; 32];
    id_buf.copy_from_slice(&bytes[..32]);
    let mut fragment_index_bytes = [0u8; 4];
    fragment_index_bytes.copy_from_slice(&bytes[32..]);
    Ok((ChunkId(id_buf), u32::from_be_bytes(fragment_index_bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_chunk_record() -> ChunkRecord {
        ChunkRecord {
            chunk_id: [0x42; 32],
            refcount: 7,
            retention_holds: vec!["legal-hold-A".into(), "compliance-2027".into()],
            pool_name: "pool-fast".into(),
            stored_bytes: 1024,
            data_bytes: 1000,
            extent_offset: 4096,
            extent_length: 1024,
            extra_extents: vec![(8192, 1024), (12_288, 1024)],
            nonce: [0xAB; 12],
            auth_tag: [0xCD; 16],
            system_epoch: 3,
            tenant_epoch: Some(11),
            tenant_wrapped_material: Some(vec![0xEE; 48]),
        }
    }

    #[test]
    fn chunk_record_roundtrip() {
        let r = sample_chunk_record();
        let bytes = encode_chunk(&r).unwrap();
        // Must carry the version byte.
        assert_eq!(bytes[0], CHUNK_RECORD_SCHEMA_VERSION);
        let r2 = decode_chunk(&bytes).unwrap();
        assert_eq!(r, r2);
    }

    #[test]
    fn fragment_record_roundtrip() {
        let r = FragmentRecord {
            chunk_id: [0x55; 32],
            fragment_index: 3,
            extent_offset: 65_536,
            extent_length: 16_384,
            data_bytes: 16_384,
        };
        let bytes = encode_fragment(&r).unwrap();
        assert_eq!(bytes[0], CHUNK_RECORD_SCHEMA_VERSION);
        let r2 = decode_fragment(&bytes).unwrap();
        assert_eq!(r, r2);
    }

    #[test]
    fn schema_too_new_rejected() {
        let mut bytes = encode_chunk(&sample_chunk_record()).unwrap();
        bytes[0] = CHUNK_RECORD_SCHEMA_VERSION + 1;
        let err = decode_chunk(&bytes).unwrap_err();
        assert!(err.to_string().contains("schema too new"));
    }

    #[test]
    fn empty_record_rejected() {
        let err = decode_chunk(&[]).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn fragment_key_orders_by_index() {
        let id = ChunkId([0; 32]);
        let k0 = fragment_key(&id, 0);
        let k1 = fragment_key(&id, 1);
        let k_max = fragment_key(&id, u32::MAX);
        // Big-endian guarantees lex order matches numeric order.
        assert!(k0 < k1);
        assert!(k1 < k_max);
    }

    #[test]
    fn fragment_key_decode_roundtrip() {
        let id = ChunkId([0xAA; 32]);
        let k = fragment_key(&id, 17);
        let (id2, idx) = decode_fragment_key(&k).unwrap();
        assert_eq!(id2, id);
        assert_eq!(idx, 17);
    }
}
