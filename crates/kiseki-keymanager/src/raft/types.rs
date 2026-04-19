//! openraft type configuration for the key manager.

use std::io::Cursor;

use kiseki_raft::KisekiNode;
use serde::{Deserialize, Serialize};

use crate::raft_store::KeyCommand;

/// Response from applying a key command through Raft.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum KeyResponse {
    /// Epoch created or rotated to.
    Epoch(u64),
    /// Command completed successfully.
    Ok,
}

impl std::fmt::Display for KeyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Epoch(e) => write!(f, "Epoch({e})"),
            Self::Ok => write!(f, "Ok"),
        }
    }
}

openraft::declare_raft_types!(
    /// Raft type configuration for the key manager.
    pub KeyTypeConfig:
        D = KeyCommand,
        R = KeyResponse,
        NodeId = u64,
        Node = KisekiNode,
        SnapshotData = Cursor<Vec<u8>>,
);
