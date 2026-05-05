//! openraft type configuration for the control-plane Raft group.

use std::io::Cursor;

use kiseki_common::ids::ShardId;
use kiseki_raft::KisekiNode;
use serde::{Deserialize, Serialize};

use super::commands::ControlCommand;

/// Response from applying a `ControlCommand` through Raft.
///
/// Most commands are book-keeping mutations of the namespace shard
/// map and return `Applied`. `NamespaceCreated` carries the post-
/// apply count so admin RPCs can echo the topology size back to the
/// caller without a follow-up read.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ControlResponse {
    /// Command applied. Used by `RecordSplit` / `RecordMerge` /
    /// `RetireShard` — they don't surface anything beyond success.
    Applied,
    /// Namespace created with the given shard count. Returned by
    /// `CreateNamespace` so the admin RPC can echo "created N
    /// shards" back to the caller in a single round trip.
    NamespaceCreated {
        /// Final shard count after apply.
        shard_count: u32,
    },
    /// `RecordSplit` was applied; the namespace map now contains the
    /// new shard. The id is echoed back so the admin RPC can return
    /// it to the caller (the same id that was supplied in the
    /// `ControlCommand` — but echoing it here saves the caller a
    /// re-read of the command they sent).
    SplitRecorded {
        /// The new shard's id.
        new_shard_id: ShardId,
    },
}

impl std::fmt::Display for ControlResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Applied => write!(f, "Applied"),
            Self::NamespaceCreated { shard_count } => {
                write!(f, "NamespaceCreated(shards={shard_count})")
            }
            Self::SplitRecorded { new_shard_id } => {
                write!(f, "SplitRecorded(new={:?})", new_shard_id.0)
            }
        }
    }
}

openraft::declare_raft_types!(
    /// Raft type configuration for the control-plane group.
    pub ControlTypeConfig:
        D = ControlCommand,
        R = ControlResponse,
        NodeId = u64,
        Node = KisekiNode,
        SnapshotData = Cursor<Vec<u8>>,
);

/// Constant `ShardId` for the control-plane Raft group. The group is
/// cluster-wide (one per cluster, not per data shard) but the
/// multiplexed Raft transport (ADR-041) requires every Raft group to
/// register under a `ShardId`. Pick a deterministic UUID derived from
/// the literal "kiseki-cluster-ctrl" so leader and followers agree
/// without configuration.
///
/// Distinct from `KEYMANAGER_RAFT_GROUP_ID` /
/// `AUDIT_RAFT_GROUP_ID` / `Uuid::from_u128(1)` (the bootstrap data
/// shard) so the multiplexed listener routes inbound RPCs to the
/// right state machine.
pub const CONTROL_RAFT_GROUP_ID: ShardId = ShardId(uuid::Uuid::from_u128(
    0x636c_7573_7465_725f_4374_726c_4772_7000_u128, // "cluster_CtrlGrp\0"
));
