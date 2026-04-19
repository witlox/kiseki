//! openraft type configuration for the Audit context.

use crate::raft_store::AuditCommand;
use kiseki_raft::KisekiNode;
use serde::{Deserialize, Serialize};
use std::io::Cursor;

/// Response from applying an audit command.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AuditResponse {
    /// Event appended.
    Appended,
    /// Command completed.
    Ok,
}

impl std::fmt::Display for AuditResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Appended => write!(f, "Appended"),
            Self::Ok => write!(f, "Ok"),
        }
    }
}

openraft::declare_raft_types!(
    /// Raft type config for Audit shards.
    pub AuditTypeConfig:
        D = AuditCommand,
        R = AuditResponse,
        NodeId = u64,
        Node = KisekiNode,
        SnapshotData = Cursor<Vec<u8>>,
);
