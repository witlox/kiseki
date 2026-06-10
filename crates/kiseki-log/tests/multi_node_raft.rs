#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests: multi-node Raft cluster formation over TCP.
//!
//! These tests spin up real Raft instances with TCP transport to verify
//! correct cluster formation, leader election, and replication.
//! Follows the lattice pattern: seed node calls `initialize()`, followers
//! join by receiving membership via `AppendEntries` RPCs.

use std::collections::BTreeMap;
use std::time::Duration;

use kiseki_common::ids::{OrgId, SequenceNumber, ShardId};
use kiseki_log::raft::OpenRaftLogStore;
use kiseki_log::traits::{AppendDeltaRequest, ReadDeltasRequest};
use kiseki_log::OperationType;
use kiseki_raft::tcp_transport::RaftRpcListener;

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(0xCAFE))
}

/// Spawn the per-node Raft RPC listener and register the test shard's
/// Raft handle. ADR-041 multiplexed transport — listener owns the
/// port, shards register their handles for inbound RPC routing.
fn spawn_listener_with_shard(
    addr: String,
    store: &OpenRaftLogStore,
) -> tokio::task::JoinHandle<()> {
    let raft = store.raft_handle();
    tokio::spawn(async move {
        let listener = RaftRpcListener::new(addr, None);
        listener.registry().register_shard(test_shard(), raft);
        let _ = listener.run().await;
    })
}

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn make_append_req(key_byte: u8) -> AppendDeltaRequest {
    AppendDeltaRequest {
        shard_id: test_shard(),
        tenant_id: test_tenant(),
        operation: OperationType::Create,
        timestamp: kiseki_common::time::DeltaTimestamp {
            hlc: kiseki_common::time::HybridLogicalClock {
                physical_ms: 1000,
                logical: 0,
                node_id: kiseki_common::ids::NodeId(1),
            },
            wall: kiseki_common::time::WallTime {
                millis_since_epoch: 1000,
                timezone: "UTC".into(),
            },
            quality: kiseki_common::time::ClockQuality::Ntp,
        },
        hashed_key: [key_byte; 32],
        chunk_refs: vec![],
        payload: vec![0xab; 64],
        has_inline_data: false,
    }
}

/// Bind TCP listeners and return their ports.
fn find_ports(n: usize) -> Vec<u16> {
    let mut ports = Vec::with_capacity(n);
    for _ in 0..n {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        ports.push(listener.local_addr().unwrap().port());
    }
    ports
}

/// Build peers map: `node_id` (1-based) to `"127.0.0.1:{port}"`.
fn peers_map(ports: &[u16]) -> BTreeMap<u64, String> {
    ports
        .iter()
        .enumerate()
        .map(|(i, port)| ((i + 1) as u64, format!("127.0.0.1:{port}")))
        .collect()
}

// =========================================================================
// Test: 3-node cluster forms correctly with seed + 2 followers
// =========================================================================

#[tokio::test]
async fn three_node_cluster_formation() {
    let ports = find_ports(3);
    let peers = peers_map(&ports);

    // Node 1: seed — calls initialize().
    let node1 = OpenRaftLogStore::new(1, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc1 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[0]), &node1);

    // Node 2: follower — does NOT call initialize().
    let node2 = OpenRaftLogStore::new(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[1]), &node2);

    // Node 3: follower.
    let node3 = OpenRaftLogStore::new(3, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc3 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[2]), &node3);

    // Seed node initializes membership AFTER every listener is up
    // — Vote requests need peers reachable.
    node1.initialize_membership(&peers).await.unwrap();

    // Wait for leader election (need quorum = 2 of 3).
    tokio::time::sleep(Duration::from_secs(4)).await;

    // All nodes should agree on a leader.
    let h1 = node1.shard_health().await;
    let h2 = node2.shard_health().await;
    let h3 = node3.shard_health().await;

    assert!(h1.leader.is_some(), "node1 should see a leader");
    assert!(h2.leader.is_some(), "node2 should see a leader");
    assert!(h3.leader.is_some(), "node3 should see a leader");

    let leader_id = h1.leader.unwrap();
    assert_eq!(
        h2.leader.unwrap(),
        leader_id,
        "all nodes should agree on leader"
    );
    assert_eq!(
        h3.leader.unwrap(),
        leader_id,
        "all nodes should agree on leader"
    );

    // Membership should include all 3 nodes.
    assert_eq!(h1.raft_members.len(), 3, "should have 3 raft members");
}

// =========================================================================
// Test: Writes through leader replicate to followers
// =========================================================================

#[tokio::test]
async fn writes_replicate_to_followers() {
    let ports = find_ports(3);
    let peers = peers_map(&ports);

    let node1 = OpenRaftLogStore::new(1, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc1 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[0]), &node1);

    let node2 = OpenRaftLogStore::new(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[1]), &node2);

    let node3 = OpenRaftLogStore::new(3, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc3 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[2]), &node3);

    node1.initialize_membership(&peers).await.unwrap();

    // Wait for cluster formation + leader election.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Write 5 deltas through the leader.
    for i in 0u8..5 {
        node1.append_delta(make_append_req(i)).await.unwrap();
    }

    // Wait for replication.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Followers should have all 5 deltas.
    let tip2 = node2.current_tip().await;
    let tip3 = node3.current_tip().await;

    assert_eq!(tip2, SequenceNumber(5), "node2 should have all 5 deltas");
    assert_eq!(tip3, SequenceNumber(5), "node3 should have all 5 deltas");

    // Verify data integrity on follower.
    let deltas = node2
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(1),
            to: SequenceNumber(5),
        })
        .await
        .unwrap();
    assert_eq!(deltas.len(), 5);
    for (i, d) in deltas.iter().enumerate() {
        assert_eq!(d.header.sequence, SequenceNumber(i as u64 + 1));
    }
}

// =========================================================================
// Test: Follower joins late and catches up
// =========================================================================

#[tokio::test]
async fn follower_joins_late_and_catches_up() {
    let ports = find_ports(3);
    let peers = peers_map(&ports);

    // Start seed + one follower (quorum = 2/3).
    let node1 = OpenRaftLogStore::new(1, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc1 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[0]), &node1);

    let node2 = OpenRaftLogStore::new(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[1]), &node2);

    node1.initialize_membership(&peers).await.unwrap();

    // Wait for leader election with 2 nodes.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Write data before node 3 joins.
    for i in 0u8..3 {
        node1.append_delta(make_append_req(i)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Now node 3 joins late.
    let node3 = OpenRaftLogStore::new(3, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc3 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[2]), &node3);

    // Wait for node 3 to catch up.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Node 3 should have all 3 deltas.
    let tip3 = node3.current_tip().await;
    assert_eq!(
        tip3,
        SequenceNumber(3),
        "late follower should catch up to tip=3"
    );
}

// =========================================================================
// Test: Follower does not call initialize (no double-init)
// =========================================================================

#[tokio::test]
async fn follower_skips_initialize() {
    let ports = find_ports(2);
    let peers = peers_map(&ports);

    // Seed: initializes.
    let node1 = OpenRaftLogStore::new(1, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc1 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[0]), &node1);

    // Follower: does NOT initialize — should not panic.
    let node2 = OpenRaftLogStore::new(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[1]), &node2);

    node1.initialize_membership(&peers).await.unwrap();

    // Wait for cluster.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Both should be healthy.
    let h1 = node1.shard_health().await;
    let h2 = node2.shard_health().await;

    assert!(h1.leader.is_some(), "node1 should see a leader");
    assert!(h2.leader.is_some(), "node2 should see a leader");
    assert_eq!(h1.leader, h2.leader, "both should agree on leader");
}

// =========================================================================
// Test: Single-node cluster (backward compat)
// =========================================================================

#[tokio::test]
async fn single_node_cluster() {
    // Empty peers → single-node mode (backward compatible).
    // `OpenRaftLogStore::new` no longer initializes membership
    // automatically (Phase B of #4 — see the doc on `new`); the
    // caller handles bootstrap.
    let mut peers = BTreeMap::new();
    peers.insert(1u64, "localhost:9201".to_owned());
    let node = OpenRaftLogStore::new(1, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    node.initialize_membership(&peers).await.unwrap();

    // Should become leader immediately.
    let health = node.shard_health().await;
    assert!(health.leader.is_some(), "single node should be leader");

    // Writes should work.
    let seq = node.append_delta(make_append_req(0x10)).await.unwrap();
    assert_eq!(seq, SequenceNumber(1));
}

// =========================================================================
// ADR-042 §4 — `append_delta_with_forwarding` surfaces leader id on a
// follower's write attempt.
// =========================================================================

/// Two-node Raft cluster. node1 becomes leader; we issue
/// `append_delta_with_forwarding` against the follower (node2) and
/// expect `LogError::ForwardToLeader { leader_node_id = NodeId(1) }`.
///
/// Validates ADR-042 §4 §"Decision — native row" + the new variant's
/// contract documented on
/// `OpenRaftLogStore::append_delta_with_forwarding`.
#[tokio::test]
async fn follower_append_delta_with_forwarding_returns_leader_id() {
    use kiseki_log::error::LogError;

    let ports = find_ports(2);
    let peers = peers_map(&ports);

    let node1 = OpenRaftLogStore::new(1, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc1 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[0]), &node1);

    let node2 = OpenRaftLogStore::new(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[1]), &node2);

    node1.initialize_membership(&peers).await.unwrap();

    // Wait for leader election to converge.
    tokio::time::sleep(Duration::from_secs(4)).await;

    let h1 = node1.shard_health().await;
    let h2 = node2.shard_health().await;
    assert_eq!(h1.leader, h2.leader, "both nodes must agree on leader");
    let leader_id = h1.leader.expect("leader must be elected").0;
    let follower = if leader_id == 1 { &node2 } else { &node1 };
    let leader_expected = kiseki_common::ids::NodeId(leader_id);

    // The follower MUST surface the ForwardToLeader hint with the
    // current leader's node id — the proxy code path uses this to
    // dial the leader directly.
    let result = follower
        .append_delta_with_forwarding(make_append_req(0xAA))
        .await;
    match result {
        Err(LogError::ForwardToLeader { leader_node_id, .. }) => {
            assert_eq!(
                leader_node_id, leader_expected,
                "follower's ForwardToLeader hint must name the actual leader"
            );
        }
        Err(LogError::LeaderUnavailable(_)) => {
            // Acceptable transient: openraft hadn't published the
            // leader hint to this follower yet. Retry once after a
            // longer settle.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let retry = follower
                .append_delta_with_forwarding(make_append_req(0xAB))
                .await;
            match retry {
                Err(LogError::ForwardToLeader { leader_node_id, .. }) => {
                    assert_eq!(leader_node_id, leader_expected);
                }
                other => panic!("after settle, expected ForwardToLeader, got {other:?}"),
            }
        }
        other => panic!("expected ForwardToLeader from follower, got {other:?}"),
    }
}

/// Verify the **legacy** `append_delta` still collapses the
/// forward hint onto `LeaderUnavailable`. This is the
/// backwards-compatibility contract — existing callers
/// (`mem_gateway`, `composition::log_bridge`, etc.) MUST see no
/// behavior change from the ADR-042 §4 work.
#[tokio::test]
async fn follower_legacy_append_delta_still_returns_leader_unavailable() {
    use kiseki_log::error::LogError;

    let ports = find_ports(2);
    let peers = peers_map(&ports);

    let node1 = OpenRaftLogStore::new(1, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc1 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[0]), &node1);

    let node2 = OpenRaftLogStore::new(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[1]), &node2);

    node1.initialize_membership(&peers).await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;

    let h1 = node1.shard_health().await;
    let leader_id = h1.leader.expect("leader must be elected").0;
    let follower = if leader_id == 1 { &node2 } else { &node1 };

    // Legacy method MUST NOT return ForwardToLeader — that variant
    // is reserved for callers that opted in via the _with_forwarding
    // suffix. Existing callers expect LeaderUnavailable.
    let result = follower.append_delta(make_append_req(0xCC)).await;
    assert!(
        matches!(
            result,
            Err(LogError::LeaderUnavailable(_) | LogError::Unavailable)
        ),
        "legacy append_delta on follower must return LeaderUnavailable / Unavailable, got {result:?}"
    );
    assert!(
        !matches!(result, Err(LogError::ForwardToLeader { .. })),
        "legacy append_delta MUST NOT surface ForwardToLeader (backwards compat)"
    );
}

/// Leader's `append_delta_with_forwarding` MUST commit normally
/// (same shape as legacy `append_delta`). The new method only
/// changes the **error** mapping, never the success path.
#[tokio::test]
async fn leader_append_delta_with_forwarding_commits_normally() {
    let ports = find_ports(2);
    let peers = peers_map(&ports);

    let node1 = OpenRaftLogStore::new(1, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc1 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[0]), &node1);

    let node2 = OpenRaftLogStore::new(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = spawn_listener_with_shard(format!("127.0.0.1:{}", ports[1]), &node2);

    node1.initialize_membership(&peers).await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;

    let h1 = node1.shard_health().await;
    let leader_id = h1.leader.expect("leader must be elected").0;
    let leader = if leader_id == 1 { &node1 } else { &node2 };

    let seq = leader
        .append_delta_with_forwarding(make_append_req(0xDD))
        .await
        .expect("leader writes must succeed");
    assert_eq!(seq, SequenceNumber(1));
}

// =========================================================================
// P3 / I-L4 — supervisor-driven delta pruning: each node reports its
// hydrator position LOCALLY; the shard leader's supervisor gathers all
// voters' positions and proposes min as ONE replicated AdvanceWatermark,
// whose apply prunes deltas on every replica deterministically.
// =========================================================================

/// 3-node `RaftShardStore` cluster (the production wrapper — node-local
/// report map + `consumer_positions` aux dispatcher + per-shard
/// supervisor, all wired by `create_shard`).
///
/// Plain `#[test]` (not `#[tokio::test]`) because `RaftShardStore` owns
/// its own tokio runtime; nesting it inside an outer runtime panics on
/// drop (same convention as `raft_shard_store_topology.rs`).
///
/// Asserts the full ADR-040-§D6.3-safety contract:
/// 1. writes applied, two of three voters reported → NO pruning (the
///    round requires EVERY voter);
/// 2. the third voter reports a LOWER position → boundary advances to
///    exactly that min on ALL THREE nodes, never past it, and
///    `earliest_visible_seq == boundary` after the prune;
/// 3. the slow voter catches up → boundary follows to the new min.
#[test]
#[allow(clippy::too_many_lines)] // one cluster lifetime, three sequential phases — splitting would re-pay the ~10 s election per phase
fn supervisor_prunes_deltas_only_after_all_voters_reported_min() {
    use kiseki_log::traits::{LogOps, HYDRATOR_CONSUMER};
    use kiseki_log::RaftShardStore;

    // Process-local env (safe under nextest's process-per-test):
    // shrink the watermark-advance round to 300 ms (6 supervisor
    // ticks) and the per-store Raft runtime to 2 threads so three
    // stores fit one CI process.
    std::env::set_var("KISEKI_WATERMARK_ADVANCE_INTERVAL_MS", "300");
    std::env::set_var("KISEKI_RAFT_THREADS", "2");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let ports = find_ports(3);
    let peers = peers_map(&ports);
    let shard = test_shard();

    // data_dir per node: the per-shard supervisor (the watermark
    // gather lives on its tick loop) only spawns for shards whose
    // intent store opened durably.
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let stores: Vec<RaftShardStore> = (0..3usize)
        .map(|i| {
            let node_id = u64::try_from(i).unwrap() + 1;
            let store =
                RaftShardStore::new(node_id, peers.clone(), Some(dirs[i].path().to_path_buf()));
            let addr = format!("127.0.0.1:{}", ports[i]);
            store.create_shard(
                shard,
                test_tenant(),
                kiseki_common::ids::NodeId(node_id),
                kiseki_log::shard::ShardConfig::default(),
                Some(&addr),
            );
            store
        })
        .collect();
    stores[0].initialize_shard(shard).expect("initialize");

    // Wait for leader election (quorum 2/3) — poll all nodes agree.
    let elected = rt.block_on(async {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let mut leaders = Vec::new();
            for s in &stores {
                leaders.push(s.shard_health(shard).await.ok().and_then(|h| h.leader));
            }
            if leaders.iter().all(|l| l.is_some() && *l == leaders[0]) {
                return true;
            }
            if std::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    assert!(elected, "3-node cluster failed to elect a leader");

    // Append 10 deltas through the leader.
    rt.block_on(async {
        let leader_id = usize::try_from(
            stores[0]
                .shard_health(shard)
                .await
                .unwrap()
                .leader
                .unwrap()
                .0,
        )
        .unwrap();
        let leader = &stores[leader_id - 1];
        for i in 0u8..10 {
            leader.append_delta(make_append_req(i)).await.unwrap();
        }
        // Wait for replication to all followers.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut tips = Vec::new();
            for s in &stores {
                tips.push(s.shard_health(shard).await.unwrap().tip.0);
            }
            if tips.iter().all(|t| *t == 10) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "replication stalled: tips {tips:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    // Poll helper: (gc_boundary, earliest_visible_seq) per node.
    let snapshot = |rt: &tokio::runtime::Runtime| -> Vec<(u64, u64)> {
        rt.block_on(async {
            let mut out = Vec::new();
            for s in &stores {
                out.push((
                    s.gc_boundary(shard).await.unwrap().0,
                    s.earliest_visible_seq(shard).await.unwrap().0,
                ));
            }
            out
        })
    };

    // --- Phase 1: only 2 of 3 voters reported → NO pruning ----------
    stores[0].report_consumer_position(shard, HYDRATOR_CONSUMER, SequenceNumber(10));
    stores[1].report_consumer_position(shard, HYDRATOR_CONSUMER, SequenceNumber(10));
    // Give the leader several advance rounds (300 ms each) to
    // (incorrectly) advance.
    rt.block_on(async { tokio::time::sleep(Duration::from_secs(2)).await });
    for (node, (boundary, earliest)) in snapshot(&rt).iter().enumerate() {
        assert_eq!(
            *boundary,
            0,
            "node {} pruned with a voter missing (boundary {boundary}) — \
             the round must require EVERY voter",
            node + 1,
        );
        assert_eq!(
            *earliest,
            1,
            "node {}: deltas must be intact while a voter is unreported",
            node + 1,
        );
    }

    // --- Phase 2: third voter reports LOWER (4) → boundary == min ---
    stores[2].report_consumer_position(shard, HYDRATOR_CONSUMER, SequenceNumber(4));
    let converged = rt.block_on(async {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let mut ok = true;
            for s in &stores {
                let b = s.gc_boundary(shard).await.unwrap().0;
                assert!(
                    b <= 4,
                    "boundary {b} advanced past the slowest voter's position (4)"
                );
                ok &= b == 4;
            }
            if ok {
                return true;
            }
            if std::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    assert!(
        converged,
        "boundary never converged to the min (4) on all nodes"
    );
    for (node, (boundary, earliest)) in snapshot(&rt).iter().enumerate() {
        assert_eq!(
            *boundary,
            4,
            "node {}: boundary pinned at the min",
            node + 1
        );
        assert_eq!(
            *earliest,
            4,
            "node {}: earliest_visible_seq must equal the boundary after \
             the prune (deltas 1-3 gone, 4 survives)",
            node + 1,
        );
    }

    // --- Phase 3: the slow voter catches up → boundary follows ------
    stores[2].report_consumer_position(shard, HYDRATOR_CONSUMER, SequenceNumber(10));
    let converged = rt.block_on(async {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let mut ok = true;
            for s in &stores {
                let b = s.gc_boundary(shard).await.unwrap().0;
                assert!(b <= 10, "boundary {b} advanced past every voter's position");
                ok &= b == 10;
            }
            if ok {
                return true;
            }
            if std::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    assert!(converged, "boundary never followed the catch-up to 10");
    for (node, (boundary, earliest)) in snapshot(&rt).iter().enumerate() {
        assert_eq!(*boundary, 10, "node {}", node + 1);
        assert_eq!(
            *earliest,
            10,
            "node {}: prune is `sequence < boundary` — delta 10 survives \
             and is the earliest visible",
            node + 1,
        );
    }
}
