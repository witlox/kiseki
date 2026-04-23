//! Federation peer management.
//!
//! Multiple Kiseki sites replicate tenant config and discovery metadata
//! asynchronously. Data replication carries ciphertext only.
//!
//! Spec: `ubiquitous-language.md#Federation`, I-F1.
//! Scenarios: `control-plane.feature` — Register federation peer,
//!   Data residency enforcement in federation, Tenant config sync.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::ControlError;

/// A federated peer cluster.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FederationPeer {
    /// Unique peer identifier.
    pub peer_id: String,
    /// Endpoint URL for the peer site.
    pub endpoint: String,
    /// Geographic region (e.g., "eu-west-1", "ch-zurich").
    pub region: String,
    /// Current status of the peer.
    pub status: PeerStatus,
    /// Last successful sync timestamp (epoch millis).
    pub last_sync: Option<u64>,
    /// Replication mode ("async" or "sync").
    pub replication_mode: String,
    /// Whether config syncs between sites.
    pub config_sync: bool,
    /// Whether data replication carries ciphertext only.
    pub data_cipher_only: bool,
}

impl FederationPeer {
    /// Legacy accessor: returns `peer_id` (for BDD compat with old `site_id`).
    #[must_use]
    pub fn site_id(&self) -> &str {
        &self.peer_id
    }

    /// Legacy accessor: whether the peer is connected (Active or Syncing).
    #[must_use]
    pub fn connected(&self) -> bool {
        self.status == PeerStatus::Active || self.status == PeerStatus::Syncing
    }
}

/// Status of a federation peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PeerStatus {
    /// Peer is active and reachable.
    Active,
    /// Peer is currently syncing.
    Syncing,
    /// Peer is unreachable.
    Unreachable,
    /// Peer has been deregistered.
    Deregistered,
}

/// Federation-specific errors.
#[derive(Debug, thiserror::Error)]
pub enum FederationError {
    /// Peer is already registered.
    #[error("peer already registered: {0}")]
    PeerAlreadyRegistered(String),
    /// Peer not found.
    #[error("peer not found: {0}")]
    PeerNotFound(String),
    /// Data residency violation prevents the operation.
    #[error("data residency violation: {0}")]
    DataResidencyViolation(String),
}

impl From<FederationError> for ControlError {
    fn from(e: FederationError) -> Self {
        match e {
            FederationError::PeerAlreadyRegistered(id) => ControlError::AlreadyExists(id),
            FederationError::PeerNotFound(id) => ControlError::NotFound(id),
            FederationError::DataResidencyViolation(msg) => ControlError::Rejected(msg),
        }
    }
}

/// Federation peer registry — manages peer relationships.
pub struct FederationRegistry {
    peers: RwLock<HashMap<String, FederationPeer>>,
}

impl FederationRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            peers: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new federation peer.
    ///
    /// # Errors
    ///
    /// Returns `FederationError::PeerAlreadyRegistered` if a peer with
    /// the same `peer_id` is already registered.
    pub fn register_peer(&self, peer: FederationPeer) -> Result<(), FederationError> {
        let mut peers = self.peers.write().expect("federation lock poisoned");
        if peers.contains_key(&peer.peer_id) {
            return Err(FederationError::PeerAlreadyRegistered(peer.peer_id));
        }
        peers.insert(peer.peer_id.clone(), peer);
        Ok(())
    }

    /// Deregister a peer by marking it `Deregistered`.
    ///
    /// # Errors
    ///
    /// Returns `FederationError::PeerNotFound` if the peer does not exist.
    pub fn deregister_peer(&self, peer_id: &str) -> Result<(), FederationError> {
        let mut peers = self.peers.write().expect("federation lock poisoned");
        let peer = peers
            .get_mut(peer_id)
            .ok_or_else(|| FederationError::PeerNotFound(peer_id.to_owned()))?;
        peer.status = PeerStatus::Deregistered;
        Ok(())
    }

    /// Look up a peer by ID.
    #[must_use]
    pub fn get_peer(&self, peer_id: &str) -> Option<FederationPeer> {
        let peers = self.peers.read().expect("federation lock poisoned");
        peers.get(peer_id).cloned()
    }

    /// List all registered peers.
    #[must_use]
    pub fn list_peers(&self) -> Vec<FederationPeer> {
        let peers = self.peers.read().expect("federation lock poisoned");
        peers.values().cloned().collect()
    }

    /// Mark a peer as unreachable.
    pub fn mark_unreachable(&self, peer_id: &str) {
        let mut peers = self.peers.write().expect("federation lock poisoned");
        if let Some(peer) = peers.get_mut(peer_id) {
            peer.status = PeerStatus::Unreachable;
        }
    }

    /// Mark a peer as syncing.
    pub fn mark_syncing(&self, peer_id: &str) {
        let mut peers = self.peers.write().expect("federation lock poisoned");
        if let Some(peer) = peers.get_mut(peer_id) {
            peer.status = PeerStatus::Syncing;
        }
    }

    /// Count of active (non-deregistered, non-unreachable) peers.
    #[must_use]
    pub fn active_count(&self) -> usize {
        let peers = self.peers.read().expect("federation lock poisoned");
        peers
            .values()
            .filter(|p| p.status == PeerStatus::Active || p.status == PeerStatus::Syncing)
            .count()
    }

    // --- Legacy compatibility helpers (used by existing BDD steps) ---

    /// Register or update a federation peer (legacy API).
    ///
    /// Unlike `register_peer`, this upserts and always sets the peer
    /// as connected/active.
    pub fn register(&self, peer: Peer) -> Result<(), ControlError> {
        let federation_peer = FederationPeer {
            peer_id: peer.site_id,
            endpoint: peer.endpoint,
            region: String::new(),
            status: PeerStatus::Active,
            last_sync: None,
            replication_mode: peer.replication_mode,
            config_sync: peer.config_sync,
            data_cipher_only: peer.data_cipher_only,
        };
        let mut peers = self.peers.write().expect("federation lock poisoned");
        peers.insert(federation_peer.peer_id.clone(), federation_peer);
        Ok(())
    }

    /// Check if a site is connected (legacy API).
    #[must_use]
    pub fn is_connected(&self, site_id: &str) -> bool {
        let peers = self.peers.read().expect("federation lock poisoned");
        peers
            .get(site_id)
            .is_some_and(|p| p.status == PeerStatus::Active || p.status == PeerStatus::Syncing)
    }
}

impl Default for FederationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Legacy peer type — kept for backward compatibility with existing code.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer(id: &str) -> FederationPeer {
        FederationPeer {
            peer_id: id.to_owned(),
            endpoint: format!("https://{id}.example.com:8443"),
            region: "eu-west-1".into(),
            status: PeerStatus::Active,
            last_sync: None,
            replication_mode: "async".into(),
            config_sync: true,
            data_cipher_only: true,
        }
    }

    #[test]
    fn register_peer_success() {
        let registry = FederationRegistry::new();
        let peer = test_peer("site-ch");

        registry.register_peer(peer).unwrap();
        assert_eq!(registry.list_peers().len(), 1);

        let retrieved = registry.get_peer("site-ch").unwrap();
        assert_eq!(retrieved.peer_id, "site-ch");
        assert_eq!(retrieved.status, PeerStatus::Active);
    }

    #[test]
    fn duplicate_registration_fails() {
        let registry = FederationRegistry::new();
        registry.register_peer(test_peer("site-eu")).unwrap();

        let result = registry.register_peer(test_peer("site-eu"));
        assert!(matches!(
            result,
            Err(FederationError::PeerAlreadyRegistered(_))
        ));
    }

    #[test]
    fn deregister_peer_success() {
        let registry = FederationRegistry::new();
        registry.register_peer(test_peer("site-eu")).unwrap();

        registry.deregister_peer("site-eu").unwrap();
        let peer = registry.get_peer("site-eu").unwrap();
        assert_eq!(peer.status, PeerStatus::Deregistered);
    }

    #[test]
    fn deregister_nonexistent_peer_fails() {
        let registry = FederationRegistry::new();
        let result = registry.deregister_peer("ghost");
        assert!(matches!(result, Err(FederationError::PeerNotFound(_))));
    }

    #[test]
    fn list_peers_returns_all() {
        let registry = FederationRegistry::new();
        registry.register_peer(test_peer("site-a")).unwrap();
        registry.register_peer(test_peer("site-b")).unwrap();
        registry.register_peer(test_peer("site-c")).unwrap();

        let peers = registry.list_peers();
        assert_eq!(peers.len(), 3);
    }

    #[test]
    fn mark_unreachable_changes_status() {
        let registry = FederationRegistry::new();
        registry.register_peer(test_peer("site-eu")).unwrap();

        registry.mark_unreachable("site-eu");
        let peer = registry.get_peer("site-eu").unwrap();
        assert_eq!(peer.status, PeerStatus::Unreachable);
    }

    #[test]
    fn mark_syncing_changes_status() {
        let registry = FederationRegistry::new();
        registry.register_peer(test_peer("site-eu")).unwrap();

        registry.mark_syncing("site-eu");
        let peer = registry.get_peer("site-eu").unwrap();
        assert_eq!(peer.status, PeerStatus::Syncing);
    }

    #[test]
    fn active_count_excludes_unreachable_and_deregistered() {
        let registry = FederationRegistry::new();
        registry.register_peer(test_peer("site-a")).unwrap();
        registry.register_peer(test_peer("site-b")).unwrap();
        registry.register_peer(test_peer("site-c")).unwrap();

        assert_eq!(registry.active_count(), 3);

        registry.mark_unreachable("site-a");
        assert_eq!(registry.active_count(), 2);

        registry.deregister_peer("site-b").unwrap();
        assert_eq!(registry.active_count(), 1);

        // Syncing still counts as active.
        registry.mark_syncing("site-c");
        assert_eq!(registry.active_count(), 1);
    }

    #[test]
    fn legacy_register_api_works() {
        let registry = FederationRegistry::new();
        let peer = Peer {
            site_id: "site-ch".into(),
            endpoint: "https://site-ch.example.com".into(),
            connected: true,
            replication_mode: "async".into(),
            config_sync: true,
            data_cipher_only: true,
        };
        registry.register(peer).unwrap();
        assert!(registry.is_connected("site-ch"));
    }
}
