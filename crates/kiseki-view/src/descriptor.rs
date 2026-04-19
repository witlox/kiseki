//! View descriptor — declares the view's shape and behavior.

use kiseki_common::ids::{OrgId, ShardId, ViewId};

/// Protocol semantics for the view.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProtocolSemantics {
    /// POSIX filesystem semantics.
    Posix,
    /// S3 object semantics.
    S3,
}

/// Consistency model for the view.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ConsistencyModel {
    /// Read-your-writes — strong consistency for the writing session.
    ReadYourWrites,
    /// Bounded staleness — reads may lag by up to `max_staleness_ms`.
    BoundedStaleness {
        /// Maximum allowed lag in milliseconds.
        max_staleness_ms: u64,
    },
    /// Eventual consistency.
    Eventual,
}

/// Declarative specification of a view's shape and behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewDescriptor {
    /// View identifier.
    pub view_id: ViewId,
    /// Owning tenant.
    pub tenant_id: OrgId,
    /// Source shards this view materializes from.
    pub source_shards: Vec<ShardId>,
    /// Protocol semantics.
    pub protocol: ProtocolSemantics,
    /// Consistency model.
    pub consistency: ConsistencyModel,
    /// Whether the view can be dropped and rebuilt from the log.
    pub discardable: bool,
    /// Descriptor version (immutable per version — changes create a new version).
    pub version: u64,
}
