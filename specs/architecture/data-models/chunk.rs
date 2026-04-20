//! Chunk Storage context types — encrypted chunks, placement, EC, GC.
//! Spec: domain-model.md#ChunkStorage, invariants.md#Chunk, features/chunk-storage.feature

use crate::common::*;
use crate::crypto::*;

// --- Chunk ---

/// Stored chunk metadata (the ciphertext lives on disk, not in this struct).
/// Spec: ubiquitous-language.md#Chunk, I-C1 (immutable)
pub struct ChunkMeta {
    pub chunk_id: ChunkId,
    pub envelope_meta: EnvelopeMeta,
    pub pool_id: AffinityPoolId,
    pub placement: ChunkPlacement,
    pub refcount: u64,
    pub retention_holds: Vec<RetentionHoldId>,
    pub created_at: DeltaTimestamp,
    /// Size of ciphertext on disk
    pub ciphertext_size: u64,
    /// Whether compression was applied (tenant opt-in)
    pub compressed: bool,
}

/// Minimal envelope metadata stored alongside chunk (not the ciphertext itself).
pub struct EnvelopeMeta {
    pub algorithm: EncryptionAlgorithm,
    pub system_epoch: KeyEpoch,
    pub nonce: [u8; 12],
    pub auth_tag: [u8; 16],
}

// --- Affinity pools ---

/// Spec: ubiquitous-language.md#AffinityPool
pub struct AffinityPoolId(pub uuid::Uuid);

pub struct AffinityPool {
    pub pool_id: AffinityPoolId,
    pub device_class: DeviceClass,
    pub durability: DurabilityStrategy,
    pub devices: Vec<DeviceId>,
    pub capacity_bytes: u64,
    pub used_bytes: u64,
}

pub struct DeviceId(pub uuid::Uuid);

/// Spec: ADR-024
pub enum DeviceClass {
    NvmeU2,
    NvmeQlc,
    NvmePersistentMemory,
    SsdSata,
    HddEnterprise,
    HddBulk,
    Custom(String),
}

/// Spec: ADR-024
pub enum DeviceState {
    Healthy,
    Degraded { reason: String },
    Evacuating { progress_percent: u8 },
    Failed { since_epoch_ms: u64 },
    Removed,
}

/// Spec: I-C4, ADR-005
pub enum DurabilityStrategy {
    ErasureCoding { data_chunks: u8, parity_chunks: u8 },
    Replication { copies: u8 },
}

// --- Placement ---

pub struct ChunkPlacement {
    /// For EC: which devices hold which fragments
    pub fragments: Vec<(DeviceId, u32)>,
    /// For replication: which devices hold copies
    pub replicas: Vec<DeviceId>,
}

// --- Retention holds ---

/// Spec: ubiquitous-language.md#RetentionHold, I-C2b
pub struct RetentionHoldId(pub uuid::Uuid);

pub struct RetentionHold {
    pub hold_id: RetentionHoldId,
    pub scope: RetentionScope,
    pub created_at: WallTime,
    /// None = explicit release required
    pub expires_at: Option<WallTime>,
    pub reason: String,
}

pub enum RetentionScope {
    Tenant(OrgId),
    Namespace(NamespaceId),
    Composition(CompositionId),
}

// --- Commands ---

pub struct WriteChunkRequest {
    pub chunk_id: ChunkId,
    pub envelope: Envelope,
    pub target_pool: AffinityPoolId,
    pub tenant_id: OrgId,
}

/// Idempotent: if chunk exists, increments refcount.
/// Spec: A-B1
pub struct WriteChunkResponse {
    pub chunk_id: ChunkId,
    pub was_dedup: bool,
    pub new_refcount: u64,
}

pub struct ReadChunkRequest {
    pub chunk_id: ChunkId,
}

pub struct ReadChunkResponse {
    pub envelope: Envelope,
    pub meta: EnvelopeMeta,
}

pub struct IncrementRefcountRequest {
    pub chunk_id: ChunkId,
    pub tenant_id: OrgId,
}

pub struct DecrementRefcountRequest {
    pub chunk_id: ChunkId,
    pub tenant_id: OrgId,
}

pub struct RepairChunkRequest {
    pub chunk_id: ChunkId,
    pub trigger: RepairTrigger,
}

pub enum RepairTrigger {
    DeviceFailure(DeviceId),
    IntegrityCheck,
    AdminTriggered,
}

// --- Trait stubs ---

pub trait ChunkOps {
    /// Idempotent write — dedup if chunk exists.
    fn write_chunk(&self, req: WriteChunkRequest) -> Result<WriteChunkResponse, KisekiError>;
    fn read_chunk(&self, req: ReadChunkRequest) -> Result<ReadChunkResponse, KisekiError>;
    fn increment_refcount(&self, req: IncrementRefcountRequest) -> Result<u64, KisekiError>;
    fn decrement_refcount(&self, req: DecrementRefcountRequest) -> Result<u64, KisekiError>;
    fn repair_chunk(&self, req: RepairChunkRequest) -> Result<(), KisekiError>;
    fn set_retention_hold(&self, hold: RetentionHold) -> Result<(), KisekiError>;
    fn release_retention_hold(&self, hold_id: RetentionHoldId) -> Result<(), KisekiError>;
    fn chunk_health(&self, chunk_id: ChunkId) -> Result<ChunkMeta, KisekiError>;
}
