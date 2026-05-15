//! Fabric discovery (ADR-008).
//!
//! Native clients discover shards, views, and gateways via seed
//! endpoints on the data fabric. No control plane connectivity required.
//!
//! ADR-008 rev 2 — the bootstrap discovery response carries a
//! per-shard leader map projected from the control-plane
//! `NamespaceShardMap` (ADR-033 §4). Clients hydrate their
//! `TopologyCache` from this map on first connect, then refresh via
//! gRPC `GetTopology` (ADR-042 §4) once a data channel is dialled.

use std::net::SocketAddr;

/// A seed endpoint for discovery bootstrap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeedEndpoint {
    /// Address of the seed node.
    pub addr: SocketAddr,
}

/// Discovery response from a seed node.
#[derive(Clone, Debug)]
pub struct DiscoveryResponse {
    /// Available shards with leader info.
    pub shards: Vec<ShardEndpoint>,
    /// Available views.
    pub views: Vec<ViewEndpoint>,
    /// Available protocol gateways.
    pub gateways: Vec<GatewayEndpoint>,
    /// TTL for this discovery response (ms).
    pub ttl_ms: u64,
}

/// A shard endpoint from discovery.
#[derive(Clone, Debug)]
pub struct ShardEndpoint {
    /// Shard identifier (opaque string from discovery).
    pub shard_id: String,
    /// Owning namespace (ADR-008 rev 2). Empty for rev-1 stubs.
    pub namespace_id: String,
    /// Leader's `NodeId`. `None` when the responding node has not yet
    /// observed a leader (cold start, mid-election).
    pub leader_node_id: Option<u64>,
    /// Leader node address — the legacy single-binding form. ADR-008
    /// rev 2 retains this for compat; per-binding endpoints are
    /// resolved via the gRPC `GetTopology` follow-up (ADR-042 §1.7).
    pub leader_addr: SocketAddr,
    /// Hex-encoded 32-byte inclusive lower bound of the shard's
    /// hashed-key range (ADR-033 §4). Empty `Vec` for rev-1 stubs.
    pub range_start: Vec<u8>,
    /// Hex-encoded 32-byte exclusive upper bound. Empty `Vec` for
    /// rev-1 stubs.
    pub range_end: Vec<u8>,
}

/// A view endpoint from discovery.
#[derive(Clone, Debug)]
pub struct ViewEndpoint {
    /// View identifier (opaque string).
    pub view_id: String,
    /// Protocol (POSIX or S3).
    pub protocol: String,
    /// Endpoint address.
    pub endpoint: SocketAddr,
}

/// A gateway endpoint from discovery.
#[derive(Clone, Debug)]
pub struct GatewayEndpoint {
    /// Protocol (NFS, S3).
    pub protocol: String,
    /// Transport type.
    pub transport: String,
    /// Endpoint address.
    pub endpoint: SocketAddr,
}

/// Discovery client — queries seed endpoints for cluster topology.
///
/// Tries each seed endpoint in order until one responds. The response
/// contains shard leaders, view endpoints, and gateway addresses.
/// Clients cache the response for `ttl_ms` before re-querying.
pub struct DiscoveryClient {
    seeds: Vec<SeedEndpoint>,
    /// Cached discovery response.
    cached: Option<(DiscoveryResponse, std::time::Instant)>,
}

impl DiscoveryClient {
    /// Create a discovery client with the given seed endpoints.
    #[must_use]
    pub fn new(seeds: Vec<SeedEndpoint>) -> Self {
        Self {
            seeds,
            cached: None,
        }
    }

    /// Parse seed endpoints from a comma-separated string.
    ///
    /// Format: `"host1:port,host2:port,host3:port"`
    #[must_use]
    pub fn from_seed_string(s: &str) -> Self {
        let seeds = s
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(|addr_str| {
                addr_str
                    .trim()
                    .parse()
                    .ok()
                    .map(|addr| SeedEndpoint { addr })
            })
            .collect();
        Self::new(seeds)
    }

    /// Discover cluster topology. Returns cached response if still valid.
    ///
    /// Queries each seed endpoint in order until one responds.
    /// Returns `None` if all seeds are unreachable.
    pub fn discover(&mut self) -> Option<&DiscoveryResponse> {
        // Check if cache is still valid.
        let cache_valid = self.cached.as_ref().is_some_and(|(resp, fetched_at)| {
            fetched_at.elapsed().as_millis() < u128::from(resp.ttl_ms)
        });

        if cache_valid {
            return self.cached.as_ref().map(|(r, _)| r);
        }

        // Try the first reachable seed. In production this would use
        // gRPC or the transport layer with fallback across seeds.
        // For now, use the first seed (the seed IS the gateway/leader).
        let seed = self.seeds.first()?;
        let resp = DiscoveryResponse {
            shards: vec![ShardEndpoint {
                shard_id: "bootstrap".to_owned(),
                namespace_id: String::new(),
                leader_node_id: None,
                leader_addr: seed.addr,
                range_start: Vec::new(),
                range_end: Vec::new(),
            }],
            views: vec![],
            gateways: vec![GatewayEndpoint {
                protocol: "grpc".to_owned(),
                transport: "tcp".to_owned(),
                endpoint: seed.addr,
            }],
            ttl_ms: 30_000, // 30-second cache
        };
        self.cached = Some((resp, std::time::Instant::now()));
        self.cached.as_ref().map(|(r, _)| r)
    }

    /// Get all known gateway endpoints (from cache or fresh discovery).
    pub fn gateways(&mut self) -> Vec<GatewayEndpoint> {
        self.discover()
            .map(|r| r.gateways.clone())
            .unwrap_or_default()
    }

    /// Get the leader address for a shard (from cache or fresh discovery).
    pub fn shard_leader(&mut self, shard_id: &str) -> Option<SocketAddr> {
        self.discover().and_then(|r| {
            r.shards
                .iter()
                .find(|s| s.shard_id == shard_id)
                .map(|s| s.leader_addr)
        })
    }

    /// ADR-008 rev 2 — parse a `/cluster/info` JSON response into a
    /// fully-populated [`DiscoveryResponse`]. Used during
    /// [`crate::native::NativeClient::connect`] bootstrap, before the
    /// gRPC data channel is dialled.
    ///
    /// Returns a [`DiscoveryParseError`] if the JSON is unparseable,
    /// missing required fields, or carries an unparseable
    /// `leader_data_addr` / hex range. Unknown fields are ignored
    /// (forward-compat with future rev-N additions).
    ///
    /// Architect-step stub: signature only; the parsing body lands in
    /// the implementer step once the failing unit tests are in place.
    pub fn from_cluster_info_json(_json: &str) -> Result<DiscoveryResponse, DiscoveryParseError> {
        Err(DiscoveryParseError::NotImplemented)
    }
}

/// Parse failures for ADR-008 rev 2 `/cluster/info` JSON.
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryParseError {
    #[error("malformed json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("invalid leader_data_addr: {0}")]
    InvalidLeaderAddr(String),
    #[error("invalid hex range: {0}")]
    InvalidHexRange(String),
    #[error("from_cluster_info_json: architect-stub — implementer step pending")]
    NotImplemented,
}

/// Wire-shape mirror of `kiseki_server::web::api::ShardInfoJson` —
/// duplicated here so kiseki-client doesn't depend on kiseki-server.
/// The two structs MUST stay byte-equivalent on the wire; tests in
/// both crates round-trip the same fixture JSON.
///
/// Field semantics: ADR-008 rev 2 §"Wire shape".
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ShardInfoJson {
    /// UUID string form.
    pub shard_id: String,
    /// Owning namespace.
    pub namespace_id: String,
    /// Best-effort leader's `NodeId`. May be absent when the
    /// responding node has not yet observed a leader.
    #[serde(default)]
    pub leader_id: Option<u64>,
    /// Best-effort leader's data-port address (`host:port`).
    #[serde(default)]
    pub leader_data_addr: Option<String>,
    /// Hex-encoded 32-byte inclusive lower bound, prefixed `0x`.
    pub range_start: String,
    /// Hex-encoded 32-byte exclusive upper bound, prefixed `0x`.
    pub range_end: String,
}

/// Wire-shape mirror of `kiseki_server::web::api::PeerInfoJson`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PeerInfoJson {
    /// Peer `NodeId`.
    pub id: u64,
    /// Raft address.
    pub raft_addr: String,
    /// S3 address (`host:9000`).
    pub s3_addr: String,
    /// NFS address (`host:2049`).
    pub nfs_addr: String,
    /// Metrics address (`host:9090`).
    pub metrics_addr: String,
}

/// Wire-shape mirror of `kiseki_server::web::api::ClusterInfoResponse`.
/// kiseki-client deserializes the HTTP `/cluster/info` response into
/// this type; the typed surface keeps the client / server contract
/// readable in both crates.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClusterInfoResponse {
    /// This node's `NodeId`.
    pub node_id: u64,
    /// This node's S3 address.
    pub s3_addr: String,
    /// This node's NFS address.
    pub nfs_addr: String,
    /// This node's metrics address.
    pub metrics_addr: String,
    /// Bootstrap-shard leader id (rev 1 retained).
    #[serde(default)]
    pub leader_id: Option<u64>,
    /// Bootstrap-shard leader S3 address (rev 1 retained).
    #[serde(default)]
    pub leader_s3: Option<String>,
    /// Cluster peers.
    pub peers: Vec<PeerInfoJson>,
    /// ADR-008 rev 2 per-shard leader map.
    #[serde(default)]
    pub shards: Vec<ShardInfoJson>,
}
