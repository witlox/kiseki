//! Delta types — structurally separated header and payload (I-L7).
//!
//! The header is system-visible (cleartext metadata for routing,
//! ordering, and compaction). The payload is tenant-encrypted and
//! opaque to the Log — compaction carries it without decryption.

use kiseki_common::ids::{ChunkId, OrgId, SequenceNumber, ShardId};
use kiseki_common::time::DeltaTimestamp;

/// Operation type for a delta.
///
/// Spec: `data-models/log.rs`, `ubiquitous-language.md#Delta`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum OperationType {
    /// Create a new composition or entry.
    Create,
    /// Update an existing composition.
    Update,
    /// Delete a composition or entry (tombstone).
    Delete,
    /// Rename (within the same shard — cross-shard is `EXDEV`, I-L8).
    Rename,
    /// Set attributes (xattrs, permissions, etc.).
    SetAttribute,
    /// Finalize a multipart upload — gates reader visibility (I-L5).
    Finalize,
}

/// System-visible delta header — cleartext metadata.
///
/// Compaction operates on headers only; the payload is carried
/// opaquely (I-L7). The `hashed_key` field determines the key range
/// for shard splitting and `SSTable` merge ordering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeltaHeader {
    /// Raft-assigned sequence number — monotonic, gap-free within
    /// the shard (I-L1).
    pub sequence: SequenceNumber,
    /// Shard this delta belongs to.
    pub shard_id: ShardId,
    /// Tenant that owns this delta.
    pub tenant_id: OrgId,
    /// Operation type.
    pub operation: OperationType,
    /// Dual-clock timestamp.
    pub timestamp: DeltaTimestamp,
    /// `sha256(parent_dir_id || name)` — determines key range for
    /// shard split and merge ordering during compaction.
    pub hashed_key: [u8; 32],
    /// Whether this delta is a tombstone (delete marker).
    pub tombstone: bool,
    /// Chunk references (for non-inline data).
    pub chunk_refs: Vec<ChunkId>,
    /// Size of the encrypted payload in bytes.
    pub payload_size: u32,
    /// Whether the payload includes inline data (below threshold).
    pub has_inline_data: bool,
}

/// Tenant-encrypted delta payload — opaque to the Log.
///
/// The Log stores, replicates, and carries this blob without ever
/// decrypting it. Only the Composition context (with tenant KEK)
/// can interpret the contents. Carries full AEAD metadata so the
/// payload is self-describing for decryption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeltaPayload {
    /// Encrypted payload bytes.
    pub ciphertext: Vec<u8>,
    /// AEAD authentication tag (16 bytes).
    pub auth_tag: Vec<u8>,
    /// AEAD nonce (12 bytes).
    pub nonce: Vec<u8>,
    /// System key epoch used for encryption.
    pub system_epoch: Option<u64>,
    /// Tenant key epoch used for wrapping.
    pub tenant_epoch: Option<u64>,
    /// Tenant KEK-wrapped derivation material.
    pub tenant_wrapped_material: Vec<u8>,
}

/// Complete delta = header + payload (I-L3: immutable once committed).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delta {
    /// System-visible header.
    pub header: DeltaHeader,
    /// Tenant-encrypted payload (opaque to the Log).
    pub payload: DeltaPayload,
}
