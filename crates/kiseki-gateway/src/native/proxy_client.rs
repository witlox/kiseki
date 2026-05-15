//! Server-side proxy fallback client (ADR-042 §4 native row + ADR-042 §4).
//!
//! When a non-leader node receives a write whose Raft leader is on
//! another node, the native server proxies the request in-process to
//! the leader's `GatewayDataService` and returns the leader's response.
//! This module holds the per-node tonic channel pool, the per-call
//! hop counter enforcement (gate-1 finding C-H4), and the self-forward
//! defense (gate-1 finding C-H2).
//!
//! The proxy path is opt-in (`KISEKI_NATIVE_PROXY_FALLBACK=on`,
//! default off matching ADR-042 §4 "explicit-routing-only"). When
//! disabled, the native server surfaces `Status::unavailable` to the
//! client and leaves it to the client to dial the leader directly
//! (Step C topology-cache path).
//!
//! ## Hop counter (gate-1 finding C-H4)
//!
//! Every proxied request carries the metadata key
//! [`PROXY_HOP_COUNT_HEADER`] set to the current hop count
//! (initially 0, incremented by each hop). The receiving node MUST
//! reject the request with `Status::resource_exhausted` when the
//! counter reaches [`MAX_PROXY_HOPS`]. The cap defends against a
//! pathological cycle (A's cache says leader=B, B's cache says
//! leader=A, etc.) without requiring topology-cache convergence.
//!
//! ## Self-forward (gate-1 finding C-H2)
//!
//! When the proxy is asked to forward to `leader_node_id ==
//! self.node_id`, the request is rejected with `Status::internal`
//! instead of dialed — a self-forward indicates the local Raft state
//! is out of sync (the local node thinks it's a follower but the
//! cluster thinks it's the leader, or vice versa). The client retries
//! after its own cache refresh.

use std::collections::HashMap;

use kiseki_common::ids::NodeId;
use parking_lot::RwLock;
use tonic::transport::Channel;

/// tonic request metadata key carrying the proxy hop counter.
/// Lowercase ASCII per gRPC metadata rules — tonic will reject
/// uppercase or other-encoded names.
pub const PROXY_HOP_COUNT_HEADER: &str = "kiseki-proxy-hop-count";

/// Maximum number of proxy hops before the request is rejected.
/// Configured at the module level rather than as an env var so the
/// hop budget is auditable from one place. Value chosen per ADR-042 §4
/// §"Hop cap": 1 absorbs in-flight transitions; 2 buys a
/// belt-and-braces second hop; >= 3 risks turning the divergence
/// window into a latency hazard.
pub const MAX_PROXY_HOPS: u8 = 2;

/// Errors specific to the proxy code path. Kept distinct from
/// `tonic::Status` so the native `ServerImpl::put_object` can decide
/// per-call whether to map to `Status::unavailable` (client retries)
/// or to surface a richer body.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// Proxy fallback is enabled but no `ProxyClient` was wired
    /// (configuration bug). The server should fall back to surfacing
    /// the original `ForwardToLeader` error to the client.
    #[error("proxy fallback enabled but ProxyClient not wired")]
    NotConfigured,

    /// The leader's `data_addr` isn't known to the local topology
    /// cache. The client should retry after a topology refresh.
    #[error("leader data_addr unknown for node {0:?}")]
    LeaderAddrUnknown(NodeId),

    /// The hop count has reached `MAX_PROXY_HOPS`. The request is
    /// not forwarded; the client must refresh its topology cache.
    #[error("proxy hop limit exceeded ({0} >= {MAX_PROXY_HOPS})")]
    HopLimitExceeded(u8),

    /// The proxy was asked to forward to itself. Indicates stale
    /// Raft state (gate-1 finding C-H2).
    #[error("self-forward refused (leader_node_id == self_node_id == {0:?})")]
    SelfForwardRefused(NodeId),

    /// tonic transport failure when dialing the leader.
    #[error("tonic transport error: {0}")]
    Transport(String),
}

/// Per-node channel pool for the proxy fallback path.
///
/// Holds a `tonic::transport::Channel` per leader node id, lazily
/// created on first forward. Channels are HTTP/2-multiplexed so
/// concurrent forwards to the same leader share one TCP connection.
///
/// The map is `RwLock<HashMap>` rather than `DashMap` because:
/// (a) the proxy path is a steady-state < 5% fraction of writes
///     (ADR-042 §4 §"Consequences"), so contention is low;
/// (b) the channel registry shape mirrors the existing
///     [`crate::native::server::TopologyInjector`] write pattern
///     (`parking_lot::RwLock`) for consistency.
pub struct ProxyClient {
    /// `NodeId → (data_addr, lazily-opened Channel)`.
    ///
    /// `data_addr` is duplicated alongside the channel so a topology
    /// update can detect when an existing channel points at a stale
    /// address (e.g., leader moved IPs) and refresh the channel.
    nodes: RwLock<HashMap<NodeId, ProxyEntry>>,
    /// Local node id — used to enforce the self-forward defense.
    self_node_id: NodeId,
}

/// One proxy-target entry. Channel is `Option<>` because the
/// `register_node` path stores the address up front (during topology
/// publication) and the channel is built lazily on first forward.
struct ProxyEntry {
    data_addr: String,
    channel: Option<Channel>,
}

impl ProxyClient {
    /// Build a new (empty) proxy client. `self_node_id` is the
    /// local node's Raft id; the self-forward defense rejects
    /// forwards whose target equals this value.
    #[must_use]
    pub fn new(self_node_id: NodeId) -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
            self_node_id,
        }
    }

    /// Register a peer node's data-path gRPC address. The channel is
    /// built lazily on first `forward(...)`. Called by the runtime
    /// when the local topology snapshot is replaced.
    pub fn register_node(&self, node_id: NodeId, data_addr: String) {
        let mut guard = self.nodes.write();
        // If the entry exists and the address didn't change, keep
        // the existing channel. Otherwise drop it so the next
        // forward rebuilds against the new address.
        match guard.get_mut(&node_id) {
            Some(entry) if entry.data_addr == data_addr => {}
            _ => {
                guard.insert(
                    node_id,
                    ProxyEntry {
                        data_addr,
                        channel: None,
                    },
                );
            }
        }
    }

    /// Discard a peer entry — used when a node is removed from the
    /// cluster (drain complete, decommission, etc.).
    pub fn forget_node(&self, node_id: NodeId) {
        self.nodes.write().remove(&node_id);
    }

    /// Local node id this proxy serves. Tests + audit hooks read
    /// this to confirm the self-forward defense is wired.
    #[must_use]
    pub fn self_node_id(&self) -> NodeId {
        self.self_node_id
    }

    /// Check `leader_node_id` against the self-forward defense and
    /// the hop counter. Returns `Err` if either check fails; returns
    /// the cached `data_addr` (the channel build happens on the
    /// hot path inside `ServerImpl`'s proxy method).
    ///
    /// Separated from `acquire_channel` so the `ServerImpl` proxy
    /// path can apply gate-1 finding C-H2 / C-H4 with a single
    /// validation call before allocating any tonic state.
    pub fn validate_forward(
        &self,
        leader_node_id: NodeId,
        current_hop_count: u8,
    ) -> Result<String, ProxyError> {
        if leader_node_id == self.self_node_id {
            return Err(ProxyError::SelfForwardRefused(leader_node_id));
        }
        if current_hop_count >= MAX_PROXY_HOPS {
            return Err(ProxyError::HopLimitExceeded(current_hop_count));
        }
        self.nodes
            .read()
            .get(&leader_node_id)
            .map(|e| e.data_addr.clone())
            .ok_or(ProxyError::LeaderAddrUnknown(leader_node_id))
    }

    /// Acquire a `Channel` for `leader_node_id`. Builds the channel
    /// lazily if absent. Caller MUST have already called
    /// `validate_forward(...)` for the same node id.
    pub async fn acquire_channel(&self, leader_node_id: NodeId) -> Result<Channel, ProxyError> {
        // Fast path: read lock, channel already built.
        {
            let guard = self.nodes.read();
            if let Some(entry) = guard.get(&leader_node_id) {
                if let Some(ch) = &entry.channel {
                    return Ok(ch.clone());
                }
            }
        }
        // Slow path: build the channel. Read the address under the
        // read lock to keep the build off the write lock; then write
        // the resulting channel.
        let addr = {
            let guard = self.nodes.read();
            guard
                .get(&leader_node_id)
                .map(|e| e.data_addr.clone())
                .ok_or(ProxyError::LeaderAddrUnknown(leader_node_id))?
        };
        let endpoint_uri = format!("http://{addr}")
            .parse::<tonic::transport::Uri>()
            .map_err(|e| ProxyError::Transport(format!("invalid leader uri {addr}: {e}")))?;
        let channel = tonic::transport::Channel::builder(endpoint_uri)
            // tonic 0.14 defaults to TCP_NODELAY=true (verified in
            // commit f362060) — no override needed here.
            .connect()
            .await
            .map_err(|e| ProxyError::Transport(e.to_string()))?;
        let mut guard = self.nodes.write();
        if let Some(entry) = guard.get_mut(&leader_node_id) {
            // Another concurrent acquire may have populated the
            // channel between the read-unlock and the write-lock;
            // if so, prefer the existing one and discard ours.
            if entry.channel.is_none() {
                entry.channel = Some(channel.clone());
            }
            Ok(entry.channel.as_ref().expect("populated above").clone())
        } else {
            // Entry vanished — node was forgotten between read and
            // write. Return the freshly-built channel anyway so the
            // in-flight request can complete; the next forward will
            // re-register.
            Ok(channel)
        }
    }

    /// Read-only snapshot of (node, addr) registrations. Used by
    /// tests + `kiseki-server` metrics endpoint.
    #[must_use]
    pub fn registered_nodes(&self) -> Vec<(NodeId, String)> {
        self.nodes
            .read()
            .iter()
            .map(|(k, v)| (*k, v.data_addr.clone()))
            .collect()
    }

    /// ADR-042 §4 wire-level proxy `put_object` re-issue.
    ///
    /// Dial the leader's `GatewayDataService::put_object` and forward
    /// the request **byte-for-byte** with two mutations:
    ///
    /// 1. `ControlFields.forwarded_from_node` is stamped with our own
    ///    `self_node_id` (gate-1 finding M2 — leader's audit log sees
    ///    BOTH the originating tenant AND the forwarding node).
    /// 2. The request carries `kiseki-proxy-hop-count` metadata
    ///    incremented from `incoming_hop_count`. The receiving node's
    ///    [`Self::validate_forward`] enforces the [`MAX_PROXY_HOPS`]
    ///    cap (gate-1 finding C-H4).
    ///
    /// The `ControlFields.idempotency_key` MUST already be set on the
    /// outbound `req` — the caller (server.rs `put_object` handler)
    /// preserves it from the original inbound request. I-NG5 holds
    /// because we never construct a new key here; the leader's dedup
    /// table sees the original.
    ///
    /// The leader's openraft `client_write().await` is what waits for
    /// Raft commit — this call is a single `.await` on the upstream
    /// RPC (gate-1 finding H1 — no `tokio::spawn`, no early-return,
    /// no `tokio::time::timeout` shorter than the proposal timeout).
    /// I-L2 holds end-to-end.
    ///
    /// # Errors
    /// Returns `ProxyError::Transport` for tonic-level failures (no
    /// connection, leader crashed mid-RPC). Returns the leader's
    /// `Status` wrapped in `Transport` for application-level errors
    /// (so the caller can surface them through `map_gateway_error`
    /// or a `Status::aborted` for proxy-node-died-mid-proxy paths
    /// — that scenario is covered by I-NG5 dedup on client retry).
    pub async fn forward_put_object(
        &self,
        leader_node_id: NodeId,
        incoming_hop_count: u8,
        mut req: kiseki_proto::v1::native::PutObjectRequest,
    ) -> Result<kiseki_proto::v1::native::PutObjectResponse, ProxyError> {
        // Validate first — applies hop-cap, self-forward, leader-known
        // defenses. Re-applies the cap so two concurrent forwards
        // from the same node observe the same gate.
        self.validate_forward(leader_node_id, incoming_hop_count)?;

        // Stamp `forwarded_from_node` so the leader's audit log
        // attributes both the originating tenant AND the proxy hop.
        if let Some(cf) = req.control.as_mut() {
            cf.forwarded_from_node = Some(self.self_node_id.0);
        }

        // Acquire / build the tonic channel.
        let channel = self.acquire_channel(leader_node_id).await?;

        // Build the typed gRPC client over the shared channel and
        // wrap the request with the hop-count metadata. Bumping by
        // 1 ensures the leader sees `hop_count = incoming + 1` and
        // the gate-1 cap fires at MAX_PROXY_HOPS.
        let next_hop = incoming_hop_count.saturating_add(1);
        let mut grpc_client =
            kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient::new(
                channel,
            );
        let mut tonic_req = tonic::Request::new(req);
        tonic_req.metadata_mut().insert(
            PROXY_HOP_COUNT_HEADER,
            next_hop
                .to_string()
                .parse()
                .expect("u8 hop count is ASCII numeric — always valid metadata"),
        );

        let resp = grpc_client
            .put_object(tonic_req)
            .await
            .map_err(|s| ProxyError::Transport(format!("leader returned {s}")))?;
        Ok(resp.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nid(n: u64) -> NodeId {
        NodeId(n)
    }

    #[test]
    fn validate_forward_rejects_self_forward() {
        let pc = ProxyClient::new(nid(7));
        pc.register_node(nid(7), "127.0.0.1:9100".to_owned());
        let err = pc.validate_forward(nid(7), 0).unwrap_err();
        assert!(matches!(err, ProxyError::SelfForwardRefused(_)));
    }

    #[test]
    fn validate_forward_rejects_hop_limit() {
        let pc = ProxyClient::new(nid(1));
        pc.register_node(nid(2), "127.0.0.1:9100".to_owned());
        let err = pc.validate_forward(nid(2), MAX_PROXY_HOPS).unwrap_err();
        assert!(matches!(err, ProxyError::HopLimitExceeded(c) if c == MAX_PROXY_HOPS));
    }

    #[test]
    fn validate_forward_rejects_unknown_leader() {
        let pc = ProxyClient::new(nid(1));
        let err = pc.validate_forward(nid(99), 0).unwrap_err();
        assert!(matches!(err, ProxyError::LeaderAddrUnknown(_)));
    }

    #[test]
    fn validate_forward_returns_data_addr_when_valid() {
        let pc = ProxyClient::new(nid(1));
        pc.register_node(nid(2), "127.0.0.1:9100".to_owned());
        let addr = pc.validate_forward(nid(2), 0).unwrap();
        assert_eq!(addr, "127.0.0.1:9100");
    }

    #[test]
    fn register_node_idempotent_for_same_addr_keeps_channel() {
        // Cannot directly inspect Channel; we test that re-registering
        // doesn't reset the entry (the address stays).
        let pc = ProxyClient::new(nid(1));
        pc.register_node(nid(2), "127.0.0.1:9100".to_owned());
        pc.register_node(nid(2), "127.0.0.1:9100".to_owned());
        let nodes = pc.registered_nodes();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0], (nid(2), "127.0.0.1:9100".to_owned()));
    }

    #[test]
    fn register_node_replaces_entry_on_addr_change() {
        let pc = ProxyClient::new(nid(1));
        pc.register_node(nid(2), "127.0.0.1:9100".to_owned());
        pc.register_node(nid(2), "127.0.0.2:9100".to_owned());
        let nodes = pc.registered_nodes();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0], (nid(2), "127.0.0.2:9100".to_owned()));
    }

    #[test]
    fn forget_node_removes_entry() {
        let pc = ProxyClient::new(nid(1));
        pc.register_node(nid(2), "127.0.0.1:9100".to_owned());
        pc.forget_node(nid(2));
        assert!(pc.registered_nodes().is_empty());
    }
}
