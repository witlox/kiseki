//! `LogOps` trait — the public API of the Log context.
//!
//! Spec: `api-contracts.md` §Log, `data-models/log.rs`.

use kiseki_common::ids::{OrgId, SequenceNumber, ShardId};
use kiseki_common::time::DeltaTimestamp;

use crate::delta::{Delta, OperationType};
use crate::error::LogError;
use crate::shard::ShardInfo;

/// Request to append a delta to a shard.
#[derive(Clone, Debug)]
pub struct AppendDeltaRequest {
    /// Target shard.
    pub shard_id: ShardId,
    /// Tenant.
    pub tenant_id: OrgId,
    /// Operation type.
    pub operation: OperationType,
    /// Timestamp for the delta.
    pub timestamp: DeltaTimestamp,
    /// Routing key — `sha256(parent_dir_id || name)`.
    pub hashed_key: [u8; 32],
    /// Chunk references (empty for inline data).
    pub chunk_refs: Vec<kiseki_common::ids::ChunkId>,
    /// Encrypted payload (opaque to the Log).
    pub payload: Vec<u8>,
    /// Whether payload includes inline data.
    pub has_inline_data: bool,
}

/// Request to read a range of deltas.
#[derive(Clone, Debug)]
pub struct ReadDeltasRequest {
    /// Shard to read from.
    pub shard_id: ShardId,
    /// Start position (inclusive).
    pub from: SequenceNumber,
    /// End position (inclusive).
    pub to: SequenceNumber,
}

/// The Log context API.
///
/// All mutation methods take `&self` (not `&mut self`) because the
/// Raft-backed implementation uses interior mutability — mutations go
/// through the consensus layer, not direct field access. In-memory
/// implementations use `Mutex` or `RefCell` internally.
///
/// Implementations: `MemShardStore` (in-memory, for testing),
/// `RaftShardStore` (production, with openraft — future).
pub trait LogOps {
    /// Append a delta to a shard. Returns the assigned sequence number.
    ///
    /// Fails if the shard is in maintenance mode, splitting (for
    /// out-of-range keys), or has lost Raft quorum.
    fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError>;

    /// Read deltas in `[from, to]` inclusive from a shard.
    fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError>;

    /// Get shard health and metadata.
    fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError>;

    /// Set or clear maintenance mode on a shard (I-O6).
    fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError>;

    /// Run GC: truncate deltas below the minimum consumer watermark.
    /// Returns the new GC boundary.
    fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError>;

    /// Run compaction on a shard: merge deltas by `(hashed_key, sequence)`.
    ///
    /// Newer deltas (higher sequence) supersede older ones for the same
    /// `hashed_key`. Tombstones are removed if all consumers have
    /// advanced past them. Payloads are carried opaquely — never
    /// decrypted (I-L7). Returns the number of deltas removed.
    fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError>;
}
