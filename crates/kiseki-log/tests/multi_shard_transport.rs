#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Multi-shard transport integration test — ADR-041.
//!
//! 2 nodes × 2 shards per node, sharing one Raft RPC port per node.
//! Pre-ADR-041 the second shard's call to `spawn_rpc_server` on a
//! port already used by the first shard fails silently (`EADDRINUSE`
//! in a backgrounded `tokio::spawn`'d task), so the cross-node Raft
//! group for shard 2 never forms — neither node sees a leader for
//! shard 2 within the election timeout.
//!
//! After ADR-041, the per-node `RaftRpcListener` multiplexes both
//! shards' RPCs on a single port; both groups elect leaders.
//!
//! Test uses plain `#[test]` (not `#[tokio::test]`) because the
//! `OpenRaftLogStore` paths internally `tokio::spawn` and dropping
//! the per-test runtime mid-flight causes "Cannot drop a runtime
//! within an asynchronous context" panics.

use std::collections::BTreeMap;
use std::time::Duration;

use kiseki_common::ids::{NodeId, OrgId, ShardId};
use kiseki_log::raft::OpenRaftLogStore;
use kiseki_log::shard::ShardConfig;
use kiseki_log::traits::LogOps;
use kiseki_log::RaftShardStore;
use kiseki_raft::tcp_transport::RaftRpcListener;

fn shard_a() -> ShardId {
    ShardId(uuid::Uuid::from_u128(0xa1a1_a1a1_u128))
}

fn shard_b() -> ShardId {
    ShardId(uuid::Uuid::from_u128(0xb2b2_b2b2_u128))
}

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(0xe041_0001_u128))
}

fn find_ports(n: usize) -> Vec<u16> {
    let mut ports = Vec::with_capacity(n);
    for _ in 0..n {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        ports.push(listener.local_addr().unwrap().port());
    }
    ports
}

fn peers_map(ports: &[u16]) -> BTreeMap<u64, String> {
    ports
        .iter()
        .enumerate()
        .map(|(i, port)| ((i + 1) as u64, format!("127.0.0.1:{port}")))
        .collect()
}

fn make_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Both shards' Raft groups must reach quorum on a 2-node cluster
/// when each node hosts both shards sharing one port. Pins the core
/// ADR-041 multiplexing contract: pre-fix, the second shard's
/// listener fails to bind, no cross-node RPCs flow, and shard B
/// elects no leader within the election timeout.
#[test]
fn both_shards_reach_quorum_when_sharing_a_single_port_per_node() {
    let rt = make_runtime();
    let ports = find_ports(2);
    let peers = peers_map(&ports);

    let stores = rt.block_on(async {
        // Per-node multiplexed listener — ADR-041. Each node has ONE
        // listener; shards register their Raft handles into the
        // registry. Pre-ADR-041 this required two listeners per
        // node (one per shard) and the second bind hit EADDRINUSE.
        let listener_n1 = RaftRpcListener::new(format!("127.0.0.1:{}", ports[0]), None);
        let registry_n1 = listener_n1.registry();
        tokio::spawn(async move {
            let _ = listener_n1.run().await;
        });

        let listener_n2 = RaftRpcListener::new(format!("127.0.0.1:{}", ports[1]), None);
        let registry_n2 = listener_n2.registry();
        tokio::spawn(async move {
            let _ = listener_n2.run().await;
        });

        // Node 1, Shard A — seed.
        let n1a = OpenRaftLogStore::new(1, shard_a(), test_tenant(), &peers, None, None)
            .await
            .unwrap();
        registry_n1.register_shard(shard_a(), n1a.raft_handle());

        // Node 1, Shard B — seed.
        let n1b = OpenRaftLogStore::new(1, shard_b(), test_tenant(), &peers, None, None)
            .await
            .unwrap();
        registry_n1.register_shard(shard_b(), n1b.raft_handle());

        // Node 2, Shard A — follower.
        let n2a = OpenRaftLogStore::new(2, shard_a(), test_tenant(), &peers, None, None)
            .await
            .unwrap();
        registry_n2.register_shard(shard_a(), n2a.raft_handle());

        // Node 2, Shard B — follower.
        let n2b = OpenRaftLogStore::new(2, shard_b(), test_tenant(), &peers, None, None)
            .await
            .unwrap();
        registry_n2.register_shard(shard_b(), n2b.raft_handle());

        // Seed initializes both shards now that every replica's
        // listener is up and the per-shard Raft handles are
        // registered with their respective registry handles.
        n1a.initialize_membership(&peers).await.unwrap();
        n1b.initialize_membership(&peers).await.unwrap();

        // Wait for elections (need 2-of-2 quorum on each shard).
        tokio::time::sleep(Duration::from_secs(4)).await;
        (n1a, n1b, n2a, n2b)
    });

    let (node1_a, node1_b, node2_a, node2_b) = stores;
    let h_node1_shard_a = rt.block_on(node1_a.shard_health());
    let h_node1_shard_b = rt.block_on(node1_b.shard_health());
    let h_node2_shard_a = rt.block_on(node2_a.shard_health());
    let h_node2_shard_b = rt.block_on(node2_b.shard_health());

    // Shard A on both nodes — works pre- and post-fix because A's
    // listener bound first.
    assert!(
        h_node1_shard_a.leader.is_some(),
        "shard A node 1: no leader"
    );
    assert!(
        h_node2_shard_a.leader.is_some(),
        "shard A node 2: no leader"
    );

    // Shard B on both nodes — fails pre-ADR-041 because the second
    // spawn_rpc_server() on each node's port hits EADDRINUSE silently.
    // Cross-node messages for shard B never arrive; election never
    // completes.
    assert!(
        h_node1_shard_b.leader.is_some(),
        "shard B node 1: no leader — pre-ADR-041 the second \
         spawn_rpc_server() on port {} hit EADDRINUSE silently. \
         Cross-node Raft messages for shard B never arrived. ADR-041 \
         multiplexes both shards on a single per-node port; with the \
         multiplexed listener, shard B should elect just like shard A.",
        ports[0],
    );
    assert!(
        h_node2_shard_b.leader.is_some(),
        "shard B node 2: no leader — same root cause (second listener \
         on port {} fails to bind).",
        ports[1],
    );
}

/// End-to-end split: `RaftShardStore::split_shard` creates a brand
/// new Raft group, registers it with the multiplexed listener, and
/// the new shard reaches a leader. Pre-ADR-041 the new shard's
/// listener bind would have hit `EADDRINUSE` and the new shard's
/// Raft group would never form. This pins that the ADR-033 §3 split
/// path is functionally unblocked.
///
/// Single-node `RaftShardStore` for simplicity — the multiplexing
/// behavior is in the listener, not the membership; the cross-node
/// case is already covered by the test above.
#[test]
fn split_shard_creates_new_raft_group_via_multiplexed_listener() {
    let rt = make_runtime();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };

    let mut peers = BTreeMap::new();
    peers.insert(1u64, format!("127.0.0.1:{port}"));

    let store = RaftShardStore::new(1, peers, None);
    let original = ShardId(uuid::Uuid::from_u128(0x5d11_0001_u128));
    store.create_shard(
        original,
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
        Some(&format!("127.0.0.1:{port}")),
    );
    // Phase B of #4: `create_shard` no longer initializes — the
    // caller drives membership setup explicitly. Single-node init
    // is fast (no peers to wait for).
    store
        .initialize_shard(original)
        .expect("initialize original");
    rt.block_on(async { tokio::time::sleep(Duration::from_secs(2)).await });

    // Verify original shard has a leader.
    let info = rt.block_on(store.shard_health(original)).expect("original");
    assert!(
        info.leader.is_some(),
        "original shard should have a leader before split",
    );

    // Trigger the split — `RaftShardStore::split_shard` internally
    // calls the inherent `create_shard` (which goes through the
    // SAME multiplexed listener) for the new shard.
    let new_shard = ShardId(uuid::Uuid::from_u128(0x5d11_0002_u128));
    let result = LogOps::split_shard(&store, original, new_shard, NodeId(1));
    assert!(
        result.is_ok(),
        "split_shard returned {result:?}; expected Ok — the new shard's \
         Raft group should have been created via the multiplexed \
         listener without EADDRINUSE.",
    );
    rt.block_on(async { tokio::time::sleep(Duration::from_secs(2)).await });

    // Both shards should now have leaders. Pre-ADR-041 the new
    // shard's listener bind would have failed; this assertion
    // exercises the ADR-041 → ADR-033 unblock chain.
    let new_info = rt
        .block_on(store.shard_health(new_shard))
        .expect("new shard");
    assert!(
        new_info.leader.is_some(),
        "new shard from split has no leader — its Raft group never \
         formed. The multiplexed listener should have accepted the \
         second `create_shard` call without EADDRINUSE.",
    );
    let original_info = rt.block_on(store.shard_health(original)).expect("original");
    assert!(
        original_info.leader.is_some(),
        "original shard lost its leader during split",
    );
}

/// PERF PROBE (2026-05-29, #259): measure the openraft `client_write`
/// round latency in ISOLATION — 2 real nodes over loopback TCP, single
/// shard, but NONE of the gateway/chunk/fjall/composition load that the
/// full kiseki-server carries. Single-node full path = 240µs; 3-node
/// full path = ~15ms. The only thing added is this replication round.
/// If THIS prints ~sub-ms, the full-path 15ms is co-location/load on the
/// 3 heavy servers; if it prints ~15ms, the openraft round is inherently
/// that slow on loopback. `#[ignore]` — manual probe, not CI.
#[test]
#[ignore = "perf probe: openraft single-round latency"]
fn measure_openraft_round_latency() {
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
    use kiseki_log::delta::OperationType;
    use kiseki_log::traits::AppendDeltaRequest;

    let rt = make_runtime();
    let ports = find_ports(2);
    let peers = peers_map(&ports);
    let (n1, _n2) = rt.block_on(async {
        let l1 = RaftRpcListener::new(format!("127.0.0.1:{}", ports[0]), None);
        let r1 = l1.registry();
        tokio::spawn(async move {
            let _ = l1.run().await;
        });
        let l2 = RaftRpcListener::new(format!("127.0.0.1:{}", ports[1]), None);
        let r2 = l2.registry();
        tokio::spawn(async move {
            let _ = l2.run().await;
        });
        let n1 = OpenRaftLogStore::new(1, shard_a(), test_tenant(), &peers, None, None)
            .await
            .unwrap();
        r1.register_shard(shard_a(), n1.raft_handle());
        let n2 = OpenRaftLogStore::new(2, shard_a(), test_tenant(), &peers, None, None)
            .await
            .unwrap();
        r2.register_shard(shard_a(), n2.raft_handle());
        n1.initialize_membership(&peers).await.unwrap();
        tokio::time::sleep(Duration::from_secs(4)).await;
        (n1, n2)
    });

    let mk = |i: u64| AppendDeltaRequest {
        shard_id: shard_a(),
        tenant_id: test_tenant(),
        operation: OperationType::Create,
        timestamp: DeltaTimestamp {
            hlc: HybridLogicalClock {
                physical_ms: 1000,
                logical: 0,
                node_id: NodeId(1),
            },
            wall: WallTime {
                millis_since_epoch: 1000,
                timezone: "UTC".into(),
            },
            quality: ClockQuality::Ntp,
        },
        hashed_key: [(i % 251) as u8; 32],
        chunk_refs: vec![],
        payload: vec![0xab; 32],
        has_inline_data: false,
    };

    rt.block_on(async {
        // warmup
        for i in 0..10 {
            n1.append_chunk_and_delta(mk(i), vec![])
                .await
                .expect("warmup append");
        }
        let mut lat: Vec<Duration> = Vec::new();
        for i in 10..110 {
            let t = std::time::Instant::now();
            n1.append_chunk_and_delta(mk(i), vec![])
                .await
                .expect("timed append");
            lat.push(t.elapsed());
        }
        lat.sort_unstable();
        let n = lat.len();
        let mean = lat.iter().sum::<Duration>() / u32::try_from(n).unwrap_or(1);
        println!(
            "OPENRAFT-ROUND-LATENCY (2-node loopback TCP, no gateway/chunk/fjall load): \
             n={n} mean={:?} p50={:?} min={:?} max={:?}",
            mean,
            lat[n / 2],
            lat[0],
            lat[n - 1]
        );
    });
}

/// PERF BISECTION (2026-05-29, #259 follow-up): same 2-node round probe,
/// but with the FJALL persistent log (data_dir=Some) + the server's
/// KISEKI_RAFT_FLUSH_INTERVAL_MS=100, vs the in-memory probe above. If
/// fjall makes the round jump from ~1ms toward ~15ms, the stacked wait is
/// in the log persistence / io-flush-tracking path. If it stays ~1-2ms,
/// fjall is NOT it and the wait is in the RaftShardStore dedicated-runtime
/// / full-server task population. `#[ignore]` — manual probe.
#[test]
#[ignore = "perf probe: openraft round latency with fjall persistent log"]
fn measure_openraft_round_latency_fjall() {
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
    use kiseki_log::delta::OperationType;
    use kiseki_log::traits::AppendDeltaRequest;
    use std::path::PathBuf;

    // Match the GCP/compose server: periodic flush every 100ms.
    std::env::set_var("KISEKI_RAFT_FLUSH_INTERVAL_MS", "100");
    let base = std::env::temp_dir().join(format!("kiseki-round-probe-{}", uuid::Uuid::new_v4()));
    let d1: PathBuf = base.join("n1");
    let d2: PathBuf = base.join("n2");
    std::fs::create_dir_all(&d1).unwrap();
    std::fs::create_dir_all(&d2).unwrap();

    let rt = make_runtime();
    let ports = find_ports(2);
    let peers = peers_map(&ports);
    let n1 = rt.block_on(async {
        let l1 = RaftRpcListener::new(format!("127.0.0.1:{}", ports[0]), None);
        let r1 = l1.registry();
        tokio::spawn(async move {
            let _ = l1.run().await;
        });
        let l2 = RaftRpcListener::new(format!("127.0.0.1:{}", ports[1]), None);
        let r2 = l2.registry();
        tokio::spawn(async move {
            let _ = l2.run().await;
        });
        let n1 = OpenRaftLogStore::new(1, shard_a(), test_tenant(), &peers, Some(&d1), None)
            .await
            .unwrap();
        r1.register_shard(shard_a(), n1.raft_handle());
        let n2 = OpenRaftLogStore::new(2, shard_a(), test_tenant(), &peers, Some(&d2), None)
            .await
            .unwrap();
        r2.register_shard(shard_a(), n2.raft_handle());
        n1.initialize_membership(&peers).await.unwrap();
        tokio::time::sleep(Duration::from_secs(4)).await;
        n1
    });

    let mk = |i: u64| AppendDeltaRequest {
        shard_id: shard_a(),
        tenant_id: test_tenant(),
        operation: OperationType::Create,
        timestamp: DeltaTimestamp {
            hlc: HybridLogicalClock {
                physical_ms: 1000,
                logical: 0,
                node_id: NodeId(1),
            },
            wall: WallTime {
                millis_since_epoch: 1000,
                timezone: "UTC".into(),
            },
            quality: ClockQuality::Ntp,
        },
        hashed_key: [(i % 251) as u8; 32],
        chunk_refs: vec![],
        payload: vec![0xab; 32],
        has_inline_data: false,
    };
    rt.block_on(async {
        for i in 0..10 {
            n1.append_chunk_and_delta(mk(i), vec![])
                .await
                .expect("warmup");
        }
        let mut lat: Vec<Duration> = Vec::new();
        for i in 10..110 {
            let t = std::time::Instant::now();
            n1.append_chunk_and_delta(mk(i), vec![])
                .await
                .expect("timed");
            lat.push(t.elapsed());
        }
        lat.sort_unstable();
        let n = lat.len();
        let mean = lat.iter().sum::<Duration>() / u32::try_from(n).unwrap_or(1);
        println!(
            "OPENRAFT-ROUND-LATENCY-FJALL (2-node loopback TCP, fjall log, flush=100ms, \
             no gateway/chunk load): n={n} mean={:?} p50={:?} min={:?} max={:?}",
            mean,
            lat[n / 2],
            lat[0],
            lat[n - 1]
        );
    });
    let _ = std::fs::remove_dir_all(&base);
}
