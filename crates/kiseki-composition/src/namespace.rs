//! Namespace management.
//!
//! A namespace is a tenant-scoped collection of compositions within a shard.

use kiseki_common::ids::{NamespaceId, OrgId, ShardId};

/// Compliance regime tag for a namespace or org.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum ComplianceTag {
    /// US health data.
    Hipaa,
    /// EU General Data Protection Regulation.
    Gdpr,
    /// Swiss Federal Act on Data Protection (revised).
    RevFadp,
    /// Custom compliance tag.
    Custom(String),
}

/// One tier of a namespace's placement policy (ADR-045 §D3): a device
/// class plus an optional logical quota. The class name is the
/// chunk-store pool string (`fast` / `bulk` / `cold`) that
/// `kiseki_chunk::tier_for_pool` maps to a `StorageTier`; kept as a
/// `String` here so `kiseki-composition` needs no `kiseki-block` dep.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TierQuota {
    /// Device-class tier name (`fast` / `bulk` / `cold`).
    pub tier: String,
    /// Logical quota in bytes; `0` = unbounded.
    pub quota_bytes: u64,
}

/// Per-namespace size-band pool selector (ADR-024 2026-05-31 amendment
/// cross-referenced from ADR-045 §"three-tier durability"). Names the
/// pool that should receive each size band's writes. Empty fields fall
/// through to the cluster default chain
/// `[inline, replicated, ec]` resolved against the highest-class
/// pools available; explicit overrides win.
///
/// Stored on `Namespace` alongside the ADR-045 `tier_policy`
/// (device-class spill order). The two axes are orthogonal:
/// `tier_policy` chooses *which device class* a chunk lands on;
/// `size_band_pools` chooses *which durability strategy* the write
/// uses (inline / replication / EC).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NamespaceSizeBandPools {
    /// Pool name for the inline band (size ≤ `inline_threshold`).
    /// `None` → cluster default inline pool.
    pub inline: Option<String>,
    /// Pool name for the replicated band
    /// (`inline_threshold` < size ≤ `replication_ceiling`).
    pub replicated: Option<String>,
    /// Pool name for the EC band (size > `replication_ceiling`).
    pub ec: Option<String>,
}

impl NamespaceSizeBandPools {
    /// True if none of the three bands has an explicit pool name set —
    /// the namespace inherits the cluster default chain.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.inline.is_none() && self.replicated.is_none() && self.ec.is_none()
    }
}

/// A namespace within a shard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Namespace {
    /// Namespace identifier.
    pub id: NamespaceId,
    /// Owning tenant.
    pub tenant_id: OrgId,
    /// Shard this namespace lives in.
    pub shard_id: ShardId,
    /// Whether the namespace is read-only.
    pub read_only: bool,
    /// Whether object versioning is enabled (delete creates tombstone).
    pub versioning_enabled: bool,
    /// Compliance tags applied at the namespace level.
    pub compliance_tags: Vec<ComplianceTag>,
    /// Placement-tier policy (ADR-045 §D3). Empty = the default
    /// fastest-fit behavior. When non-empty, the first entry is the
    /// preferred tier and the order is the spill order; each carries an
    /// optional per-tier logical quota.
    pub tier_policy: Vec<TierQuota>,
    /// Per-size-band pool selector (ADR-024 amendment cross-referenced
    /// from ADR-045). Drives `select_pool_for_write` in the gateway
    /// PUT path. Empty (default) inherits the cluster default chain.
    pub size_band_pools: NamespaceSizeBandPools,
}

impl Namespace {
    /// Effective compliance tags: org-level merged with namespace-level.
    /// Returns a sorted, deduplicated set.
    #[must_use]
    pub fn effective_compliance_tags(&self, org_tags: &[ComplianceTag]) -> Vec<ComplianceTag> {
        let mut tags: Vec<ComplianceTag> = org_tags
            .iter()
            .chain(self.compliance_tags.iter())
            .cloned()
            .collect();
        tags.sort();
        tags.dedup();
        tags
    }
}
