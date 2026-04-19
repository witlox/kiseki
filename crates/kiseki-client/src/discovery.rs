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
