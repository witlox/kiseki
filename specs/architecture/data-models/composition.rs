//! Composition context types — namespace, chunk assembly, refcounting.
//! Spec: domain-model.md#Composition, features/composition.feature

use crate::common::*;
use crate::log::*;
use crate::chunk::*;

// --- Composition ---

/// A tenant-scoped metadata structure describing how chunks assemble
/// into a data unit (file, object). Reconstructed from shard deltas.
/// Spec: ubiquitous-language.md#Composition, I-X1, I-X3
pub struct Composition {
    pub composition_id: CompositionId,
    pub namespace_id: NamespaceId,
    pub tenant_id: OrgId,
    /// Ordered list of chunk references forming this data unit
    pub chunks: Vec<ChunkRef>,
    /// Total logical size of the assembled data
    pub logical_size: u64,
    /// Current version (log position of last mutation)
    pub version: SequenceNumber,
    pub created_at: DeltaTimestamp,
    pub modified_at: DeltaTimestamp,
    /// Object versioning: previous versions if enabled
    pub version_history: Vec<CompositionVersion>,
}

pub struct ChunkRef {
    pub chunk_id: ChunkId,
    pub offset: u64,
    pub length: u64,
}

pub struct CompositionVersion {
    pub version_id: uuid::Uuid,
    pub sequence: SequenceNumber,
    pub chunks: Vec<ChunkRef>,
    pub logical_size: u64,
    pub created_at: DeltaTimestamp,
}

// --- Namespace ---

/// Tenant-scoped collection of compositions within a shard.
/// Spec: ubiquitous-language.md#Namespace
pub struct Namespace {
    pub namespace_id: NamespaceId,
    pub tenant_id: OrgId,
    pub shard_id: ShardId,
    pub name: String,
    /// Inherited + own compliance tags (union of constraints)
    pub compliance_tags: Vec<ComplianceTag>,
    /// Object versioning enabled for this namespace
    pub versioning_enabled: bool,
    /// Read-only flag
    pub read_only: bool,
    pub created_at: DeltaTimestamp,
}

// --- Inline data ---

/// Threshold for inline data in delta payloads.
/// Spec: ADR-006
pub const INLINE_DATA_THRESHOLD: usize = 4096; // 4KB default

// --- Multipart upload state ---

/// Tracks in-progress multipart uploads before finalization.
/// Spec: I-L5 (not visible to readers until finalize)
pub struct MultipartUpload {
    pub upload_id: uuid::Uuid,
    pub composition_id: CompositionId,
    pub namespace_id: NamespaceId,
    pub tenant_id: OrgId,
    pub parts: Vec<MultipartPart>,
    pub started_at: DeltaTimestamp,
}

pub struct MultipartPart {
    pub part_number: u32,
    pub chunk_ids: Vec<ChunkId>,
    pub size: u64,
    pub stored: bool,
}

// --- Commands ---

pub struct CreateCompositionRequest {
    pub namespace_id: NamespaceId,
    pub tenant_id: OrgId,
    /// Encrypted name (in delta payload)
    pub name_encrypted: Vec<u8>,
    /// Chunks to reference
    pub chunks: Vec<ChunkRef>,
    /// Inline data (if below threshold, no chunks needed)
    pub inline_data: Option<Vec<u8>>,
}

pub struct UpdateCompositionRequest {
    pub composition_id: CompositionId,
    /// New chunks to add or replace
    pub chunk_mutations: Vec<ChunkMutation>,
}

pub enum ChunkMutation {
    Append(ChunkRef),
    Replace { old: ChunkId, new: ChunkRef },
    Truncate { new_size: u64 },
}

pub struct DeleteCompositionRequest {
    pub composition_id: CompositionId,
    /// If versioning enabled: creates delete marker. If not: tombstone.
    pub version_aware: bool,
}

pub struct FinalizeMultipartRequest {
    pub upload_id: uuid::Uuid,
    pub parts: Vec<u32>,
}

// --- Trait stubs ---

pub trait CompositionOps {
    fn create(&self, req: CreateCompositionRequest) -> Result<CompositionId, KisekiError>;
    fn update(&self, req: UpdateCompositionRequest) -> Result<SequenceNumber, KisekiError>;
    fn delete(&self, req: DeleteCompositionRequest) -> Result<(), KisekiError>;
    fn get(&self, id: CompositionId, at: Option<SequenceNumber>) -> Result<Composition, KisekiError>;
    fn list_namespace(&self, ns: NamespaceId, prefix: Option<&str>) -> Result<Vec<Composition>, KisekiError>;
    fn start_multipart(&self, ns: NamespaceId, tenant: OrgId) -> Result<MultipartUpload, KisekiError>;
    fn finalize_multipart(&self, req: FinalizeMultipartRequest) -> Result<CompositionId, KisekiError>;
    fn abort_multipart(&self, upload_id: uuid::Uuid) -> Result<(), KisekiError>;
    fn list_versions(&self, id: CompositionId) -> Result<Vec<CompositionVersion>, KisekiError>;
}
