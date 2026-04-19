//! Raft node identity.

use serde::{Deserialize, Serialize};

/// A Kiseki Raft node — carries the gRPC address for Raft transport.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KisekiNode {
    /// gRPC address for Raft RPCs (e.g., `"192.168.1.10:9102"`).
    pub addr: String,
}

impl KisekiNode {
    /// Create a new node.
    #[must_use]
    pub fn new(addr: &str) -> Self {
        Self {
            addr: addr.to_owned(),
        }
    }
}

impl std::fmt::Display for KisekiNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KisekiNode({})", self.addr)
    }
}
