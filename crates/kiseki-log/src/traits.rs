//! `LogOps` trait — the public API of the Log context.
//!
//! Spec: `api-contracts.md` §Log, `data-models/log.rs`.

use kiseki_common::ids::{NodeId, OrgId, SequenceNumber, ShardId};
use kiseki_common::time::DeltaTimestamp;

use crate::delta::{Delta, OperationType};
use crate::error::LogError;
use crate::shard::{ShardConfig, ShardInfo, ShardState};

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
/// `RaftShardStore` (production, with openraft).
///
/// All methods are async (ADR-032) to avoid thread starvation when
/// bridging to the Raft consensus layer under concurrent load.
#[async_trait::async_trait]
pub trait LogOps: Send + Sync {
    /// Append a delta to a shard. Returns the assigned sequence number.
    ///
    /// Fails if the shard is in maintenance mode, splitting (for
    /// out-of-range keys), or has lost Raft quorum.
    async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError>;

    /// Read deltas in `[from, to]` inclusive from a shard.
    async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError>;

    /// Get shard health and metadata.
    async fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError>;

    /// Set or clear maintenance mode on a shard (I-O6).
    async fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError>;

    /// Run GC: truncate deltas below the minimum consumer watermark.
    /// Returns the new GC boundary.
    async fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError>;

    /// Run compaction on a shard: merge deltas by `(hashed_key, sequence)`.
    ///
    /// Newer deltas (higher sequence) supersede older ones for the same
    /// `hashed_key`. Tombstones are removed if all consumers have
    /// advanced past them. Payloads are carried opaquely — never
    /// decrypted (I-L7). Returns the number of deltas removed.
    async fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError>;

    // --- Shard management (ADR-036) ---

    /// Create a new shard with the given parameters.
    ///
    /// Idempotent: if the shard already exists, this is a no-op.
    /// Sync because shard metadata is local state (control plane Raft
    /// handles distributed coordination separately).
    fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        node_id: NodeId,
        config: ShardConfig,
    );

    /// Update a shard's key range (used during split/merge, ADR-033/034).
    fn update_shard_range(
        &self,
        shard_id: ShardId,
        range_start: [u8; 32],
        range_end: [u8; 32],
    );

    /// Transition a shard's lifecycle state (ADR-034 merge protocol).
    fn set_shard_state(&self, shard_id: ShardId, state: ShardState);

    // --- Consumer watermarks (ADR-036, I-L4) ---

    /// Register a consumer at a starting position.
    ///
    /// Async because on Raft-backed stores, consumer state is part of
    /// the replicated state machine.
    async fn register_consumer(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError>;

    /// Advance a consumer's watermark. Only moves forward.
    ///
    /// Callers advance watermarks BEFORE calling `truncate_log` — GC
    /// uses `min(all watermarks)` as the boundary (I-L4).
    async fn advance_watermark(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError>;
}
