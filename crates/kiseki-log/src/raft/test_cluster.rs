//! In-process multi-node Raft test cluster (ADR-037).
//!
//! Creates N `Raft<LogTypeConfig, ShardStateMachine>` instances with
//! in-memory channel-based transport. Supports partition simulation,
//! leader election triggers, and write/read operations.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kiseki_common::ids::{OrgId, SequenceNumber, ShardId};
use kiseki_raft::{KisekiNode, RedbRaftLogStore, Topology};
use openraft::error::RPCError;
use openraft::type_config::async_runtime::WatchReceiver;
use openraft::Raft;
use tokio::sync::RwLock;

use super::state_machine::{ShardSmInner, ShardStateMachine};
use super::types::{LogResponse, LogTypeConfig};
use crate::delta::Delta;
use crate::error::LogError;
use crate::raft_store::LogCommand;

type C = LogTypeConfig;

/// A single node in the test cluster.
pub struct RaftTestNode {
    /// The Raft handle for this node.
    pub raft: Arc<Raft<C, ShardStateMachine>>,
    /// Shared state machine inner (for direct reads).
    pub state: Arc<futures::lock::Mutex<ShardSmInner>>,
    /// Node ID.
    pub node_id: u64,
}

/// Router that dispatches RPCs between in-process Raft nodes.
pub struct TestRouter {
    /// Raft handles keyed by node ID.
    nodes: RwLock<HashMap<u64, Arc<Raft<C, ShardStateMachine>>>>,
    /// Blocked directional links for partition simulation.
    blocked: RwLock<HashSet<(u64, u64)>>,
}

impl TestRouter {
    fn new() -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
            blocked: RwLock::new(HashSet::new()),
        }
    }

    async fn register(&self, node_id: u64, raft: Arc<Raft<C, ShardStateMachine>>) {
        self.nodes.write().await.insert(node_id, raft);
    }

    /// Remove a node from the router. Used by `crash_node` /
    /// `restart_node` to drop the Raft instance before rebuilding.
    async fn deregister(&self, node_id: u64) {
        self.nodes.write().await.remove(&node_id);
    }

    async fn is_blocked(&self, from: u64, to: u64) -> bool {
        self.blocked.read().await.contains(&(from, to))
    }

    /// Dispatch `append_entries` to target node's Raft handle.
    async fn append_entries(
        &self,
        from: u64,
        to: u64,
        rpc: openraft::raft::AppendEntriesRequest<C>,
    ) -> Result<openraft::raft::AppendEntriesResponse<C>, RPCError<C>> {
        if self.is_blocked(from, to).await {
            return Err(RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "partitioned"),
            )));
        }
        let nodes = self.nodes.read().await;
        let target = nodes.get(&to).ok_or_else(|| {
            RPCError::Unreachable(openraft::error::Unreachable::new(&std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "node not found",
            )))
        })?;
        target.append_entries(rpc).await.map_err(|e| {
            RPCError::Unreachable(openraft::error::Unreachable::new(&std::io::Error::other(
                e.to_string(),
            )))
        })
    }

    /// Dispatch vote to target node's Raft handle.
    async fn vote(
        &self,
        from: u64,
        to: u64,
        rpc: openraft::raft::VoteRequest<C>,
    ) -> Result<openraft::raft::VoteResponse<C>, RPCError<C>> {
        if self.is_blocked(from, to).await {
            return Err(RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "partitioned"),
            )));
        }
        let nodes = self.nodes.read().await;
        let target = nodes.get(&to).ok_or_else(|| {
            RPCError::Unreachable(openraft::error::Unreachable::new(&std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "node not found",
            )))
        })?;
        target.vote(rpc).await.map_err(|e| {
            RPCError::Unreachable(openraft::error::Unreachable::new(&std::io::Error::other(
                e.to_string(),
            )))
        })
    }
}

/// Network factory for the test cluster.
struct TestNetworkFactory {
    router: Arc<TestRouter>,
    source_id: u64,
}

/// Network connection to a specific target.
struct TestNetwork {
    router: Arc<TestRouter>,
    source_id: u64,
    target_id: u64,
}

impl openraft::network::RaftNetworkFactory<C> for TestNetworkFactory {
    type Network = TestNetwork;

    async fn new_client(&mut self, target: u64, _node: &KisekiNode) -> TestNetwork {
        TestNetwork {
            router: Arc::clone(&self.router),
            source_id: self.source_id,
            target_id: target,
        }
    }
}

impl openraft::network::v2::RaftNetworkV2<C> for TestNetwork {
    async fn append_entries(
        &mut self,
        rpc: openraft::raft::AppendEntriesRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::AppendEntriesResponse<C>, RPCError<C>> {
        self.router
            .append_entries(self.source_id, self.target_id, rpc)
            .await
    }

    async fn full_snapshot(
        &mut self,
        _vote: openraft::alias::VoteOf<C>,
        _snapshot: openraft::alias::SnapshotOf<C>,
        _cancel: impl futures::Future<Output = openraft::error::ReplicationClosed>
            + openraft::OptionalSend
            + 'static,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::SnapshotResponse<C>, openraft::error::StreamingError<C>> {
        // Snapshot transfer not yet implemented for test cluster.
        Err(openraft::error::StreamingError::Unreachable(
            openraft::error::Unreachable::new(&std::io::Error::other(
                "snapshot not implemented in test cluster",
            )),
        ))
    }

    async fn vote(
        &mut self,
        rpc: openraft::raft::VoteRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::VoteResponse<C>, RPCError<C>> {
        self.router.vote(self.source_id, self.target_id, rpc).await
    }

    async fn transfer_leader(
        &mut self,
        _rpc: openraft::raft::TransferLeaderRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<(), RPCError<C>> {
        Ok(()) // No-op for test cluster.
    }
}

/// Construct one Raft node backed by a redb log store at `path`. Used
/// by both `new` and `restart_node` so the construction recipe is
/// shared and stays in sync across cold start vs. restart paths.
async fn build_node(
    id: u64,
    shard_id: ShardId,
    tenant_id: OrgId,
    path: &std::path::Path,
    router: Arc<TestRouter>,
) -> Result<RaftTestNode, LogError> {
    let log_store = RedbRaftLogStore::<C>::open(path).map_err(|_| LogError::Unavailable)?;
    let sm_inner = Arc::new(futures::lock::Mutex::new(ShardSmInner::new(
        shard_id, tenant_id,
    )));
    let state_machine = ShardStateMachine::new(Arc::clone(&sm_inner));
    let network = TestNetworkFactory {
        router: Arc::clone(&router),
        source_id: id,
    };
    let config = Arc::new(
        openraft::Config {
            heartbeat_interval: 50,
            election_timeout_min: 150,
            election_timeout_max: 300,
            ..openraft::Config::default()
        }
        .validate()
        .expect("valid config"),
    );
    let raft = Raft::new(id, config, network, log_store, state_machine)
        .await
        .map_err(|_| LogError::Unavailable)?;
    let raft = Arc::new(raft);
    router.register(id, Arc::clone(&raft)).await;
    Ok(RaftTestNode {
        raft,
        state: sm_inner,
        node_id: id,
    })
}

/// In-process multi-node Raft test cluster.
///
/// Uses `RedbRaftLogStore` on a per-cluster tempdir so `restart_node()`
/// can rebuild a Raft instance against the same on-disk state — proves
/// log persistence end-to-end without an extra cluster variant.
pub struct RaftTestCluster {
    router: Arc<TestRouter>,
    nodes: HashMap<u64, RaftTestNode>,
    tenant_id: OrgId,
    shard_id: ShardId,
    /// Owns the temp directory backing every node's redb log store.
    /// Dropped with the cluster — files are gone after the test exits.
    log_dir: tempfile::TempDir,
    /// Per-node redb file path under `log_dir` so `restart_node` can
    /// reopen the same state.
    log_paths: HashMap<u64, PathBuf>,
    /// Topology label per node (Phase 14f). Set via `set_topology`,
    /// consulted by the placement-spread assertions in BDD scenarios.
    node_topologies: HashMap<u64, Topology>,
}

impl RaftTestCluster {
    /// Create a new N-node cluster with a single shard.
    ///
    /// Node IDs are `1..=node_count`. Node 1 is the seed (calls `initialize`).
    pub async fn new(node_count: u64, shard_id: ShardId, tenant_id: OrgId) -> Self {
        let router = Arc::new(TestRouter::new());
        let mut nodes = HashMap::new();
        let mut log_paths = HashMap::new();
        let log_dir = tempfile::tempdir().expect("tempdir for raft logs");

        // Create all nodes.
        for id in 1..=node_count {
            let path = log_dir.path().join(format!("node-{id}.redb"));
            log_paths.insert(id, path.clone());
            let node = build_node(id, shard_id, tenant_id, &path, Arc::clone(&router))
                .await
                .expect("build node");
            nodes.insert(id, node);
        }

        // Initialize membership on node 1 (seed).
        let members: BTreeMap<u64, KisekiNode> = (1..=node_count)
            .map(|id| {
                (
                    id,
                    KisekiNode {
                        addr: format!("127.0.0.1:{}", 9100 + id),
                        ..Default::default()
                    },
                )
            })
            .collect();
        nodes
            .get(&1)
            .unwrap()
            .raft
            .initialize(members)
            .await
            .expect("raft initialization");

        Self {
            router,
            nodes,
            tenant_id,
            shard_id,
            log_dir,
            log_paths,
            node_topologies: HashMap::new(),
        }
    }

    /// Attach a topology label to `node_id`. Used by placement and
    /// rack-aware tests to model failure-domain spread.
    pub fn set_topology(&mut self, node_id: u64, topology: Topology) {
        self.node_topologies.insert(node_id, topology);
    }

    /// Topology label for `node_id`, if one has been set.
    #[must_use]
    pub fn topology_of(&self, node_id: u64) -> Option<&Topology> {
        self.node_topologies.get(&node_id)
    }

    /// Distinct failure domains across the current voters. Used by
    /// the rack-spread assertion: `>= 2` means the placement avoids
    /// a single-rack outage.
    pub async fn voter_failure_domains(&self) -> std::collections::HashSet<String> {
        self.voter_ids()
            .await
            .into_iter()
            .filter_map(|id| self.node_topologies.get(&id).map(Topology::failure_domain))
            .collect()
    }

    /// Get the current leader node ID — the node agreed on by a
    /// majority of cluster members. An isolated stale leader will not
    /// pass the majority filter (it can only vote for itself), so this
    /// matches what a real client would observe after a partition.
    // Async kept for symmetry with other RaftTestCluster RPC-style helpers
    // and because BDD step definitions await it through generic harness code.
    #[allow(clippy::unused_async)]
    pub async fn leader(&self) -> Option<u64> {
        let total = self.nodes.len();
        let mut votes: HashMap<u64, usize> = HashMap::new();
        for node in self.nodes.values() {
            let rx = node.raft.metrics();
            let leader_id = rx.borrow_watched().current_leader;
            if let Some(lid) = leader_id {
                *votes.entry(lid).or_insert(0) += 1;
            }
        }
        votes
            .into_iter()
            .find(|(_, count)| *count * 2 > total)
            .map(|(id, _)| id)
    }

    /// Wait until a leader is elected, with timeout.
    pub async fn wait_for_leader(&self, timeout: Duration) -> Option<u64> {
        let start = std::time::Instant::now();
        loop {
            if let Some(leader) = self.leader().await {
                return Some(leader);
            }
            if start.elapsed() > timeout {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Write a delta through the leader.
    ///
    /// Bounded by a 5 s timeout — openraft's `client_write` blocks
    /// indefinitely when quorum cannot commit, which would hang the
    /// "quorum loss blocks writes" scenario forever (~90 min observed
    /// before adding this guard). The timeout converts the indefinite
    /// wait into a `LogError::Unavailable`, which is exactly what the
    /// scenario asserts.
    pub async fn write_delta(&self, key_byte: u8) -> Result<SequenceNumber, LogError> {
        let leader_id = self
            .wait_for_leader(Duration::from_secs(5))
            .await
            .ok_or(LogError::Unavailable)?;
        let leader = &self.nodes[&leader_id];

        let cmd = LogCommand::AppendDelta {
            tenant_id_bytes: *self.tenant_id.0.as_bytes(),
            operation: 0, // Create
            hashed_key: [key_byte; 32],
            payload: vec![0xab; 64],
            chunk_refs: vec![],
            has_inline_data: false,
        };

        let resp = tokio::time::timeout(Duration::from_secs(5), leader.raft.client_write(cmd))
            .await
            .map_err(|_| LogError::Unavailable)?
            .map_err(|_| LogError::Unavailable)?;

        match resp.data {
            LogResponse::Appended(seq) => Ok(SequenceNumber(seq)),
            LogResponse::Ok => Ok(SequenceNumber(0)),
        }
    }

    /// Read deltas from a specific node's state machine.
    pub async fn read_from(&self, node_id: u64) -> Vec<Delta> {
        let node = &self.nodes[&node_id];
        let inner = node.state.lock().await;
        inner.deltas.clone()
    }

    /// Isolate a node (symmetric partition).
    pub async fn isolate_node(&self, node_id: u64) {
        let all: Vec<u64> = self.nodes.keys().copied().collect();
        let mut blocked = self.router.blocked.write().await;
        for &other in &all {
            if other != node_id {
                blocked.insert((node_id, other));
                blocked.insert((other, node_id));
            }
        }
    }

    /// Restore a node (remove partition).
    pub async fn restore_node(&self, node_id: u64) {
        let all: Vec<u64> = self.nodes.keys().copied().collect();
        let mut blocked = self.router.blocked.write().await;
        for &other in &all {
            blocked.remove(&(node_id, other));
            blocked.remove(&(other, node_id));
        }
    }

    /// Get the number of nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Trigger election on a specific node.
    pub async fn trigger_election(&self, node_id: u64) {
        if let Some(node) = self.nodes.get(&node_id) {
            let _ = node.raft.trigger().elect().await;
        }
    }

    /// Spawn an additional node and add it as a learner to the cluster.
    /// Returns once the leader has accepted the learner change.
    pub async fn add_learner(&mut self, new_id: u64) -> Result<(), LogError> {
        let path = self.log_dir.path().join(format!("node-{new_id}.redb"));
        let node = build_node(
            new_id,
            self.shard_id,
            self.tenant_id,
            &path,
            Arc::clone(&self.router),
        )
        .await?;
        self.log_paths.insert(new_id, path);

        // Tell the leader to add it as a learner.
        let leader_id = self
            .wait_for_leader(Duration::from_secs(5))
            .await
            .ok_or(LogError::Unavailable)?;
        let leader = &self.nodes[&leader_id];
        let kn = KisekiNode {
            addr: format!("127.0.0.1:{}", 9100 + new_id),
            ..Default::default()
        };
        leader
            .raft
            .add_learner(new_id, kn, true)
            .await
            .map_err(|_| LogError::Unavailable)?;

        self.nodes.insert(new_id, node);
        Ok(())
    }

    /// Simulate a crash of `node_id`: tear down the in-memory Raft
    /// instance but leave the on-disk log store intact. After this,
    /// peers will see the node as unreachable and `restart_node` can
    /// bring it back from the persisted state.
    pub async fn crash_node(&mut self, node_id: u64) -> Result<(), LogError> {
        let node = self.nodes.remove(&node_id).ok_or(LogError::Unavailable)?;
        self.router.deregister(node_id).await;
        // Polite shutdown: openraft cancels its background workers.
        let _ = node.raft.shutdown().await;
        // Drop the Arc explicitly so the inner Raft is dropped here.
        drop(node);
        Ok(())
    }

    /// Bring a previously-crashed node back online by reopening its
    /// on-disk log store and rebuilding the Raft instance. The
    /// state machine replays from the log — proves the local log
    /// survives a process restart (Phase 14f).
    pub async fn restart_node(&mut self, node_id: u64) -> Result<(), LogError> {
        let path = self
            .log_paths
            .get(&node_id)
            .ok_or(LogError::Unavailable)?
            .clone();
        let node = build_node(
            node_id,
            self.shard_id,
            self.tenant_id,
            &path,
            Arc::clone(&self.router),
        )
        .await?;
        self.nodes.insert(node_id, node);
        Ok(())
    }

    /// Change cluster membership to the given voter set. Promotes any
    /// listed learners and removes voters not in the new set.
    pub async fn change_membership(
        &self,
        voters: BTreeMap<u64, KisekiNode>,
    ) -> Result<(), LogError> {
        let leader_id = self
            .wait_for_leader(Duration::from_secs(5))
            .await
            .ok_or(LogError::Unavailable)?;
        let leader = &self.nodes[&leader_id];
        let voter_ids: std::collections::BTreeSet<u64> = voters.keys().copied().collect();
        leader
            .raft
            .change_membership(voter_ids, false)
            .await
            .map_err(|_| LogError::Unavailable)?;
        Ok(())
    }

    /// Current voter set as known to the leader.
    pub async fn voter_ids(&self) -> Vec<u64> {
        let Some(leader_id) = self.leader().await else {
            return Vec::new();
        };
        let metrics = self.nodes[&leader_id].raft.metrics();
        let m = metrics.borrow_watched().clone();
        m.membership_config
            .membership()
            .voter_ids()
            .collect::<Vec<_>>()
    }

    /// Shutdown all nodes.
    pub async fn shutdown(self) {
        for (_, node) in self.nodes {
            let _ = node.raft.shutdown().await;
        }
    }
}

/// `RaftMembershipAdapter` impl so the control-plane drain
/// orchestrator can drive `RaftTestCluster` directly. This is the
/// integration glue called out in the integrator review (seam #5):
/// the orchestrator no longer maintains a parallel narrative of voter
/// state — it asks Raft.
impl kiseki_common::raft_adapter::RaftMembershipAdapter for RaftTestCluster {
    fn add_learner(
        &mut self,
        replacement: kiseki_common::ids::NodeId,
    ) -> kiseki_common::raft_adapter::MembershipFuture<'_, ()> {
        Box::pin(async move {
            self.add_learner(replacement.0)
                .await
                .map_err(|e| kiseki_common::raft_adapter::MembershipError::Raft(e.to_string()))
        })
    }

    fn replace_voter(
        &mut self,
        target: kiseki_common::ids::NodeId,
        replacement: kiseki_common::ids::NodeId,
    ) -> kiseki_common::raft_adapter::MembershipFuture<'_, ()> {
        Box::pin(async move {
            // Build the new voter set: everything currently known minus
            // the target, plus the replacement.
            let current = self.voter_ids().await;
            let mut new_voters: BTreeMap<u64, KisekiNode> = BTreeMap::new();
            for id in current {
                if id == target.0 {
                    continue;
                }
                new_voters.insert(
                    id,
                    KisekiNode {
                        addr: format!("127.0.0.1:{}", 9100 + id),
                        ..Default::default()
                    },
                );
            }
            new_voters.insert(
                replacement.0,
                KisekiNode {
                    addr: format!("127.0.0.1:{}", 9100 + replacement.0),
                    ..Default::default()
                },
            );
            self.change_membership(new_voters)
                .await
                .map_err(|e| kiseki_common::raft_adapter::MembershipError::Raft(e.to_string()))
        })
    }

    fn voter_ids(
        &self,
    ) -> kiseki_common::raft_adapter::MembershipFuture<'_, Vec<kiseki_common::ids::NodeId>> {
        Box::pin(async move {
            Ok(self
                .voter_ids()
                .await
                .into_iter()
                .map(kiseki_common::ids::NodeId)
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn three_node_cluster_elects_leader() {
        let shard = ShardId(uuid::Uuid::from_u128(1));
        let tenant = OrgId(uuid::Uuid::from_u128(100));
        let cluster = RaftTestCluster::new(3, shard, tenant).await;

        let leader = cluster.wait_for_leader(Duration::from_secs(10)).await;
        assert!(leader.is_some(), "cluster should elect a leader");

        cluster.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_replicates_to_followers() {
        let shard = ShardId(uuid::Uuid::from_u128(2));
        let tenant = OrgId(uuid::Uuid::from_u128(200));
        let cluster = RaftTestCluster::new(3, shard, tenant).await;

        cluster
            .wait_for_leader(Duration::from_secs(10))
            .await
            .unwrap();

        // Write through leader.
        let seq = cluster.write_delta(0x42).await.unwrap();
        assert!(seq.0 > 0, "should get a sequence number");

        // Give time for replication.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Read from all nodes — all should have the delta.
        for node_id in 1..=3 {
            let deltas = cluster.read_from(node_id).await;
            assert!(
                !deltas.is_empty(),
                "node {node_id} should have the replicated delta"
            );
        }

        cluster.shutdown().await;
    }
}
