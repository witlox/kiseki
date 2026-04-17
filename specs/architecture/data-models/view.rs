//! View Materialization context types — stream processors, view lifecycle.
//! Spec: domain-model.md#ViewMaterialization, features/view-materialization.feature

use crate::common::*;
use crate::log::SequenceNumber;

// --- View descriptor ---

/// Declarative spec of a view's shape and behavior.
/// Spec: ubiquitous-language.md#ViewDescriptor
pub struct ViewDescriptor {
    pub view_id: ViewId,
    pub tenant_id: OrgId,
    /// Which shards this view projects from
    pub source_shards: Vec<ShardId>,
    pub protocol: ProtocolSemantics,
    pub consistency: ConsistencyModel,
    /// Target affinity pool for materialized state
    pub affinity_pool: AffinityPoolId,
    /// Can be dropped and rebuilt from log
    pub discardable: bool,
    /// Descriptor version (incremented on update)
    pub version: u64,
    pub created_at: DeltaTimestamp,
}

pub enum ProtocolSemantics {
    Posix,
    S3,
}

/// Spec: I-V3, I-K9
pub enum ConsistencyModel {
    /// POSIX: read-your-writes, stream processor tracks log tip
    ReadYourWrites,
    /// Bounded staleness with configurable bound.
    /// Effective bound = max(this, compliance_floor).
    BoundedStaleness { max_staleness_ms: u64 },
    /// Eventual — no bound (used for analytics views)
    Eventual,
}

// --- Stream processor state ---

/// Per-view stream processor state.
/// Spec: domain-model.md#ViewMaterialization
pub struct StreamProcessorState {
    pub view_id: ViewId,
    pub tenant_id: OrgId,
    /// Last consumed delta position per source shard
    pub watermarks: Vec<(ShardId, SequenceNumber)>,
    /// Current descriptor version being applied
    pub descriptor_version: u64,
    pub status: StreamProcessorStatus,
    /// Cached tenant KEK for payload decryption
    pub has_cached_tenant_key: bool,
}

pub enum StreamProcessorStatus {
    Running,
    /// Behind staleness bound — alerts raised
    StalenessViolation { behind_ms: u64 },
    /// Cannot decrypt — tenant KMS unreachable, cache expired
    KeyUnavailable,
    /// Source shard unavailable — serving last known state
    SourceUnavailable(ShardId),
    /// Stopped (discarded or paused)
    Stopped,
    /// Rebuilding from log position 0
    Rebuilding { progress_pct: f32 },
}

// --- MVCC ---

/// Read pin for MVCC snapshots.
/// Spec: I-V4 (bounded TTL)
pub struct MvccReadPin {
    pub pin_id: uuid::Uuid,
    pub view_id: ViewId,
    pub position: SequenceNumber,
    pub created_at: WallTime,
    pub ttl_seconds: u32,
}

// --- Commands ---

pub struct CreateViewRequest {
    pub descriptor: ViewDescriptor,
}

pub struct DiscardViewRequest {
    pub view_id: ViewId,
    /// Requires tenant admin approval if not the tenant admin
    pub approved_by: OrgId,
}

pub struct UpdateDescriptorRequest {
    pub view_id: ViewId,
    pub new_descriptor: ViewDescriptor,
}

pub struct AcquirePinRequest {
    pub view_id: ViewId,
    pub ttl_seconds: u32,
}

pub struct AcquirePinResponse {
    pub pin: MvccReadPin,
}

// --- Trait stubs ---

pub trait ViewOps {
    fn create_view(&self, req: CreateViewRequest) -> Result<ViewId, KisekiError>;
    fn discard_view(&self, req: DiscardViewRequest) -> Result<(), KisekiError>;
    fn rebuild_view(&self, view_id: ViewId) -> Result<(), KisekiError>;
    fn update_descriptor(&self, req: UpdateDescriptorRequest) -> Result<(), KisekiError>;
    fn view_status(&self, view_id: ViewId) -> Result<StreamProcessorState, KisekiError>;
    fn acquire_pin(&self, req: AcquirePinRequest) -> Result<AcquirePinResponse, KisekiError>;
    fn release_pin(&self, pin_id: uuid::Uuid) -> Result<(), KisekiError>;
}

use crate::chunk::AffinityPoolId;
