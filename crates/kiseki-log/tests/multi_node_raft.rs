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

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(0xCAFE))
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
    let _rpc1 = node1.spawn_rpc_server(format!("127.0.0.1:{}", ports[0]));

    // Node 2: follower — does NOT call initialize().
    let node2 = OpenRaftLogStore::new_follower(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = node2.spawn_rpc_server(format!("127.0.0.1:{}", ports[1]));

    // Node 3: follower.
    let node3 = OpenRaftLogStore::new_follower(3, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc3 = node3.spawn_rpc_server(format!("127.0.0.1:{}", ports[2]));

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
    let _rpc1 = node1.spawn_rpc_server(format!("127.0.0.1:{}", ports[0]));

    let node2 = OpenRaftLogStore::new_follower(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = node2.spawn_rpc_server(format!("127.0.0.1:{}", ports[1]));

    let node3 = OpenRaftLogStore::new_follower(3, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc3 = node3.spawn_rpc_server(format!("127.0.0.1:{}", ports[2]));

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
    let _rpc1 = node1.spawn_rpc_server(format!("127.0.0.1:{}", ports[0]));

    let node2 = OpenRaftLogStore::new_follower(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = node2.spawn_rpc_server(format!("127.0.0.1:{}", ports[1]));

    // Wait for leader election with 2 nodes.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Write data before node 3 joins.
    for i in 0u8..3 {
        node1.append_delta(make_append_req(i)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Now node 3 joins late.
    let node3 = OpenRaftLogStore::new_follower(3, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc3 = node3.spawn_rpc_server(format!("127.0.0.1:{}", ports[2]));

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
    let _rpc1 = node1.spawn_rpc_server(format!("127.0.0.1:{}", ports[0]));

    // Follower: does NOT initialize — should not panic.
    let node2 = OpenRaftLogStore::new_follower(2, test_shard(), test_tenant(), &peers, None, None)
        .await
        .unwrap();
    let _rpc2 = node2.spawn_rpc_server(format!("127.0.0.1:{}", ports[1]));

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
    let node = OpenRaftLogStore::new(1, test_shard(), test_tenant(), &BTreeMap::new(), None, None)
        .await
        .unwrap();

    // Should become leader immediately.
    let health = node.shard_health().await;
    assert!(health.leader.is_some(), "single node should be leader");

    // Writes should work.
    let seq = node.append_delta(make_append_req(0x10)).await.unwrap();
    assert_eq!(seq, SequenceNumber(1));
}
