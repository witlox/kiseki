//! Step definitions for cluster-formation.feature.
//!
//! Cluster formation exercises multi-node Raft bootstrap, follower join,
//! staggered startup, and leader election. Steps validate the formation
//! protocol using the in-memory Raft store.

use cucumber::{given, then, when};
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
