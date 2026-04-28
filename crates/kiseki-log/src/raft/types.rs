//! openraft type configuration for the Log context.

use std::io::Cursor;

use kiseki_raft::KisekiNode;
use serde::{Deserialize, Serialize};

use crate::raft_store::LogCommand;

/// Response from applying a log command through Raft.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LogResponse {
    /// Delta appended with this sequence number.
    Appended(u64),
    /// Command completed.
    Ok,
    /// Phase 16c: a `DecrementChunkRefcount` apply observed a refcount
    /// transition. `true` = the entry just tombstoned (refcount hit 0)
    /// and the leader should fan `DeleteFragment` out to the placement
    /// list. `false` = decremented but refcount > 0.
    DecrementOutcome(bool),
}

impl std::fmt::Display for LogResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Appended(seq) => write!(f, "Appended({seq})"),
            Self::Ok => write!(f, "Ok"),
            Self::DecrementOutcome(tomb) => write!(f, "DecrementOutcome({tomb})"),
        }
    }
}

openraft::declare_raft_types!(
    /// Raft type configuration for Log shards.
    pub LogTypeConfig:
        D = LogCommand,
        R = LogResponse,
        NodeId = u64,
        Node = KisekiNode,
        SnapshotData = Cursor<Vec<u8>>,
);
