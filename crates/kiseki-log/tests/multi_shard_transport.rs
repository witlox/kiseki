#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::items_after_statements,
    clippy::cast_precision_loss
)]
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
            n1.append_chunk_and_delta(mk(i), vec![], vec![])
                .await
                .expect("warmup append");
        }
        let mut lat: Vec<Duration> = Vec::new();
        for i in 10..110 {
            let t = std::time::Instant::now();
            n1.append_chunk_and_delta(mk(i), vec![], vec![])
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
            n1.append_chunk_and_delta(mk(i), vec![], vec![])
                .await
                .expect("warmup");
        }
        let mut lat: Vec<Duration> = Vec::new();
        for i in 10..110 {
            let t = std::time::Instant::now();
            n1.append_chunk_and_delta(mk(i), vec![], vec![])
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

/// PERF BISECTION (2026-05-29, #259): round latency through the production
/// `RaftShardStore` wrapper — 2 nodes, single shard. RaftShardStore runs
/// openraft on a DEDICATED runtime (self.rt), so the append crosses a
/// runtime boundary (caller-rt → raft-rt → back), unlike the raw
/// OpenRaftLogStore probe (~1ms, all one runtime). If this jumps toward
/// ~15ms, the cross-runtime hop / dedicated-runtime scheduling is the
/// stacked wait. If still ~1-2ms, the wait needs the FULL-server task
/// population (hydrator/scrub/listeners/other shards) → tokio-console on
/// the real server. `#[ignore]` — manual probe.
#[test]
#[ignore = "perf probe: round latency through RaftShardStore (dedicated runtime)"]
fn measure_round_latency_raftshardstore() {
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
    use kiseki_log::delta::OperationType;
    use kiseki_log::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};

    let ports = find_ports(2);
    let peers = peers_map(&ports);
    let shard = shard_a();
    let a0 = peers[&1].clone();
    let a1 = peers[&2].clone();

    let n1 = RaftShardStore::new(1, peers.clone(), None);
    let n2 = RaftShardStore::new(2, peers.clone(), None);
    n1.create_shard(
        shard,
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
        Some(&a0),
    );
    n2.create_shard(
        shard,
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
        Some(&a1),
    );
    n1.initialize_shard(shard).expect("init");
    std::thread::sleep(Duration::from_secs(4)); // election

    let mk = |i: u64| AppendChunkAndDeltaRequest {
        delta: AppendDeltaRequest {
            shard_id: shard,
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
        },
        new_chunks: vec![],
        inline_payloads: vec![],
    };

    let rt = make_runtime();
    rt.block_on(async {
        for i in 0..10 {
            let _ = LogOps::append_chunk_and_delta(&n1, mk(i)).await;
        }
        let mut lat: Vec<Duration> = Vec::new();
        for i in 10..110 {
            let t = std::time::Instant::now();
            LogOps::append_chunk_and_delta(&n1, mk(i))
                .await
                .expect("timed");
            lat.push(t.elapsed());
        }
        lat.sort_unstable();
        let n = lat.len();
        let mean = lat.iter().sum::<Duration>() / u32::try_from(n).unwrap_or(1);
        println!(
            "ROUND-LATENCY-RAFTSHARDSTORE (2-node, dedicated raft runtime, cross-runtime hop): \
             n={n} mean={:?} p50={:?} min={:?} max={:?}",
            mean,
            lat[n / 2],
            lat[0],
            lat[n - 1]
        );
    });
}

/// PERF ROOT-CAUSE TEST (2026-05-29, #126+#133): the per-shard SM mutex is
/// shared by the write APPLY and the hydrator's `read_deltas`, and
/// read_deltas is O(total deltas) (iterate-all + clone-range) held UNDER
/// that lock. So a hydrator reading a large log stalls every write's
/// apply. This reproduces it without the full server: grow the log, then
/// time writes WITH vs WITHOUT a concurrent read_deltas hammer. If
/// "contended" >> "baseline", the SM-mutex / O(N)-read_deltas contention
/// is the shared-layer write bottleneck. `#[ignore]` — manual probe.
#[test]
#[ignore = "perf probe: write-apply vs hydrator read_deltas SM-mutex contention"]
fn measure_sm_mutex_contention() {
    use kiseki_common::ids::SequenceNumber;
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
    use kiseki_log::delta::OperationType;
    use kiseki_log::traits::{AppendDeltaRequest, ReadDeltasRequest};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

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
        Arc::new(n1)
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
    let stats = |mut v: Vec<Duration>| {
        v.sort_unstable();
        let n = v.len();
        (
            v.iter().sum::<Duration>() / u32::try_from(n).unwrap_or(1),
            v[n / 2],
            v[n - 1],
        )
    };

    // Grow the log to ~1500 deltas so read_deltas([1,tip]) is a big O(N) scan.
    rt.block_on(async {
        for i in 0..1500 {
            n1.append_chunk_and_delta(mk(i), vec![], vec![])
                .await
                .expect("grow");
        }
    });

    // BASELINE: writes with no concurrent reader.
    let base = rt.block_on(async {
        let mut v = Vec::new();
        for i in 1500..1560 {
            let t = std::time::Instant::now();
            n1.append_chunk_and_delta(mk(i), vec![], vec![])
                .await
                .expect("base");
            v.push(t.elapsed());
        }
        v
    });

    // CONTENDED: a tight read_deltas([1, tip]) hammer (what the hydrator does)
    // running concurrently while we time the same writes.
    let stop = Arc::new(AtomicBool::new(false));
    let hammer = {
        let store = Arc::clone(&n1);
        let stop = Arc::clone(&stop);
        rt.spawn(async move {
            let mut reads = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let _ = store
                    .read_deltas(ReadDeltasRequest {
                        shard_id: shard_a(),
                        from: SequenceNumber(1),
                        to: SequenceNumber(5000),
                    })
                    .await;
                reads += 1;
            }
            reads
        })
    };
    let cont = rt.block_on(async {
        let mut v = Vec::new();
        for i in 1560..1620 {
            let t = std::time::Instant::now();
            n1.append_chunk_and_delta(mk(i), vec![], vec![])
                .await
                .expect("cont");
            v.push(t.elapsed());
        }
        v
    });
    stop.store(true, Ordering::Relaxed);
    let reads = rt.block_on(hammer).unwrap_or(0);

    let (bm, bp, bx) = stats(base);
    let (cm, cp, cx) = stats(cont);
    println!(
        "SM-MUTEX-CONTENTION (log~1500 deltas):\n  BASELINE  (no reader): mean={bm:?} p50={bp:?} max={bx:?}\n  CONTENDED (read_deltas hammer, {reads} reads): mean={cm:?} p50={cp:?} max={cx:?}\n  => write slowdown under hydrator-style read: {:.1}x",
        cm.as_secs_f64() / bm.as_secs_f64().max(1e-9)
    );
}

/// Build a `ChunkAndDelta` append for the fan-through probe (free fn so it
/// captures nothing and can be called from spawned tasks).
fn mk_ca(i: u64) -> kiseki_log::traits::AppendChunkAndDeltaRequest {
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
    use kiseki_log::delta::OperationType;
    use kiseki_log::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};
    AppendChunkAndDeltaRequest {
        delta: AppendDeltaRequest {
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
        },
        new_chunks: vec![],
        inline_payloads: vec![],
    }
}

/// PERF PROBE (2026-05-29, #135/#126): is the multi-shard write FORWARD tax
/// the single-ingress fan-through, or the commit round itself? On GCP the
/// forwarded `raft_commit` tier is 50-100ms vs the local tier 1-5ms; since
/// the leader's commit alone ≈ a local commit, the gap should live in the
/// forward path. This drives N concurrent appends AT THE LEADER three ways:
///   DIRECT     — in-process `LogOps::append_chunk_and_delta` (no gRPC) =
///                the bare commit round under concurrency C.
///   GRPC-1CHAN — N concurrent gRPC calls over ONE shared `LogService`
///                channel = single-ingress: one node fans every forward
///                through its one pooled channel to this leader.
///   GRPC-NCHAN — N concurrent gRPC calls, each its OWN channel =
///                distributed ingress / route-to-leader (forwards spread).
///
/// Reading the result:
///   GRPC-1CHAN >> DIRECT && GRPC-NCHAN ≈ DIRECT
///       → single-channel fan-through is the tax → #135 (route-to-leader /
///         distributed ingress) fixes it, durability untouched.
///   GRPC-1CHAN ≈ GRPC-NCHAN >> DIRECT
///       → the gRPC LogService hop itself stacks (not the single channel) →
///         deeper handler/serialization issue.
///   all three ≈
///       → no fan-through cost; the 50-100ms is the commit round under REAL
///         concurrency (loopback too fast to show it) → #126 core, not #135.
/// `#[ignore]` — manual probe, not CI.
#[test]
#[ignore = "perf probe: forward fan-through (1 vs N gRPC channels) vs direct commit"]
fn measure_forward_fanthrough() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use kiseki_log::grpc::{append_chunk_and_delta_request_to_proto, LogGrpc};
    use kiseki_proto::v1::log_service_client::LogServiceClient;
    use kiseki_proto::v1::log_service_server::LogServiceServer;
    use tonic::transport::Server;

    #[derive(Clone, Copy)]
    enum Mode {
        Direct,
        Grpc1,
        GrpcN,
    }

    let ports = find_ports(2);
    let peers = peers_map(&ports);
    let shard = shard_a();
    let a0 = peers[&1].clone();
    let a1 = peers[&2].clone();

    let n1 = Arc::new(RaftShardStore::new(1, peers.clone(), None));
    let n2 = Arc::new(RaftShardStore::new(2, peers.clone(), None));
    n1.create_shard(
        shard,
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
        Some(&a0),
    );
    n2.create_shard(
        shard,
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
        Some(&a1),
    );
    n1.initialize_shard(shard).expect("init");
    std::thread::sleep(Duration::from_secs(4)); // election → n1 leads

    const ITERS: usize = 25;
    let ctr = Arc::new(AtomicU64::new(0));

    let stats = |mut v: Vec<Duration>| -> (Duration, Duration, Duration) {
        if v.is_empty() {
            return (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        }
        v.sort_unstable();
        let n = v.len();
        let mean = v.iter().sum::<Duration>() / u32::try_from(n).unwrap_or(1);
        (mean, v[n / 2], v[(n * 99 / 100).min(n - 1)])
    };

    let rt = make_runtime();
    rt.block_on(async {
        // Serve the LEADER's LogService on an ephemeral port.
        let log_grpc = LogGrpc::new(Arc::clone(&n1) as Arc<dyn LogOps + Send + Sync>);
        // Production serves the data plane via `serve_with_shutdown(addr)`
        // — tonic owns the listener, so TCP_NODELAY defaults ON (tonic
        // 0.14.5 mod.rs:132/656). The proxy client `Channel` is also
        // nodelay-on by default. Match production here so the probe is
        // representative. Set KISEKI_PROBE_NAGLE=1 to DELIBERATELY serve via
        // `serve_with_incoming` on a raw listener — tonic IGNORES tcp_nodelay
        // there (mod.rs:701), reproducing the ~40ms Nagle/delayed-ACK stall.
        // That toggle proves the mechanism; the default path proves prod is
        // not affected.
        let demo_nagle = std::env::var("KISEKI_PROBE_NAGLE").as_deref() == Ok("1");
        let std_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = std_l.local_addr().unwrap();
        drop(std_l); // free the port; tonic / tokio rebinds it below
        let svc = LogServiceServer::new(log_grpc);
        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel::<()>();
        let srv = if demo_nagle {
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            tokio::spawn(async move {
                Server::builder()
                    .add_service(svc)
                    .serve_with_incoming_shutdown(incoming, async {
                        sd_rx.await.ok();
                    })
                    .await
                    .unwrap();
            })
        } else {
            tokio::spawn(async move {
                Server::builder()
                    .add_service(svc)
                    .serve_with_shutdown(addr, async {
                        sd_rx.await.ok();
                    })
                    .await
                    .unwrap();
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let url = format!("http://{addr}");

        // Drive `conc` workers × ITERS ops in `mode`; return (wall, latencies).
        let run = |mode: Mode, conc: usize| {
            let n1 = Arc::clone(&n1);
            let ctr = Arc::clone(&ctr);
            let url = url.clone();
            async move {
                // GRPC-1CHAN: one shared channel, cloned into each worker.
                let shared = match mode {
                    Mode::Grpc1 => Some(LogServiceClient::connect(url.clone()).await.unwrap()),
                    _ => None,
                };
                let start = std::time::Instant::now();
                let mut handles = Vec::with_capacity(conc);
                for _ in 0..conc {
                    let n1 = Arc::clone(&n1);
                    let ctr = Arc::clone(&ctr);
                    let url = url.clone();
                    let shared = shared.clone();
                    handles.push(tokio::spawn(async move {
                        // GRPC-NCHAN: each worker dials its own channel.
                        let mut own = match mode {
                            Mode::GrpcN => {
                                Some(LogServiceClient::connect(url).await.unwrap())
                            }
                            _ => None,
                        };
                        let mut client = match mode {
                            Mode::Grpc1 => shared,
                            _ => None,
                        };
                        let mut lat = Vec::with_capacity(ITERS);
                        for _ in 0..ITERS {
                            let i = ctr.fetch_add(1, Ordering::Relaxed);
                            let t = std::time::Instant::now();
                            match mode {
                                Mode::Direct => {
                                    LogOps::append_chunk_and_delta(n1.as_ref(), mk_ca(i))
                                        .await
                                        .expect("direct append");
                                }
                                Mode::Grpc1 => {
                                    let proto = append_chunk_and_delta_request_to_proto(&mk_ca(i));
                                    client
                                        .as_mut()
                                        .unwrap()
                                        .append_chunk_and_delta(proto)
                                        .await
                                        .expect("grpc-1chan append");
                                }
                                Mode::GrpcN => {
                                    let proto = append_chunk_and_delta_request_to_proto(&mk_ca(i));
                                    own.as_mut()
                                        .unwrap()
                                        .append_chunk_and_delta(proto)
                                        .await
                                        .expect("grpc-nchan append");
                                }
                            }
                            lat.push(t.elapsed());
                        }
                        lat
                    }));
                }
                let mut all = Vec::new();
                for h in handles {
                    all.extend(h.await.unwrap());
                }
                (start.elapsed(), all)
            }
        };

        // warmup the leader / connection setup
        let _ = run(Mode::Direct, 4).await;

        println!("FORWARD-FAN-THROUGH (#135) — 2-node loopback, leader serves LogService:");
        for &conc in &[1usize, 16, 48] {
            for (label, mode) in [
                ("DIRECT    ", Mode::Direct),
                ("GRPC-1CHAN", Mode::Grpc1),
                ("GRPC-NCHAN", Mode::GrpcN),
            ] {
                let (wall, lat) = run(mode, conc).await;
                let ops = (conc * ITERS) as f64;
                let thru = ops / wall.as_secs_f64().max(1e-9);
                let (mean, p50, p99) = stats(lat);
                println!(
                    "  conc={conc:>2} {label}: {thru:>8.0} op/s  mean={mean:?} p50={p50:?} p99={p99:?}",
                );
            }
        }

        // Clean teardown (avoid h2 stream-abort debug_assert).
        let _ = sd_tx.send(());
        let _ = srv.await;
    });
}
