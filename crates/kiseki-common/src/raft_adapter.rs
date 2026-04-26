//! Raft membership adapter trait (ADR-035 §3 drain protocol).
//!
//! Bridges the control-plane drain orchestrator (`kiseki-control`) and
//! the Raft consensus layer (`kiseki-log`/`kiseki-raft`) without
//! requiring `kiseki-control` to depend on either crate. Operators
//! supply an adapter implementation; the orchestrator drives drains
//! through it.
//!
//! The trait is intentionally small — only the membership operations
//! the drain orchestrator actually needs. Snapshot transfer, log
//! replication, and leader election remain internal to the Raft
//! crates.

use std::future::Future;
use std::pin::Pin;

use crate::ids::NodeId;

/// Errors the orchestrator surfaces from membership operations.
#[derive(Debug, thiserror::Error)]
pub enum MembershipError {
    /// Underlying Raft layer rejected the operation.
    #[error("raft membership change failed: {0}")]
    Raft(String),
    /// No leader available to accept the change.
    #[error("no leader available")]
    NoLeader,
}

/// Future returned by adapter methods.
pub type MembershipFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, MembershipError>> + Send + 'a>>;

/// Membership operations the drain orchestrator needs.
///
/// Implementations live in the consensus crates (`kiseki-log` for the
/// in-process test cluster, `kiseki-raft` for the production
/// implementation). The orchestrator depends only on this trait.
pub trait RaftMembershipAdapter: Send + Sync {
    /// Add `replacement` as a learner so it can catch up before being
    /// promoted to a voter (I-N3 step 1).
    fn add_learner(&mut self, replacement: NodeId) -> MembershipFuture<'_, ()>;

    /// Replace `target`'s voter slot with `replacement` in the membership
    /// configuration. Implementations must keep the cluster at ≥ RF
    /// voters at every step (promote-then-remove, ADR-035 §3 phase 2).
    fn replace_voter(&mut self, target: NodeId, replacement: NodeId) -> MembershipFuture<'_, ()>;

    /// Current voter set as known to the leader. Used by the orchestrator
    /// to recompute the per-target shard list before each step (avoids
    /// the stale-`voter_in_shards` trap called out in the integrator
    /// review).
    fn voter_ids(&self) -> MembershipFuture<'_, Vec<NodeId>>;
}
