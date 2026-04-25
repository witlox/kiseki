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
