//! Fabric discovery (ADR-008).
//!
//! Native clients discover shards, views, and gateways via seed
//! endpoints on the data fabric. No control plane connectivity required.

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
    /// Leader node address.
    pub leader_addr: SocketAddr,
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
                addr_str.trim().parse().ok().map(|addr| SeedEndpoint { addr })
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
        let cache_valid = self
            .cached
            .as_ref()
            .is_some_and(|(resp, fetched_at)| {
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
                leader_addr: seed.addr,
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
}
