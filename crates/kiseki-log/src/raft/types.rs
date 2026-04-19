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
}

impl std::fmt::Display for LogResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Appended(seq) => write!(f, "Appended({seq})"),
            Self::Ok => write!(f, "Ok"),
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
