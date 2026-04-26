//! Raft node identity.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Where a node lives. Used by the placement planner so a shard's
/// 3-replica set can spread across failure domains (Phase 14f
/// decision row).
///
/// The variants escalate from coarse to fine:
/// - [`Topology::Rack`] — a single rack label. Matches a flat
///   datacenter or a small cluster.
/// - [`Topology::Zone`] — rack inside a zone. Matches multi-AZ
///   deployments where two replicas in the same zone share a single
///   power/network failure boundary.
/// - [`Topology::Custom`] — arbitrary key/value labels for operators
///   who need a topology Kiseki didn't anticipate (specific PDU,
///   rack-row, kernel-version, etc.).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Topology {
    /// Rack identifier (e.g. `"rack-7"`).
    Rack(String),
    /// Rack within a zone (e.g. `Zone { rack: "r3", zone: "us-east-1a" }`).
    Zone {
        /// Rack label inside the zone.
        rack: String,
        /// Zone label (typically a cloud AZ).
        zone: String,
    },
    /// Arbitrary labels for placement policies Kiseki doesn't model.
    Custom(HashMap<String, String>),
}

impl Topology {
    /// Coarsest failure domain — used by the placement planner to ask
    /// "would these two nodes both die in the same outage?".
    #[must_use]
    pub fn failure_domain(&self) -> String {
        match self {
            Self::Rack(r) => r.clone(),
            Self::Zone { zone, .. } => zone.clone(),
            Self::Custom(m) => m
                .get("zone")
                .or_else(|| m.get("rack"))
                .cloned()
                .unwrap_or_default(),
        }
    }
}

/// A Kiseki Raft node — carries the gRPC address for Raft transport
/// and an optional [`Topology`] hint for placement.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct KisekiNode {
    /// gRPC address for Raft RPCs (e.g., `"192.168.1.10:9102"`).
    pub addr: String,
    /// Where this node sits in the cluster topology. `None` = unknown
    /// (placement planner falls back to "any spread is fine").
    #[serde(default)]
    pub topology: Option<Topology>,
}

impl KisekiNode {
    /// Create a new node with no topology hint.
    #[must_use]
    pub fn new(addr: &str) -> Self {
        Self {
            addr: addr.to_owned(),
            topology: None,
        }
    }

    /// Attach a topology label. Builder-style for ergonomic
    /// construction in tests and operator code.
    #[must_use]
    pub fn with_topology(mut self, topology: Topology) -> Self {
        self.topology = Some(topology);
        self
    }
}

impl std::fmt::Display for KisekiNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KisekiNode({})", self.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_rack_failure_domain_is_rack() {
        assert_eq!(Topology::Rack("r1".into()).failure_domain(), "r1");
    }

    #[test]
    fn topology_zone_failure_domain_is_zone() {
        let t = Topology::Zone {
            rack: "r1".into(),
            zone: "us-east-1a".into(),
        };
        assert_eq!(t.failure_domain(), "us-east-1a");
    }

    #[test]
    fn topology_custom_prefers_zone_then_rack() {
        let mut m = HashMap::new();
        m.insert("zone".to_owned(), "az-2".to_owned());
        m.insert("rack".to_owned(), "r9".to_owned());
        assert_eq!(Topology::Custom(m).failure_domain(), "az-2");

        let mut m = HashMap::new();
        m.insert("rack".to_owned(), "r9".to_owned());
        assert_eq!(Topology::Custom(m).failure_domain(), "r9");
    }

    #[test]
    fn kiseki_node_serde_with_topology() {
        let n = KisekiNode::new("127.0.0.1:9100").with_topology(Topology::Rack("r1".into()));
        let bytes = serde_json::to_vec(&n).unwrap();
        let back: KisekiNode = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(n, back);
    }

    #[test]
    fn kiseki_node_back_compat_without_topology_field() {
        // Pre-Phase-14f wire format had no `topology` field. The
        // `#[serde(default)]` on the new field ensures we can still
        // decode that JSON.
        let json = br#"{"addr":"127.0.0.1:9100"}"#;
        let n: KisekiNode = serde_json::from_slice(json).unwrap();
        assert_eq!(n.addr, "127.0.0.1:9100");
        assert!(n.topology.is_none());
    }
}
