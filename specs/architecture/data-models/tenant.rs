//! Tenant and control plane types — Go data models represented as protobuf-compatible structs.
//! These are the Rust-side view of types owned by the Go control plane.
//! Spec: domain-model.md#ControlPlane, features/control-plane.feature

use crate::common::*;

// --- Tenant hierarchy ---

/// Spec: ubiquitous-language.md#Organization
pub struct Organization {
    pub org_id: OrgId,
    pub name: String,
    pub compliance_tags: Vec<ComplianceTag>,
    pub quota: Quota,
    pub used: QuotaUsage,
    pub dedup_policy: DedupPolicy,
    pub kms_config: Option<TenantKmsConfig>,
    pub created_at: WallTime,
}

use crate::key::TenantKmsConfig;

/// Spec: ubiquitous-language.md#Project
pub struct Project {
    pub project_id: ProjectId,
    pub org_id: OrgId,
    pub name: String,
    pub compliance_tags: Vec<ComplianceTag>,
    pub quota: Quota,
    pub used: QuotaUsage,
    pub created_at: WallTime,
}

/// Spec: ubiquitous-language.md#Workload
pub struct Workload {
    pub workload_id: WorkloadId,
    pub org_id: OrgId,
    pub project_id: Option<ProjectId>,
    pub name: String,
    pub quota: Quota,
    pub used: QuotaUsage,
    pub created_at: WallTime,
}

pub struct QuotaUsage {
    pub capacity_bytes: u64,
    pub iops_current: u64,
    pub metadata_ops_current: u64,
}

// --- Flavor ---

/// Spec: ubiquitous-language.md#Flavor
pub struct Flavor {
    pub flavor_id: uuid::Uuid,
    pub name: String,
    pub protocol: Vec<FlavorProtocol>,
    pub transport: Vec<FlavorTransport>,
    pub topology: FlavorTopology,
}

pub enum FlavorProtocol { Nfs, S3 }
pub enum FlavorTransport { Tcp, Cxi, RdmaVerbs }
pub enum FlavorTopology { Dedicated, Shared, Hyperconverged }

/// Result of best-fit flavor matching.
pub struct FlavorMatchResult {
    pub requested: Flavor,
    pub provided: Flavor,
    pub mismatches: Vec<String>,
}

// --- Access requests (zero-trust boundary) ---

/// Cluster admin requesting access to tenant data.
/// Spec: I-T4, features/control-plane.feature#IAM
pub struct AccessRequest {
    pub request_id: uuid::Uuid,
    pub requester: String,
    pub tenant_id: OrgId,
    pub scope: AccessScope,
    pub duration_hours: u32,
    pub access_level: AccessLevel,
    pub status: AccessRequestStatus,
    pub requested_at: WallTime,
}

pub enum AccessScope {
    Namespace(NamespaceId),
    Tenant(OrgId),
}

pub enum AccessLevel {
    ReadOnly,
    ReadWrite,
}

pub enum AccessRequestStatus {
    Pending,
    Approved { approved_by: String, expires_at: WallTime },
    Denied { denied_by: String, reason: String },
    Expired,
}

// --- Federation ---

/// Spec: ubiquitous-language.md#FederationPeer
pub struct FederationPeer {
    pub peer_id: uuid::Uuid,
    pub site_name: String,
    pub endpoint: String,
    pub replication_mode: ReplicationMode,
    pub status: FederationStatus,
}

pub enum ReplicationMode {
    Async,
}

pub enum FederationStatus {
    Active,
    Unreachable { since: WallTime },
    ConfigSyncLag { behind_seconds: u64 },
}

// --- Audit export ---

/// Spec: ubiquitous-language.md#TenantAuditExport, I-A2
pub struct AuditExportConfig {
    pub tenant_id: OrgId,
    /// Delivery endpoint on tenant VLAN
    pub export_endpoint: String,
    /// Which system events to include for coherent audit trail
    pub include_system_events: Vec<SystemEventFilter>,
}

pub enum SystemEventFilter {
    ShardEvents,
    NodeEvents,
    MaintenanceEvents,
    SecurityEvents,
}

// --- Authentication ---

/// Spec: I-Auth1 through I-Auth4
pub struct TenantCertificate {
    pub cert_fingerprint: [u8; 32],
    pub tenant_id: OrgId,
    pub issued_at: WallTime,
    pub expires_at: WallTime,
    pub revoked: bool,
}

pub struct ClusterCa {
    pub ca_fingerprint: [u8; 32],
    pub created_at: WallTime,
}
