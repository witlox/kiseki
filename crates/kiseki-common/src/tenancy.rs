//! Tenant hierarchy, quotas, compliance tags, dedup policy, key epoch.
//!
//! Spec: `ubiquitous-language.md#Tenancy-and-access`, I-T1..I-T4, I-K9, I-K10,
//!       I-X2.

use crate::ids::{OrgId, ProjectId, WorkloadId};

/// Tenant hierarchy reference: `org → [project] → workload`. An
/// organization is the billing, admin, and master-key authority boundary;
/// a project is an optional grouping; a workload is the runtime isolation
/// unit.
///
/// Spec: `ubiquitous-language.md#Tenant`, I-T1, I-T3.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TenantScope {
    /// Whole organization.
    Org(OrgId),
    /// Project within an organization.
    Project(OrgId, ProjectId),
    /// Workload within an organization (with optional project parent).
    Workload(OrgId, Option<ProjectId>, WorkloadId),
}

impl TenantScope {
    /// The owning organization — always present regardless of depth.
    #[must_use]
    pub const fn org(&self) -> OrgId {
        match *self {
            Self::Org(o) | Self::Project(o, _) | Self::Workload(o, _, _) => o,
        }
    }
}

/// Compliance-regime tag. Attaches at any tenant level and inherits
/// downward. Effective constraint at a node is the union of constraints
/// implied by its own tags and those of its ancestors — so tags cannot
/// weaken inherited policy.
///
/// Spec: I-K9, `ubiquitous-language.md#ComplianceTag`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ComplianceTag {
    /// HIPAA §164.312 applies.
    Hipaa,
    /// GDPR applies.
    Gdpr,
    /// Swiss revFADP applies.
    RevFadp,
    /// Data residency constrained to Switzerland.
    SwissResidency,
    /// Tenant-defined tag (free-form; evaluated by policy engine).
    Custom(String),
}

/// Dedup policy per tenant.
///
/// Spec: I-K10, I-X2.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DedupPolicy {
    /// `sha256(plaintext)` — cross-tenant dedup enabled (default).
    CrossTenant,
    /// `HMAC(plaintext, tenant_key)` — tenant-isolated, no cross-tenant
    /// dedup possible, zero co-occurrence leak.
    TenantIsolated,
}

/// Resource quotas. Bounded at org; optionally narrowed at project;
/// ultimately enforced at workload.
///
/// Spec: I-T2.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Quota {
    /// Aggregate capacity ceiling, in bytes.
    pub capacity_bytes: u64,
    /// Aggregate IOPS ceiling.
    pub iops: u64,
    /// Aggregate metadata-ops/sec ceiling.
    pub metadata_ops_per_sec: u64,
}

/// Key epoch version marker for rotation.
///
/// Spec: `ubiquitous-language.md#KeyEpoch`, I-K6.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub struct KeyEpoch(pub u64);
