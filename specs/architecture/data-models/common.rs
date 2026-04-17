//! Common types shared across all Kiseki crates.
//! No method bodies — architecture stubs only.

use std::fmt;

// --- Time (dual clock model, adapted from taba) ---

/// Hybrid Logical Clock — authoritative for ordering and causality.
/// Combines physical time with logical counter for causal tiebreaking.
/// Spec: I-T5, I-T7, ubiquitous-language.md#Time
pub struct HybridLogicalClock {
    /// Milliseconds since Unix epoch (physical component)
    pub physical_ms: u64,
    /// Logical counter for causal ordering within same millisecond
    pub logical: u32,
    /// Node that produced this timestamp
    pub node_id: NodeId,
}

/// Wall clock — authoritative only for duration-based policies.
/// Spec: I-T5, ubiquitous-language.md#Time
pub struct WallTime {
    pub millis_since_epoch: u64,
    pub timezone: String,
}

/// Self-reported clock quality per node.
/// Spec: I-T6
pub enum ClockQuality {
    Ntp,
    Ptp,
    Gps,
    Unsync,
}

/// Combined timestamp on every delta and event.
/// Spec: ubiquitous-language.md#DeltaTimestamp
pub struct DeltaTimestamp {
    pub hlc: HybridLogicalClock,
    pub wall: WallTime,
    pub quality: ClockQuality,
}

// --- Identifiers ---

/// Opaque node identifier within the cluster.
pub struct NodeId(pub u64);

/// Tenant organization identifier.
pub struct OrgId(pub uuid::Uuid);

/// Optional project within an org.
pub struct ProjectId(pub uuid::Uuid);

/// Workload identifier within a tenant.
pub struct WorkloadId(pub uuid::Uuid);

/// Shard identifier.
pub struct ShardId(pub uuid::Uuid);

/// Chunk identifier — sha256(plaintext) for default tenants,
/// HMAC(plaintext, tenant_key) for opted-out tenants.
/// Spec: I-K10, ubiquitous-language.md#Chunk
pub struct ChunkId(pub [u8; 32]);

/// Composition identifier.
pub struct CompositionId(pub uuid::Uuid);

/// Namespace identifier (always tenant-scoped).
pub struct NamespaceId(pub uuid::Uuid);

/// View identifier.
pub struct ViewId(pub uuid::Uuid);

/// Key epoch version marker.
/// Spec: ubiquitous-language.md#KeyEpoch
pub struct KeyEpoch(pub u64);

/// Sequence number within a shard (Raft-assigned, total order).
/// Spec: I-L1
pub struct SequenceNumber(pub u64);

// --- Tenant hierarchy ---

/// Tenant hierarchy: org → [project] → workload.
/// Spec: ubiquitous-language.md#Tenant, I-T1 through I-T4
pub enum TenantScope {
    Org(OrgId),
    Project(OrgId, ProjectId),
    Workload(OrgId, Option<ProjectId>, WorkloadId),
}

/// Compliance regime tag — attaches at any tenant level, inherits downward.
/// Spec: I-K9, assumptions.md#A-T2
pub enum ComplianceTag {
    Hipaa,
    Gdpr,
    RevFadp,
    SwissResidency,
    Custom(String),
}

/// Dedup policy per tenant.
/// Spec: I-K10, I-X2
pub enum DedupPolicy {
    /// sha256(plaintext) — cross-tenant dedup enabled (default)
    CrossTenant,
    /// HMAC(plaintext, tenant_key) — full isolation, no cross-tenant dedup
    TenantIsolated,
}

// --- Quota ---

/// Resource quotas, settable at org and workload levels.
/// Spec: I-T2, domain-model.md#ControlPlane
pub struct Quota {
    pub capacity_bytes: u64,
    pub iops: u64,
    pub metadata_ops_per_sec: u64,
}

// --- Errors (base) ---

/// Top-level error category. Each crate extends with context-specific variants.
/// Spec: will be detailed in error-taxonomy.md
pub enum KisekiError {
    /// Retriable — caller should back off and retry
    Retriable(RetriableError),
    /// Permanent — operation cannot succeed
    Permanent(PermanentError),
    /// Security — access denied, authentication failure
    Security(SecurityError),
}

pub enum RetriableError {
    ShardUnavailable(ShardId),
    KeyManagerUnavailable,
    TenantKmsUnavailable(OrgId),
    QuorumLost(ShardId),
    MaintenanceMode(ShardId),
    QuotaExceeded(TenantScope),
}

pub enum PermanentError {
    ChunkLost(ChunkId),
    TenantKmsLost(OrgId),
    DataCorruption(ShardId),
    InvariantViolation(String),
}

pub enum SecurityError {
    AuthenticationFailed,
    TenantAccessDenied(OrgId),
    ClusterAdminAccessDenied,
    CryptoShredComplete(OrgId),
}
