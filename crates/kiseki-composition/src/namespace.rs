//! Namespace management.
//!
//! A namespace is a tenant-scoped collection of compositions within a shard.

use kiseki_common::ids::{NamespaceId, OrgId, ShardId};

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
}
