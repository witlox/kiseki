//! Workflow Advisory domain types and hot-path lookup contract.
//!
//! Types in this module split into two groups:
//!
//! 1. **Shared domain types** (re-exported from `kiseki-common`) —
//!    `WorkflowRef`, `OperationAdvisory`, and the enums it contains.
//!    These are the only advisory types that data-path crates ever see.
//!    They carry preferences expressed by the caller; the data path
//!    treats them as hints only (I-WA1, I-WA14).
//!
//! 2. **Advisory runtime types** (defined in `kiseki-advisory`) —
//!    `Workflow`, `PhaseRecord`, `HintBudgets`, `AdvisoryPolicy`,
//!    `AdvisoryRouter`, etc. Visible only to `kiseki-advisory` and the
//!    `kiseki-server` wiring code.
//!
//! Spec: ADR-020 (analyst), ADR-021 (architect),
//!       specs/features/workflow-advisory.feature,
//!       specs/invariants.md I-WA1..I-WA18.

use crate::common::*;
use crate::chunk::AffinityPoolId;

// =============================================================================
// 1. Shared domain types (defined in `kiseki-common`)
// =============================================================================

/// Opaque capability reference for a workflow. Pinned to a tenant-scoped
/// advisory registry. Mere knowledge grants no access; authority is the
/// caller's mTLS identity (I-WA3, I-WA10).
pub struct WorkflowRef(pub [u8; 16]);

/// Client ID pinned per native-client process. Generated as a CSPRNG draw
/// at process start (I-WA4).
pub struct ClientId(pub [u8; 16]);

/// Monotonic phase counter within a workflow. Strictly increasing.
/// (I-WA13).
pub struct PhaseId(pub u64);

/// One-shot workload profile declared at `DeclareWorkflow`. Allow-listed
/// by org → project → workload inheritance (I-WA7).
pub enum WorkloadProfile {
    AiTraining,
    AiInference,
    HpcCheckpoint,
    BatchEtl,
    Interactive,
}

/// Access pattern hint for a caller-owned composition.
pub enum AccessPattern {
    Sequential,
    Random,
    Strided,
    Broadcast,
}

/// QoS priority class for the caller's operations. Capped by policy
/// (I-WA14).
pub enum Priority {
    Interactive,
    Batch,
    Bulk,
}

/// Retention intent (GC urgency / EC scheme selection). Never bypasses a
/// retention hold (I-WA14).
pub enum RetentionIntent {
    Temp,
    Working,
    Final,
}

/// Dedup intent (bounded by tenant dedup policy I-X2).
pub enum DedupIntent {
    SharedEnsemble,
    PerRank,
    Default,
}

/// Tenant-scoped affinity preference. Data-path placement may ignore this
/// to satisfy I-C3/I-C4/I-C2b (I-WA9).
pub struct AffinityPreference {
    pub preferred_pool: Option<AffinityPoolId>,
    pub colocate_rack: Option<String>,
}

/// Bundle passed to each data-path operation. Every field is optional.
/// A call with all-None is equivalent to a call with no advisory at all
/// (I-WA1, I-WA2).
pub struct OperationAdvisory {
    pub workflow_ref: Option<WorkflowRef>,
    pub phase_id: Option<PhaseId>,
    pub access_pattern: Option<AccessPattern>,
    pub priority: Option<Priority>,
    pub affinity: Option<AffinityPreference>,
    pub retention_intent: Option<RetentionIntent>,
    pub dedup_intent: Option<DedupIntent>,
}

// =============================================================================
// 2. Advisory runtime types (defined in `kiseki-advisory`)
// =============================================================================

/// In-memory record per active workflow. Ephemeral — never durable on disk
/// outside the audit trail. GC'd on `End` or TTL (I-WA10).
pub struct Workflow {
    pub workflow_ref: WorkflowRef,
    pub tenant_scope: TenantScope,
    pub client_id: ClientId,
    /// Profile effective at DeclareWorkflow (prospective policy, I-WA18).
    pub profile_at_declare: WorkloadProfile,
    /// Budget snapshot at DeclareWorkflow.
    pub budgets_at_declare: HintBudgets,
    /// Ring buffer of last K phase records (default K=64, ADR-021 §9).
    pub phase_history: PhaseRing,
    pub current_phase_id: PhaseId,
    pub current_phase_tag: PhaseTag,
    pub created_at: WallTime,
    pub ttl_deadline: WallTime,
    /// mTLS identity captured at DeclareWorkflow; re-checked on every
    /// subsequent operation (I-WA3).
    pub bound_mtls_fingerprint: [u8; 32],
}

/// Opaque phase tag. Stored as-is on tenant audit exports; hashed for
/// cluster-admin exports (I-WA8, I-A3).
pub struct PhaseTag(pub String);

/// Ring buffer of `PhaseRecord`, size bounded by policy.
pub struct PhaseRing {
    pub capacity: usize,
    pub records: Vec<PhaseRecord>,
    pub summaries_written: u64,
}

/// One entry in the phase ring.
pub struct PhaseRecord {
    pub phase_id: PhaseId,
    pub tag_hash: [u8; 32],
    pub entered_at: WallTime,
    pub hints_accepted: u64,
    pub hints_rejected: u64,
}

/// Summary emitted to the tenant audit shard when a ring entry is evicted.
/// ADR-021 §9.
pub struct PhaseSummary {
    pub workflow_ref: WorkflowRef,
    pub from_phase_id: PhaseId,
    pub to_phase_id: PhaseId,
    pub total_hints_accepted: u64,
    pub total_hints_rejected: u64,
    pub duration_ms: u64,
}

/// Effective budgets for a workflow at declare time. Computed as the min
/// across (cluster, org, project, workload) ceilings by Control Plane
/// (I-WA7, I-WA17, I-WA16).
pub struct HintBudgets {
    pub hints_per_sec: u32,
    pub concurrent_workflows: u32,
    pub phases_per_workflow: u32,
    pub telemetry_subscribers: u32,
    pub declared_prefetch_bytes: u64,
    pub workflow_declares_per_sec: u32,
    pub max_prefetch_tuples_per_hint: u32,
    pub ttl_seconds_max: u32,
}

/// Policy effective for a scope. Fetched from Control Plane and cached.
/// Changes apply prospectively (I-WA18).
pub struct AdvisoryPolicy {
    pub allowed_profiles: Vec<WorkloadProfile>,
    pub allowed_priorities: Vec<Priority>,
    pub budgets: HintBudgets,
    pub state: AdvisoryState,
    /// k-anonymity minimum for aggregate telemetry (default 5).
    pub k_anonymity_minimum: u32,
}

pub enum AdvisoryState {
    Enabled,
    Draining,
    Disabled,
}

// =============================================================================
// Hot-path lookup contract (ADR-021 §3)
// =============================================================================

/// Read-only lookup surface exposed by `kiseki-advisory` to the data path.
/// Implementations must satisfy:
///   - bounded deadline (≤500 µs; default 200 µs) before returning None.
///   - no allocation in the happy path (snapshot read).
///   - no shared mutex with the advisory runtime's writer path.
/// Spec: ADR-021 §3, §4.
pub trait AdvisoryLookup: Send + Sync {
    /// Resolve a caller-supplied `WorkflowRef` (typically lifted from a
    /// data-path RPC header) into an `OperationAdvisory`. Returns `None`
    /// on miss, timeout, or advisory-disabled — all indistinguishable
    /// to the caller (I-WA2).
    fn lookup(&self, workflow_ref: &WorkflowRef) -> Option<OperationAdvisory>;
}

// =============================================================================
// Advisory emission contract (runtime-internal)
// =============================================================================

/// Emission surface used by `kiseki-advisory` to publish telemetry events
/// to subscribers. Implementations live in `kiseki-advisory` only.
pub trait TelemetryEmitter: Send + Sync {
    fn emit(&self, workflow_ref: &WorkflowRef, event: TelemetryEvent);
}

/// Opaque telemetry event shape — domain representation, serialized to
/// protobuf on the wire (advisory.proto). Caller-scoped (I-WA5).
pub enum TelemetryEvent {
    Backpressure { pool: AffinityPoolId, severity: BackpressureSeverity, retry_after: RetryAfterBucket },
    MaterializationLag { view_id: ViewId, lag_bucket: LagBucket },
    Locality { composition_id: CompositionId, entries: Vec<(u64, u64, LocalityClass)> },
    PrefetchEffectiveness { hit_rate: HeadroomBucket },
    QosHeadroom { headroom: HeadroomBucket },
    ShardSaturation { shard: ShardId, severity: BackpressureSeverity, retry_after: RetryAfterBucket },
    PinHeadroom { headroom: HeadroomBucket },
    OwnHotspot { composition_id: CompositionId, contention: HeadroomBucket },
    RepairDegraded { composition_id: CompositionId, severity: RepairSeverity },
}

pub enum BackpressureSeverity { Ok, Soft, Hard }
pub enum RetryAfterBucket     { Lt50ms, Ms50to250, Ms250to1000, S1to10, Gt10s }
pub enum LagBucket             { Lt100ms, Ms100to500, Ms500to2000, Ms2000to10000, Gt10s }
pub enum LocalityClass         { LocalNode, LocalRack, SamePool, Remote, Degraded }
pub enum HeadroomBucket        { Ample, Moderate, Tight, Exhausted }
pub enum RepairSeverity        { Advisory, Urgent }

// =============================================================================
// Advisory router (runtime-internal, top-level service type)
// =============================================================================

/// Root service. Owns the Workflow table, Effective-hints table, prefetch
/// ring, audit emitter, budget enforcer. Runs on its own tokio runtime
/// (ADR-021 §1). Exposed to `kiseki-server` for wiring via the trait below.
pub trait AdvisoryRouter: Send + Sync {
    /// Lifecycle.
    fn declare_workflow(
        &self,
        caller: &MtlsIdentity,
        client_id: ClientId,
        profile: WorkloadProfile,
        initial_phase_id: PhaseId,
        initial_phase_tag: PhaseTag,
        ttl_seconds: u32,
    ) -> Result<WorkflowRef, AdvisoryError>;

    fn end_workflow(
        &self,
        caller: &MtlsIdentity,
        workflow_ref: &WorkflowRef,
    ) -> Result<(), AdvisoryError>;

    fn phase_advance(
        &self,
        caller: &MtlsIdentity,
        workflow_ref: &WorkflowRef,
        next_phase_id: PhaseId,
        next_phase_tag: PhaseTag,
    ) -> Result<(), AdvisoryError>;

    /// Returns a read-only snapshot handle to this router's lookup cache.
    /// Used by `kiseki-server` to wire the data path.
    fn lookup_handle(&self) -> std::sync::Arc<dyn AdvisoryLookup>;
}

/// Caller identity extracted from the current request's mTLS context.
/// Re-validated on every advisory operation (I-WA3).
pub struct MtlsIdentity {
    pub peer_fingerprint: [u8; 32],
    pub tenant_scope: TenantScope,
    pub not_after: WallTime,
    pub revoked: bool,
}

// =============================================================================
// Errors (see error-taxonomy.md `kiseki-advisory` section)
// =============================================================================

pub enum AdvisoryError {
    AdvisoryDisabled,
    ProfileNotAllowed,
    PriorityNotAllowed,
    BudgetExceeded,
    DeclareRateExceeded,
    HintTooLarge,
    ForbiddenTargetField,
    PhaseNotMonotonic,
    ProfileRevoked,
    PriorityRevoked,
    RetentionPolicyConflict,
    CertRevoked,
    /// Unified for authorization-denied AND target-absent. The router
    /// MUST emit responses with identical code / payload / timing (I-WA6,
    /// ADR-021 §8).
    ScopeNotFound,
    AdvisoryUnavailable,
    PrefetchBudgetExceeded,
}
