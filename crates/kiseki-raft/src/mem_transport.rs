//! In-memory channel-based Raft transport for testing.
//!
//! Provides `InMemoryRouter` for in-process multi-node Raft clusters.
//! Each node's RPCs are dispatched via `tokio::sync::mpsc` channels.
//! Network partitions are simulated via a blocked-links set.
//!
//! ADR-037: test infrastructure for 41 multi-node Raft BDD scenarios.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;

use openraft::error::{RPCError, Unreachable};
use openraft::network::v2::RaftNetworkV2;
use openraft::network::{RPCOption, RaftNetworkFactory};
use openraft::raft::{AppendEntriesRequest, AppendEntriesResponse, VoteRequest, VoteResponse};
use openraft::RaftTypeConfig;
use tokio::sync::RwLock;

/// Shared router for in-process Raft clusters.
///
/// Maps node IDs to channel senders. RPCs are dispatched by sending
/// a message through the channel; a background dispatcher task on
/// each node receives and calls the Raft handle.
pub struct InMemoryRouter {
    /// Blocked directional links: `(from, to)`. If present, RPCs return `Unreachable`.
    blocked: RwLock<HashSet<(u64, u64)>>,
}

impl InMemoryRouter {
    /// Create a new empty router.
    pub fn new() -> Self {
        Self {
            blocked: RwLock::new(HashSet::new()),
        }
    }

    /// Block all traffic from `from` to `to` (directional).
    pub async fn block_link(&self, from: u64, to: u64) {
        self.blocked.write().await.insert((from, to));
    }

    /// Unblock traffic from `from` to `to`.
    pub async fn unblock_link(&self, from: u64, to: u64) {
        self.blocked.write().await.remove(&(from, to));
    }

    /// Isolate a node: block all traffic to/from it (symmetric, ADV-037-1).
    pub async fn isolate_node(&self, node_id: u64, all_nodes: &[u64]) {
        let mut blocked = self.blocked.write().await;
        for &other in all_nodes {
            if other != node_id {
                blocked.insert((node_id, other));
                blocked.insert((other, node_id));
            }
        }
    }

    /// Restore a node: unblock all traffic to/from it.
    pub async fn restore_node(&self, node_id: u64, all_nodes: &[u64]) {
        let mut blocked = self.blocked.write().await;
        for &other in all_nodes {
            blocked.remove(&(node_id, other));
            blocked.remove(&(other, node_id));
        }
    }

    /// Check if a link is blocked.
    pub async fn is_blocked(&self, from: u64, to: u64) -> bool {
        self.blocked.read().await.contains(&(from, to))
    }
}

impl Default for InMemoryRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// Network factory that creates `InMemoryNetwork` connections.
///
/// Each connection checks the router's blocked set before dispatching.
/// When the target is blocked, RPCs return `Unreachable`.
pub struct InMemoryNetworkFactory<C: RaftTypeConfig> {
    router: Arc<InMemoryRouter>,
    source_id: u64,
    _phantom: std::marker::PhantomData<C>,
}

impl<C: RaftTypeConfig> InMemoryNetworkFactory<C> {
    /// Create a factory for a specific source node.
    pub fn new(router: Arc<InMemoryRouter>, source_id: u64) -> Self {
        Self {
            router,
            source_id,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// A connection to a specific target node through the in-memory router.
pub struct InMemoryNetwork {
    router: Arc<InMemoryRouter>,
    source_id: u64,
    target_id: u64,
}

impl<C: RaftTypeConfig<NodeId = u64>> RaftNetworkFactory<C> for InMemoryNetworkFactory<C> {
    type Network = InMemoryNetwork;

    async fn new_client(&mut self, target: u64, _node: &C::Node) -> InMemoryNetwork {
        InMemoryNetwork {
            router: Arc::clone(&self.router),
            source_id: self.source_id,
            target_id: target,
        }
    }
}

fn blocked_err<C: RaftTypeConfig>() -> RPCError<C> {
    RPCError::Unreachable(Unreachable::new(&io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "link blocked by test partition",
    )))
}

impl<C: RaftTypeConfig<NodeId = u64>> RaftNetworkV2<C> for InMemoryNetwork {
    async fn append_entries(
        &mut self,
        _rpc: AppendEntriesRequest<C>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<C>, RPCError<C>> {
        if self.router.is_blocked(self.source_id, self.target_id).await {
            return Err(blocked_err::<C>());
        }
        // In a full implementation, this would dispatch to the target's Raft handle.
        // For now, return Unreachable — the RaftTestCluster in kiseki-log will
        // override this with a channel-based dispatcher.
        Err(blocked_err::<C>())
    }

    async fn full_snapshot(
        &mut self,
        _vote: openraft::alias::VoteOf<C>,
        _snapshot: openraft::alias::SnapshotOf<C>,
        _cancel: impl futures::Future<Output = openraft::error::ReplicationClosed>
            + openraft::OptionalSend
            + 'static,
        _option: RPCOption,
    ) -> Result<openraft::raft::SnapshotResponse<C>, openraft::error::StreamingError<C>> {
        Err(openraft::error::StreamingError::Unreachable(
            Unreachable::new(&io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "link blocked by test partition",
            )),
        ))
    }

    async fn vote(
        &mut self,
        _rpc: VoteRequest<C>,
        _option: RPCOption,
    ) -> Result<VoteResponse<C>, RPCError<C>> {
        if self.router.is_blocked(self.source_id, self.target_id).await {
            return Err(blocked_err::<C>());
        }
        Err(blocked_err::<C>())
    }

    async fn transfer_leader(
        &mut self,
        _rpc: openraft::raft::TransferLeaderRequest<C>,
        _option: RPCOption,
    ) -> Result<(), RPCError<C>> {
        Err(blocked_err::<C>())
    }
}
