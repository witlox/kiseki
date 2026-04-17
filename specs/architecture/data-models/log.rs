//! Log context types — delta ordering, shard lifecycle, Raft.
//! Spec: domain-model.md#Log, invariants.md#Log, features/log.feature

use crate::common::*;
use crate::crypto::*;

// --- Delta ---

/// System-visible delta header — cleartext or system-encrypted.
/// Compaction operates on headers only (I-L7, I-O2).
/// Spec: ubiquitous-language.md#Delta
pub struct DeltaHeader {
    pub sequence: SequenceNumber,
    pub shard_id: ShardId,
    pub tenant_id: OrgId,
    pub operation: OperationType,
    pub timestamp: DeltaTimestamp,
    /// hash(parent_dir_id, name) — for compaction merge ordering
    pub hashed_key: [u8; 32],
    pub tombstone: bool,
    /// Chunk IDs referenced by this delta (already content hashes)
    pub chunk_refs: Vec<ChunkId>,
    /// Size of the encrypted payload
    pub payload_size: u32,
    /// For inline data: true if payload contains inline data below threshold
    pub has_inline_data: bool,
}

pub enum OperationType {
    Create,
    Update,
    Delete,
    Rename,
    SetAttribute,
    /// Multipart finalize — composition becomes visible to readers (I-L5)
    Finalize,
}

/// Tenant-encrypted delta payload — contains actual filenames, attributes,
/// inline data. Encrypted with system DEK, wrapped with tenant KEK.
/// Spec: I-K3
pub struct DeltaPayload {
    pub envelope: Envelope,
}

/// Complete delta = header + payload.
/// Spec: I-L3 (immutable once committed)
pub struct Delta {
    pub header: DeltaHeader,
    pub payload: DeltaPayload,
}

// --- Shard ---

/// Shard — smallest unit of totally-ordered deltas.
/// Spec: ubiquitous-language.md#Shard, I-L1, I-L6
pub struct ShardInfo {
    pub shard_id: ShardId,
    pub tenant_id: OrgId,
    pub namespace_id: NamespaceId,
    /// Raft group members (node IDs)
    pub raft_members: Vec<NodeId>,
    pub leader: Option<NodeId>,
    /// Current sequence number (tip of log)
    pub tip: SequenceNumber,
    /// Shard state
    pub state: ShardState,
    /// Split thresholds (configurable)
    pub split_config: ShardSplitConfig,
}

pub enum ShardState {
    Healthy,
    /// Leader election in progress
    Election,
    /// Quorum lost — writes unavailable
    QuorumLost,
    /// Split in progress — writes buffered for new key range
    Splitting { boundary: [u8; 32], new_shard: ShardId },
    /// Read-only maintenance mode (I-O6)
    Maintenance,
}

/// Configurable split thresholds.
/// Spec: I-L6 (hard ceiling, multi-dimension)
pub struct ShardSplitConfig {
    pub max_delta_count: u64,
    pub max_byte_size: u64,
    pub max_write_throughput_bytes_per_sec: u64,
}

// --- Consumer watermarks (for GC) ---

/// Tracks consumer positions for delta GC.
/// Spec: I-L4 (all consumers must advance before GC)
pub struct ConsumerWatermarks {
    pub shard_id: ShardId,
    /// Stream processor watermarks per view
    pub view_watermarks: Vec<(ViewId, SequenceNumber)>,
    /// Audit log watermark
    pub audit_watermark: SequenceNumber,
    /// MVCC read pins (bounded TTL, spec: I-V4)
    pub active_pins: Vec<ReadPin>,
}

pub struct ReadPin {
    pub pin_id: uuid::Uuid,
    pub position: SequenceNumber,
    pub created_at: WallTime,
    pub ttl_seconds: u32,
}

// --- Commands ---

/// Spec: cross-context/interactions.md, features/log.feature
pub struct AppendDeltaRequest {
    pub shard_id: ShardId,
    pub delta: Delta,
}

pub struct AppendDeltaResponse {
    pub sequence: SequenceNumber,
}

pub struct ReadDeltasRequest {
    pub shard_id: ShardId,
    pub from: SequenceNumber,
    pub to: SequenceNumber,
}

pub struct SplitShardRequest {
    pub shard_id: ShardId,
    /// Determined by the system based on key distribution
    pub boundary: [u8; 32],
}

pub struct CompactShardRequest {
    pub shard_id: ShardId,
    /// Admin-triggered or automatic
    pub trigger: CompactionTrigger,
}

pub enum CompactionTrigger {
    Automatic,
    AdminTriggered,
}

// --- Trait stubs ---

/// Log context operations.
pub trait LogOps {
    fn append_delta(&self, req: AppendDeltaRequest) -> Result<AppendDeltaResponse, KisekiError>;
    fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, KisekiError>;
    fn split_shard(&self, req: SplitShardRequest) -> Result<ShardId, KisekiError>;
    fn compact_shard(&self, req: CompactShardRequest) -> Result<(), KisekiError>;
    fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, KisekiError>;
    fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), KisekiError>;
    fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, KisekiError>;
}
