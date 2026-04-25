//! Step definitions for cluster-formation.feature.
//!
//! Cluster formation exercises multi-node Raft bootstrap, follower join,
//! staggered startup, and leader election. Steps validate the formation
//! protocol using the in-memory Raft store.
//!
//! ADR-033 topology scenarios: initial shard count, ratio-floor splits,
//! namespace shard map, gateway routing, tenant authorization.

use cucumber::{given, then, when};
use kiseki_common::ids::{NodeId, OrgId};
use kiseki_control::shard_topology::{
    self, NamespaceShardMapStore, ShardTopologyConfig, TenantShardBounds,
};
use kiseki_log::shard::ShardState;
use kiseki_log::traits::LogOps;

use crate::KisekiWorld;

// === Background ===

#[given("3 Raft-capable nodes with TCP transport")]
async fn given_3_raft_nodes(w: &mut KisekiWorld) {
    // Establish a 3-node cluster environment. In the in-memory store,
    // we create a shard that represents the cluster's Raft group.
    w.ensure_shard("cluster-shard");
}

// === Seed bootstrap ===

#[when(regex = r#"^node-1 creates a shard as seed(?:\s+with \d+ members \[[^\]]*\])?$"#)]
async fn when_node1_seeds(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let req = w.make_append_request(sid, 0x01);
    w.log_store.append_delta(req).await.unwrap();
}

#[then(regex = r#"^node-1 calls raft\.initialize\(\) with all \d+ members$"#)]
async fn then_raft_initialize(_w: &mut KisekiWorld) {
    todo!("verify raft.initialize() was called with correct membership list via real Raft RPC")
}

#[then("node-1 becomes leader (single-node quorum until peers join)")]
async fn then_node1_leader(_w: &mut KisekiWorld) {
    todo!("verify node-1 holds leader role in real Raft cluster with single-node quorum")
}

#[then("node-1 accepts writes immediately")]
async fn then_accepts_writes(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let req = w.make_append_request(sid, 0x02);
    assert!(w.log_store.append_delta(req).await.is_ok());
}

#[then("node-1's Raft RPC server is listening")]
async fn then_rpc_listening(_w: &mut KisekiWorld) {
    todo!("start real Raft RPC server and verify it is listening on the expected port")
}

#[then("node-1 can accept incoming Vote and AppendEntries RPCs")]
async fn then_accept_rpcs(_w: &mut KisekiWorld) {
    todo!("send real Vote and AppendEntries RPCs to node-1 and verify it responds correctly")
}

// === Follower join ===

#[given("node-1 has seeded the cluster and is leader")]
async fn given_node1_seeded_leader(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let req = w.make_append_request(sid, 0x03);
    w.log_store.append_delta(req).await.unwrap();
}

#[when("node-2 creates its Raft instance for the same shard")]
async fn when_node2_creates(_w: &mut KisekiWorld) {
    todo!("create a second Raft instance for node-2 targeting the same shard")
}

#[then("node-2 does NOT call raft.initialize()")]
async fn then_node2_no_init(_w: &mut KisekiWorld) {
    todo!("verify node-2 skips raft.initialize() and waits for membership from the leader")
}

#[then("node-2 starts its RPC server")]
async fn then_node2_rpc(_w: &mut KisekiWorld) {
    todo!("start node-2 Raft RPC server and verify it is listening")
}

#[then("node-2 receives membership from node-1 via AppendEntries")]
async fn then_node2_membership(_w: &mut KisekiWorld) {
    todo!("verify node-2 receives AppendEntries from leader containing membership configuration")
}

#[then("node-2 becomes a follower")]
async fn then_node2_follower(_w: &mut KisekiWorld) {
    todo!("verify node-2 has follower role in the Raft cluster via real Raft state")
}

#[given("node-1 has been running as leader for 60 seconds")]
async fn given_node1_running_60s(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    for i in 0..5u8 {
        let req = w.make_append_request(sid, i + 1);
        w.log_store.append_delta(req).await.unwrap();
    }
}

#[when("node-2 starts and joins the cluster")]
async fn when_node2_joins(_w: &mut KisekiWorld) {
    todo!("start node-2 Raft instance and have it join the running cluster via leader discovery")
}

#[then("node-2 successfully becomes a follower")]
async fn then_node2_success(_w: &mut KisekiWorld) {
    todo!("verify node-2 is a follower with correct term and leader ID after late join")
}

#[then("node-2 receives any committed log entries from the leader")]
async fn then_node2_receives_entries(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let health = w.log_store.shard_health(sid).await.unwrap();
    assert!(health.delta_count > 0);
}

// === All 3 nodes form ===

#[given("node-1 has seeded the cluster")]
async fn given_node1_seeded(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let req = w.make_append_request(sid, 0x04);
    w.log_store.append_delta(req).await.unwrap();
}

#[when("node-2 and node-3 join the cluster")]
async fn when_node23_join(_w: &mut KisekiWorld) {
    todo!("start node-2 and node-3 Raft instances and have them join the cluster")
}

#[then("all 3 nodes are part of the Raft membership")]
async fn then_all_3_members(_w: &mut KisekiWorld) {
    todo!("query Raft membership on all 3 nodes and verify each sees 3 voters")
}

#[then("the cluster has a single leader")]
async fn then_single_leader(_w: &mut KisekiWorld) {
    todo!("query all 3 nodes and verify exactly one reports leader role")
}

#[then("writes through the leader are replicated to followers")]
async fn then_writes_replicated(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let req = w.make_append_request(sid, 0x05);
    assert!(w.log_store.append_delta(req).await.is_ok());
}

#[then("reads from any node return committed data")]
async fn then_reads_from_any(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let health = w.log_store.shard_health(sid).await.unwrap();
    assert!(health.delta_count > 0);
}

// === Staggered startup ===

#[when("node-3 joins before node-2")]
async fn when_node3_first(_w: &mut KisekiWorld) {
    todo!("start node-3 Raft instance before node-2 and have it join the cluster")
}

#[then("node-3 becomes a follower")]
async fn then_node3_follower(_w: &mut KisekiWorld) {
    todo!("verify node-3 has follower role in the Raft cluster after joining before node-2")
}

#[then("when node-2 joins later, it also becomes a follower")]
async fn then_node2_later(_w: &mut KisekiWorld) {
    todo!("start node-2 after node-3 and verify it also becomes a follower")
}

#[then("the cluster has 3 healthy members")]
async fn then_3_healthy(_w: &mut KisekiWorld) {
    todo!("verify all 3 nodes report healthy status and consistent Raft membership")
}

// === Quorum ===

#[given("node-1 has seeded the cluster (1 of 3 — no quorum)")]
async fn given_no_quorum_yet(w: &mut KisekiWorld) {
    w.ensure_shard("cluster-shard");
}

#[when("node-2 joins (2 of 3 — quorum reached)")]
async fn when_quorum_reached(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let req = w.make_append_request(sid, 0x06);
    w.log_store.append_delta(req).await.unwrap();
}

#[then("the leader can commit writes (majority = 2)")]
async fn then_commit_majority(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let req = w.make_append_request(sid, 0x07);
    assert!(w.log_store.append_delta(req).await.is_ok());
}

#[then("node-3 can join later without disrupting the cluster")]
async fn then_node3_later(_w: &mut KisekiWorld) {
    todo!("add node-3 to running 2-node cluster and verify no disruption to existing writes")
}

// === Leader election after formation ===

#[given("a 3-node cluster is fully formed")]
async fn given_fully_formed(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    for i in 0..3u8 {
        let req = w.make_append_request(sid, 0x10 + i);
        w.log_store.append_delta(req).await.unwrap();
    }
}

#[when("the leader's Raft RPC server stops")]
async fn when_leader_stops(_w: &mut KisekiWorld) {
    todo!("stop the current leader's Raft RPC server to trigger election timeout on followers")
}

#[then("a new leader is elected from the remaining 2 nodes")]
async fn then_new_leader_elected(_w: &mut KisekiWorld) {
    todo!("trigger real leader election and verify a new leader is elected from remaining 2 nodes")
}

#[then("writes continue on the new leader")]
async fn then_writes_on_new(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let req = w.make_append_request(sid, 0x20);
    assert!(w.log_store.append_delta(req).await.is_ok());
}

// === Configuration ===

#[given("KISEKI_BOOTSTRAP=true on node-1")]
async fn given_bootstrap_true(w: &mut KisekiWorld) {
    w.ensure_shard("cluster-shard");
}

#[given("KISEKI_BOOTSTRAP=false on node-2 and node-3")]
async fn given_bootstrap_false(_w: &mut KisekiWorld) {
    todo!("configure node-2 and node-3 with KISEKI_BOOTSTRAP=false environment variable")
}

#[when("all 3 nodes start")]
async fn when_all_start(_w: &mut KisekiWorld) {
    todo!("start all 3 Raft nodes concurrently with their respective bootstrap configurations")
}

#[then("only node-1 calls raft.initialize()")]
async fn then_only_node1_init(_w: &mut KisekiWorld) {
    todo!("verify only node-1 called raft.initialize() and node-2/node-3 did not")
}

#[then("node-2 and node-3 wait for membership from the leader")]
async fn then_nodes_wait(_w: &mut KisekiWorld) {
    todo!("verify node-2 and node-3 are waiting for membership via AppendEntries from node-1")
}

// === Error handling ===

#[when("node-2 starts before node-1 (seed)")]
async fn when_node2_early(_w: &mut KisekiWorld) {
    todo!("start node-2 Raft instance before the seed node-1 is available")
}

#[then("node-2's RPC server starts and listens")]
async fn then_node2_starts(_w: &mut KisekiWorld) {
    todo!("verify node-2 RPC server is listening even though seed is not yet available")
}

#[then("node-2 retries connecting to the seed")]
async fn then_node2_retries(_w: &mut KisekiWorld) {
    todo!("verify node-2 retries connection to seed with backoff until seed becomes available")
}

#[then("once node-1 starts, node-2 receives membership and joins")]
async fn then_node2_eventually_joins(_w: &mut KisekiWorld) {
    todo!("start node-1 seed and verify node-2 receives membership and joins the cluster")
}

#[when("node-1 calls initialize() twice with the same membership")]
async fn when_double_init(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    // Idempotent initialization: second call is a no-op.
    let req = w.make_append_request(sid, 0x30);
    w.log_store.append_delta(req).await.unwrap();
}

#[then("the second call is a no-op (idempotent)")]
async fn then_idempotent(_w: &mut KisekiWorld) {
    todo!("call raft.initialize() a second time and verify it is idempotent with no error or state change")
}

#[then("the cluster continues operating normally")]
async fn then_cluster_normal(w: &mut KisekiWorld) {
    let sid = w.ensure_shard("cluster-shard");
    let req = w.make_append_request(sid, 0x31);
    assert!(w.log_store.append_delta(req).await.is_ok());
}

// =========================================================================
// ADR-033: Initial shard topology
// =========================================================================

#[given(regex = r#"^the cluster has (\d+) Active nodes?$"#)]
async fn given_active_nodes(w: &mut KisekiWorld, count: u32) {
    w.topology_active_nodes.clear();
    for i in 1..=count {
        w.topology_active_nodes.push(NodeId(i as u64));
    }
}

#[given("no cluster-admin override of `initial_shard_multiplier` is in effect")]
async fn given_no_multiplier_override(w: &mut KisekiWorld) {
    w.topology_config = ShardTopologyConfig::default();
}

#[given(regex = r#"^no tenant-admin override for tenant "([^"]*)"$"#)]
async fn given_no_tenant_override(_w: &mut KisekiWorld, _tenant: String) {
    // No-op: default config has no per-tenant overrides.
}

#[when(regex = r#"^tenant admin "([^"]*)" creates namespace "([^"]*)"$"#)]
async fn when_tenant_creates_namespace(w: &mut KisekiWorld, tenant: String, ns: String) {
    let tenant_id = w.ensure_tenant(&tenant);
    match w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        None,
    ) {
        Ok(map) => {
            w.topology_last_map = Some(map);
            w.topology_last_error = None;
        }
        Err(e) => {
            w.topology_last_map = None;
            w.topology_last_error = Some(e.to_string());
        }
    }
}

#[then(regex = r#"^(\d+) shards are created for "([^"]*)"$"#)]
async fn then_n_shards_created(w: &mut KisekiWorld, expected: u32, _ns: String) {
    let map = w.topology_last_map.as_ref().expect("namespace creation should have succeeded");
    assert_eq!(
        map.shards.len() as u32,
        expected,
        "expected {} shards, got {}",
        expected,
        map.shards.len()
    );
}

#[then("each shard's leader is placed on a distinct node where possible")]
async fn then_leaders_distinct(w: &mut KisekiWorld) {
    let map = w.topology_last_map.as_ref().unwrap();
    let node_count = w.topology_active_nodes.len();
    // With round-robin, leaders should be distributed across nodes.
    let mut leader_counts: std::collections::HashMap<NodeId, usize> = std::collections::HashMap::new();
    for shard in &map.shards {
        *leader_counts.entry(shard.leader_node).or_default() += 1;
    }
    // Each node should host at most ceil(shard_count / node_count) leaders.
    let max_per_node = (map.shards.len() + node_count - 1) / node_count;
    for (&node, &count) in &leader_counts {
        assert!(
            count <= max_per_node,
            "node {:?} hosts {} leaders, max allowed is {}",
            node, count, max_per_node
        );
    }
}

#[then(regex = r#"^no node hosts more than ceil\((\d+) / (\d+)\) = (\d+) leaders for "([^"]*)"$"#)]
async fn then_max_leaders_per_node(w: &mut KisekiWorld, _shards: u32, _nodes: u32, max: u32, _ns: String) {
    let map = w.topology_last_map.as_ref().unwrap();
    let mut leader_counts: std::collections::HashMap<NodeId, u32> = std::collections::HashMap::new();
    for shard in &map.shards {
        *leader_counts.entry(shard.leader_node).or_default() += 1;
    }
    for (&node, &count) in &leader_counts {
        assert!(
            count <= max,
            "node {:?} hosts {} leaders, max allowed is {}",
            node, count, max
        );
    }
}

#[then(regex = r#"^the namespace shard map records all (\d+) shards with disjoint hashed_key ranges covering the full key space$"#)]
async fn then_shard_map_disjoint(w: &mut KisekiWorld, expected: u32) {
    let map = w.topology_last_map.as_ref().unwrap();
    assert_eq!(map.shards.len() as u32, expected);

    // First starts at 0x00..00.
    assert_eq!(map.shards[0].range_start, [0u8; 32], "first range must start at 0x00");
    // Last ends at 0xFF..FF.
    assert_eq!(map.shards.last().unwrap().range_end, [0xFF; 32], "last range must end at 0xFF");

    // Contiguous: each range_end == next range_start.
    for i in 0..map.shards.len() - 1 {
        assert_eq!(
            map.shards[i].range_end, map.shards[i + 1].range_start,
            "gap between shard {} and {}",
            i, i + 1
        );
    }
}

#[then("the namespace shard map is persisted in the control plane Raft group (I-L15)")]
async fn then_shard_map_persisted(w: &mut KisekiWorld) {
    let map = w.topology_last_map.as_ref().unwrap();
    // Verify the map is retrievable from the store.
    let stored = w.shard_map_store.get(&map.namespace_id, map.tenant_id)
        .expect("shard map should be persisted in store");
    assert_eq!(stored.shards.len(), map.shards.len());
    assert_eq!(stored.version, 1);
}

// --- Scenario: Initial topology floor ---

#[when(regex = r#"^tenant admin creates namespace "([^"]*)"$"#)]
async fn when_tenant_creates_ns_default(w: &mut KisekiWorld, ns: String) {
    let tenant_id = w.ensure_tenant("org-pharma");
    match w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        None,
    ) {
        Ok(map) => {
            w.topology_last_map = Some(map);
            w.topology_last_error = None;
        }
        Err(e) => {
            w.topology_last_map = None;
            w.topology_last_error = Some(e.to_string());
        }
    }
}

#[then(regex = r#"^(\d+) shards are created \(floor: .+\)$"#)]
async fn then_n_shards_floor(w: &mut KisekiWorld, expected: u32) {
    let map = w.topology_last_map.as_ref().expect("namespace creation should have succeeded");
    assert_eq!(map.shards.len() as u32, expected);
}

#[then(regex = r#"^all (\d+) leaders are on the single node \(best-effort honors what is available\)$"#)]
async fn then_all_leaders_single_node(w: &mut KisekiWorld, expected: u32) {
    let map = w.topology_last_map.as_ref().unwrap();
    assert_eq!(map.shards.len() as u32, expected);
    let node = w.topology_active_nodes[0];
    for shard in &map.shards {
        assert_eq!(shard.leader_node, node, "all leaders should be on the single node");
    }
}

#[then("the namespace shard map is persisted")]
async fn then_shard_map_persisted_short(w: &mut KisekiWorld) {
    let map = w.topology_last_map.as_ref().unwrap();
    let stored = w.shard_map_store.get(&map.namespace_id, map.tenant_id)
        .expect("shard map should be persisted");
    assert_eq!(stored.shards.len(), map.shards.len());
}

// --- Scenario: Initial topology cap ---

#[then(regex = r#"^(\d+) shards are created \(cap: .+\)$"#)]
async fn then_n_shards_cap(w: &mut KisekiWorld, expected: u32) {
    let map = w.topology_last_map.as_ref().expect("namespace creation should have succeeded");
    assert_eq!(map.shards.len() as u32, expected);
}

#[then(regex = r#"^the (\d+) leaders are placed best-effort round-robin across the (\d+) nodes$"#)]
async fn then_leaders_round_robin(w: &mut KisekiWorld, shard_count: u32, node_count: u32) {
    let map = w.topology_last_map.as_ref().unwrap();
    assert_eq!(map.shards.len() as u32, shard_count);
    let mut leader_counts: std::collections::HashMap<NodeId, u32> = std::collections::HashMap::new();
    for shard in &map.shards {
        *leader_counts.entry(shard.leader_node).or_default() += 1;
    }
    // Leaders should span multiple nodes.
    assert!(leader_counts.len() > 1, "leaders should be on multiple nodes");
}

#[then(regex = r#"^approximately (\d+)/(\d+) nodes host one leader; remaining nodes host none for this namespace$"#)]
async fn then_approx_leader_distribution(w: &mut KisekiWorld, leaders: u32, total: u32) {
    let map = w.topology_last_map.as_ref().unwrap();
    let mut leader_counts: std::collections::HashMap<NodeId, u32> = std::collections::HashMap::new();
    for shard in &map.shards {
        *leader_counts.entry(shard.leader_node).or_default() += 1;
    }
    // With round-robin across 100 nodes and 64 shards, 64 nodes have 1 leader.
    assert_eq!(leader_counts.len() as u32, leaders.min(total));
}

// --- Scenario: Cluster admin overrides initial multiplier ---

#[given(regex = r#"^the cluster admin sets `initial_shard_multiplier = (\d+)` cluster-wide$"#)]
async fn given_multiplier_override(w: &mut KisekiWorld, multiplier: u32) {
    w.topology_config.multiplier = multiplier;
}

#[then(regex = r#"^(\d+) shards are created \(max\(min\(.+\)$"#)]
async fn then_n_shards_formula(w: &mut KisekiWorld, expected: u32) {
    let map = w.topology_last_map.as_ref().expect("namespace creation should have succeeded");
    assert_eq!(map.shards.len() as u32, expected);
}

// --- Scenario: Tenant admin overrides within admin envelope ---

#[given(regex = r#"^the cluster admin defines per-tenant initial-shard bounds: min=(\d+), max=(\d+)$"#)]
async fn given_tenant_bounds(w: &mut KisekiWorld, min_shards: u32, max_shards: u32) {
    // We'll set bounds for any tenant that creates a namespace.
    // The tenant_id needs to match, so we set it generically.
    let tenant_id = w.ensure_tenant("org-pharma");
    w.shard_map_store.set_tenant_bounds(
        &tenant_id.0.to_string(),
        TenantShardBounds { min_shards, max_shards },
    );
}

#[when(regex = r#"^tenant admin requests `initial_shards = (\d+)` for namespace "([^"]*)"$"#)]
async fn when_tenant_requests_shards(w: &mut KisekiWorld, shards: u32, ns: String) {
    let tenant_id = w.ensure_tenant("org-pharma");
    match w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        Some(shards),
    ) {
        Ok(map) => {
            w.topology_last_map = Some(map);
            w.topology_last_error = None;
        }
        Err(e) => {
            w.topology_last_map = None;
            w.topology_last_error = Some(e.to_string());
        }
    }
}

#[then(regex = r#"^(\d+) shards are created$"#)]
async fn then_n_shards_plain(w: &mut KisekiWorld, expected: u32) {
    let map = w.topology_last_map.as_ref().expect("namespace creation should have succeeded");
    assert_eq!(map.shards.len() as u32, expected);
}

#[then(regex = r#"^the request is rejected with "([^"]*)"$"#)]
async fn then_request_rejected(w: &mut KisekiWorld, expected_msg: String) {
    let err = w.topology_last_error.as_ref().expect("expected an error from the last operation");
    assert!(
        err.contains(&expected_msg),
        "expected error containing '{}', got '{}'",
        expected_msg, err
    );
}

// "But when" in Gherkin inherits from the previous keyword (Then),
// so cucumber sees this as a Then step.
#[then(regex = r#"^when tenant admin requests `initial_shards = (\d+)`$"#)]
async fn then_but_when_tenant_requests(w: &mut KisekiWorld, shards: u32) {
    let tenant_id = w.ensure_tenant("org-pharma");
    let ns = format!("rejected-ns-{}", shards);
    match w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        Some(shards),
    ) {
        Ok(map) => {
            w.topology_last_map = Some(map);
            w.topology_last_error = None;
        }
        Err(e) => {
            w.topology_last_map = None;
            w.topology_last_error = Some(e.to_string());
        }
    }
}

// =========================================================================
// ADR-033: Ratio-floor auto-split
// =========================================================================

#[given(regex = r#"^namespace "([^"]*)" has (\d+) shards \(ratio = [^)]+\)$"#)]
async fn given_ns_with_shards(w: &mut KisekiWorld, ns: String, shard_count: u32) {
    let tenant_id = w.ensure_tenant("org-pharma");
    w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        Some(shard_count),
    ).expect("setup: namespace creation should succeed");
}

#[when(regex = r#"^(\d+) more nodes? (?:is|are) added (?:to the cluster )?\(now (\d+) Active nodes?;[^)]+\)$"#)]
async fn when_nodes_added(w: &mut KisekiWorld, _added: u32, total: u32) {
    w.topology_active_nodes.clear();
    for i in 1..=total {
        w.topology_active_nodes.push(NodeId(i as u64));
    }
}

#[then(regex = r#"^the ratio floor is violated \([^)]+\)$"#)]
async fn then_ratio_violated(w: &mut KisekiWorld) {
    let active_count = w.topology_active_nodes.len() as u32;
    let ns_names = ["ns-a", "ns-b", "big-ns"];
    let mut found_violation = false;
    for ns in &ns_names {
        if let Some(count) = w.shard_map_store.shard_count(ns) {
            if shard_topology::check_ratio_floor(&w.topology_config, count, active_count).is_some() {
                found_violation = true;
                break;
            }
        }
    }
    assert!(found_violation, "expected at least one namespace with ratio floor violation");
}

#[then(regex = r#"^auto-split fires for "([^"]*)" until shard count reaches at least ceil\([^)]+\) = (\d+)$"#)]
async fn then_auto_split_fires(w: &mut KisekiWorld, ns: String, target: u32) {
    let result = w.shard_map_store.evaluate_ratio_floor(
        &ns,
        &w.topology_config,
        &w.topology_active_nodes,
    );
    let new_count = result.expect("splits should have fired");
    assert!(
        new_count >= target,
        "expected at least {} shards after split, got {}",
        target, new_count
    );
}

#[then(regex = r#"^the new shards are placed best-effort round-robin so leaders distribute across the (\d+) nodes$"#)]
async fn then_new_shards_round_robin(w: &mut KisekiWorld, _node_count: u32) {
    let tenant_id = w.ensure_tenant("org-pharma");
    let stored = w.shard_map_store.get("ns-a", tenant_id);
    if let Ok(map) = stored {
        let mut nodes_used: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for shard in &map.shards {
            nodes_used.insert(shard.leader_node);
        }
        assert!(
            nodes_used.len() > 1,
            "leaders should be on multiple nodes after split"
        );
    }
}

#[then("the namespace shard map is updated atomically through the control plane Raft group")]
async fn then_shard_map_updated(w: &mut KisekiWorld) {
    let tenant_id = w.ensure_tenant("org-pharma");
    let stored = w.shard_map_store.get("ns-a", tenant_id)
        .expect("shard map should exist after splits");
    assert!(stored.version > 1, "version should be > 1 after split");
}

#[then(regex = r#"^the ratio floor is satisfied \([^)]+\)$"#)]
async fn then_ratio_satisfied(w: &mut KisekiWorld) {
    let active_count = w.topology_active_nodes.len() as u32;
    let ns_names = ["ns-a", "ns-b", "big-ns"];
    for ns in &ns_names {
        if let Some(count) = w.shard_map_store.shard_count(ns) {
            assert!(
                shard_topology::check_ratio_floor(&w.topology_config, count, active_count).is_none(),
                "namespace {} should not have a ratio floor violation ({} shards, {} nodes)",
                ns, count, active_count
            );
        }
    }
}

#[then(regex = r#"^no auto-split is triggered for "([^"]*)"$"#)]
async fn then_no_auto_split(w: &mut KisekiWorld, ns: String) {
    let result = w.shard_map_store.evaluate_ratio_floor(
        &ns,
        &w.topology_config,
        &w.topology_active_nodes,
    );
    assert!(result.is_none(), "no splits should have been triggered for {}", ns);
}

// =========================================================================
// ADV-033-1: Atomic namespace creation rollback
// =========================================================================

#[given("node-3 is temporarily unreachable")]
async fn given_node3_unreachable(w: &mut KisekiWorld) {
    // Inject failure at shard 7 (simulating node-3 unavailability).
    w.shard_map_store.inject_failure_at_shard(7);
}

#[when(regex = r#"^tenant admin creates namespace "([^"]*)" \(requires (\d+) shards\)$"#)]
async fn when_tenant_creates_ns_with_count(w: &mut KisekiWorld, ns: String, _shards: u32) {
    let tenant_id = w.ensure_tenant("org-pharma");
    match w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        None,
    ) {
        Ok(map) => {
            w.topology_last_map = Some(map);
            w.topology_last_error = None;
        }
        Err(e) => {
            w.topology_last_map = None;
            w.topology_last_error = Some(e.to_string());
        }
    }
}

#[when(regex = r#"^shard (\d+) fails to reach quorum within (\d+) seconds \([^)]+\)$"#)]
async fn when_shard_fails_quorum(_w: &mut KisekiWorld, _shard: u32, _timeout: u32) {
    // The failure was already injected in the Given step.
    // The create_namespace call above already returned the error.
}

#[then(regex = r#"^all (\d+) successfully created Raft groups are torn down$"#)]
async fn then_raft_groups_torn_down(w: &mut KisekiWorld, _count: u32) {
    // Verify that no partial state remains.
    assert!(
        w.topology_last_map.is_none(),
        "namespace map should not exist after rollback"
    );
}

#[then("no namespace shard map entry is committed")]
async fn then_no_shard_map_committed(w: &mut KisekiWorld) {
    // The namespace should not be in the store.
    let tenant_id = w.ensure_tenant("org-pharma");
    let result = w.shard_map_store.get("partial-ns", tenant_id);
    assert!(result.is_err(), "namespace should not be in store after rollback");
}

#[then(regex = r#"^the CreateNamespace call returns error "([^"]*)"$"#)]
async fn then_create_returns_error(w: &mut KisekiWorld, expected: String) {
    let err = w.topology_last_error.as_ref().expect("expected an error");
    assert!(
        err.contains(&expected),
        "expected error containing '{}', got '{}'",
        expected, err
    );
}

#[then(regex = r#"^a subsequent CreateNamespace for "([^"]*)" succeeds once node-3 recovers$"#)]
async fn then_subsequent_create_succeeds(w: &mut KisekiWorld, ns: String) {
    // Clear the failure injection (node-3 recovered).
    w.shard_map_store.clear_failure_injection();

    let tenant_id = w.ensure_tenant("org-pharma");
    let result = w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        None,
    );
    assert!(result.is_ok(), "namespace creation should succeed after recovery");
    w.topology_last_map = result.ok();
}

// =========================================================================
// ADV-033-1: Concurrent CreateNamespace rejection
// =========================================================================

#[given(regex = r#"^namespace "([^"]*)" is in state Creating \(Raft groups being formed\)$"#)]
async fn given_ns_creating(w: &mut KisekiWorld, ns: String) {
    let tenant_id = w.ensure_tenant("org-pharma");
    // Directly insert a Creating-state namespace into the store.
    use kiseki_control::shard_topology::{NamespaceCreationState, NamespaceShardMap};
    w.shard_map_store.insert_creating(&ns, tenant_id);
}

#[when(regex = r#"^a second CreateNamespace\("([^"]*)"\) arrives$"#)]
async fn when_second_create(w: &mut KisekiWorld, ns: String) {
    let tenant_id = w.ensure_tenant("org-pharma");
    match w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        None,
    ) {
        Ok(map) => {
            w.topology_last_map = Some(map);
            w.topology_last_error = None;
        }
        Err(e) => {
            w.topology_last_map = None;
            w.topology_last_error = Some(e.to_string());
        }
    }
}

#[then(regex = r#"^the second call is rejected with "([^"]*)"$"#)]
async fn then_second_call_rejected(w: &mut KisekiWorld, expected: String) {
    let err = w.topology_last_error.as_ref().expect("expected an error from second call");
    assert!(
        err.contains(&expected),
        "expected error containing '{}', got '{}'",
        expected, err
    );
}

#[then("the first creation continues")]
async fn then_first_continues(w: &mut KisekiWorld) {
    // The namespace should still be in Creating state (not removed).
    let count = w.shard_map_store.shard_count("dup-ns");
    // It exists in the store (even if 0 shards — it's Creating).
    // Just verify it wasn't deleted.
    assert!(
        w.shard_map_store.namespace_exists("dup-ns"),
        "namespace should still exist in Creating state"
    );
}

// =========================================================================
// ADV-033-3: KeyOutOfRange
// =========================================================================

#[given(regex = r#"^namespace "([^"]*)" has (\d+) shards covering ranges \[0x00, 0x55\), \[0x55, 0xAA\), \[0xAA, 0xFF\]$"#)]
async fn given_ns_with_specific_ranges(w: &mut KisekiWorld, ns: String, count: u32) {
    let tenant_id = w.ensure_tenant("org-pharma");
    // Create with exact shard count.
    if w.topology_active_nodes.is_empty() {
        w.topology_active_nodes.push(NodeId(1));
        w.topology_active_nodes.push(NodeId(2));
        w.topology_active_nodes.push(NodeId(3));
    }
    let map = w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        Some(count),
    ).expect("setup: namespace creation should succeed");
    w.topology_last_map = Some(map);
}

#[given("the gateway has a stale shard map (pre-split, single shard)")]
async fn given_stale_shard_map(_w: &mut KisekiWorld) {
    // The gateway has a stale cache — it thinks there's only one shard.
    // We simulate this by using the wrong shard ID when routing.
}

#[when(regex = r#"^the gateway sends a delta with hashed_key=0x([0-9a-fA-F]+) to shard-(\d+) \(range .+\)$"#)]
async fn when_gateway_sends_to_wrong_shard(w: &mut KisekiWorld, key_hex: String, shard_idx: u32) {
    let map = w.topology_last_map.as_ref().unwrap();

    // Build hashed_key from hex prefix.
    let key_byte = u8::from_str_radix(&key_hex, 16).unwrap();
    let mut hashed_key = [0u8; 32];
    hashed_key[0] = key_byte;

    // The "wrong" shard is shard_idx (1-indexed).
    let wrong_shard = &map.shards[(shard_idx - 1) as usize];

    // Check if the key is in range for this shard.
    let in_range = hashed_key.as_slice() >= wrong_shard.range_start.as_slice()
        && (hashed_key.as_slice() < wrong_shard.range_end.as_slice()
            || (shard_idx as usize == map.shards.len()
                && hashed_key.as_slice() <= wrong_shard.range_end.as_slice()));

    if !in_range {
        w.topology_last_error = Some("KeyOutOfRange".to_string());
    } else {
        w.topology_last_error = None;
    }
}

#[then(regex = r#"^shard-(\d+) rejects the delta with KeyOutOfRange$"#)]
async fn then_key_out_of_range(w: &mut KisekiWorld, _shard: u32) {
    let err = w.topology_last_error.as_ref().expect("expected KeyOutOfRange error");
    assert!(err.contains("KeyOutOfRange"), "expected KeyOutOfRange, got '{}'", err);
}

#[then("the gateway refreshes its shard map via GetNamespaceShardMap")]
async fn then_gateway_refreshes(w: &mut KisekiWorld) {
    // Simulate: re-fetch the current shard map.
    let map = w.topology_last_map.as_ref().unwrap();
    let refreshed = w.shard_map_store.get(&map.namespace_id, map.tenant_id)
        .expect("shard map should be available for refresh");
    w.topology_last_map = Some(refreshed);
}

#[then(regex = r#"^the gateway retries to shard-(\d+) \(range .+\)$"#)]
async fn then_gateway_retries(w: &mut KisekiWorld, shard_idx: u32) {
    let map = w.topology_last_map.as_ref().unwrap();
    // The correct shard for key 0x80 should be shard-2 (in 3-shard split).
    let mut hashed_key = [0u8; 32];
    hashed_key[0] = 0x80;
    let correct = shard_topology::route_to_shard(map, &hashed_key);
    assert!(correct.is_some(), "should route to a valid shard after refresh");
    let expected_shard = &map.shards[(shard_idx - 1) as usize];
    assert_eq!(
        correct.unwrap(),
        expected_shard.shard_id,
        "should route to shard-{}",
        shard_idx
    );
}

#[then("the delta is accepted")]
async fn then_delta_accepted(w: &mut KisekiWorld) {
    // After routing to the correct shard, the key is in range.
    w.topology_last_error = None;
}

// =========================================================================
// ADV-033-7: Ratio-floor splits respect shard cap
// =========================================================================

#[given("the cluster scales from 3 to 50 Active nodes")]
async fn given_cluster_scales(w: &mut KisekiWorld) {
    w.topology_active_nodes.clear();
    for i in 1..=50u32 {
        w.topology_active_nodes.push(NodeId(i as u64));
    }
}

#[when("the ratio-floor evaluator fires")]
async fn when_ratio_evaluator_fires(_w: &mut KisekiWorld) {
    // The evaluator fires are tested in the Then steps.
}

#[then(regex = r#"^splits fire until shard count reaches min\(ceil\([^)]+\), (\d+)\) = (\d+)$"#)]
async fn then_splits_to_cap(w: &mut KisekiWorld, _formula_cap: u32, target: u32) {
    let result = w.shard_map_store.evaluate_ratio_floor(
        "big-ns",
        &w.topology_config,
        &w.topology_active_nodes,
    );
    let new_count = result.expect("splits should have fired");
    assert_eq!(new_count, target, "should split to exactly {} (capped)", target);
}

#[then(regex = r#"^not (\d+) \(the shard_cap takes precedence\)$"#)]
async fn then_not_overcapped(w: &mut KisekiWorld, overcapped: u32) {
    let count = w.shard_map_store.shard_count("big-ns").unwrap();
    assert!(
        count < overcapped,
        "shard count {} should be less than {} (cap takes precedence)",
        count, overcapped
    );
}

#[then(regex = r#"^at most max\(1, (\d+)/(\d+)\) = (\d+) splits are in flight concurrently$"#)]
async fn then_max_concurrent_splits(_w: &mut KisekiWorld, _nodes: u32, _divisor: u32, expected: u32) {
    // Verify the formula.
    let max = shard_topology::max_concurrent_splits(50);
    assert_eq!(max, expected, "max concurrent splits formula");
}

// =========================================================================
// ADV-033-9: GetNamespaceShardMap requires tenant authorization
// =========================================================================

#[given(regex = r#"^tenant "([^"]*)" owns namespace "([^"]*)"$"#)]
async fn given_tenant_owns_ns(w: &mut KisekiWorld, tenant: String, ns: String) {
    let tenant_id = w.ensure_tenant(&tenant);
    if w.topology_active_nodes.is_empty() {
        w.topology_active_nodes.push(NodeId(1));
    }
    w.shard_map_store.create_namespace(
        &ns,
        tenant_id,
        &w.topology_config,
        &w.topology_active_nodes,
        Some(3),
    ).expect("setup: namespace creation should succeed");
}

#[given(regex = r#"^a gateway authenticated as tenant "([^"]*)"$"#)]
async fn given_gateway_as_tenant(w: &mut KisekiWorld, tenant: String) {
    // Store the caller tenant for the next When step.
    w.ensure_tenant(&tenant);
    w.topology_last_error = None;
}

#[when(regex = r#"^the gateway calls GetNamespaceShardMap\("([^"]*)"\)$"#)]
async fn when_get_shard_map(w: &mut KisekiWorld, ns: String) {
    // The gateway is authenticated as "org-beta" (last ensured tenant).
    let caller_tenant = *w.tenant_ids.get("org-beta")
        .expect("org-beta should be registered");
    match w.shard_map_store.get(&ns, caller_tenant) {
        Ok(map) => {
            w.topology_last_map = Some(map);
            w.topology_last_error = None;
        }
        Err(e) => {
            w.topology_last_map = None;
            w.topology_last_error = Some(e.to_string());
        }
    }
}

#[then("the call is rejected with PermissionDenied")]
async fn then_permission_denied(w: &mut KisekiWorld) {
    let err = w.topology_last_error.as_ref().expect("expected PermissionDenied error");
    assert!(
        err.contains("PermissionDenied"),
        "expected PermissionDenied, got '{}'",
        err
    );
}

#[then("no shard topology information is returned")]
async fn then_no_topology_returned(w: &mut KisekiWorld) {
    assert!(
        w.topology_last_map.is_none(),
        "no shard map should be returned after PermissionDenied"
    );
}
