//! Raft cluster + perf state (ADR-037).

use std::collections::HashMap;
use std::sync::Arc;
use kiseki_common::ids::NodeId;

pub struct RaftState {
    pub cluster: Option<kiseki_log::raft::test_cluster::RaftTestCluster>,
    pub write_latencies: Vec<std::time::Duration>,
    pub throughput: Option<(usize, std::time::Duration)>,
    pub single_shard_throughput: Option<(usize, std::time::Duration)>,
    /// Drain orchestrator (ADR-035).
    pub drain_orch: Arc<kiseki_control::node_lifecycle::DrainOrchestrator>,
    pub node_names: HashMap<String, NodeId>,
    pub last_drain_error: Option<String>,
    pub drain_raft: Option<kiseki_log::raft::test_cluster::RaftTestCluster>,
    pub shard_leaders: HashMap<String, String>,
}

impl RaftState {
    pub fn new() -> Self {
        Self {
            cluster: None,
            write_latencies: Vec::new(),
            throughput: None,
            single_shard_throughput: None,
            drain_orch: Arc::new(kiseki_control::node_lifecycle::DrainOrchestrator::new()),
            node_names: HashMap::new(),
            last_drain_error: None,
            drain_raft: None,
            shard_leaders: HashMap::new(),
        }
    }
}
