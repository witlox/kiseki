//! Federation peer management.
//!
//! Multiple Kiseki sites replicate tenant config and discovery metadata
//! asynchronously. Data replication carries ciphertext only.
//!
//! Spec: `ubiquitous-language.md#Federation`, I-F1.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::ControlError;

/// A federated site.
#[derive(Clone, Debug)]
pub struct Peer {
    /// Site identifier.
    pub site_id: String,
    /// Endpoint URL.
    pub endpoint: String,
    /// Whether the peer is connected.
    pub connected: bool,
    /// Replication mode ("async" or "sync").
    pub replication_mode: String,
    /// Whether config syncs between sites.
    pub config_sync: bool,
    /// Whether data replication carries ciphertext only.
    pub data_cipher_only: bool,
}

/// Federation peer registry.
pub struct FederationRegistry {
    peers: RwLock<HashMap<String, Peer>>,
}

impl FederationRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            peers: RwLock::new(HashMap::new()),
        }
    }

    /// Register or update a federation peer.
    pub fn register(&self, mut peer: Peer) -> Result<(), ControlError> {
        if peer.site_id.is_empty() {
            return Err(ControlError::Rejected("site ID required".into()));
        }
        peer.connected = true;
        let mut peers = self.peers.write().unwrap();
        peers.insert(peer.site_id.clone(), peer);
        Ok(())
    }

    /// List all registered peers.
    #[must_use]
    pub fn list_peers(&self) -> Vec<Peer> {
        let peers = self.peers.read().unwrap();
        peers.values().cloned().collect()
    }

    /// Check if a site is connected.
    #[must_use]
    pub fn is_connected(&self, site_id: &str) -> bool {
        let peers = self.peers.read().unwrap();
        peers.get(site_id).is_some_and(|p| p.connected)
    }
}

impl Default for FederationRegistry {
    fn default() -> Self {
        Self::new()
    }
}
