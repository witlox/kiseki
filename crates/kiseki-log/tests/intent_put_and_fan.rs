#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names
)]
//! ADR-047 phase 5c — the producer's quorum intent-write (`put_intent_and_fan`)
//! and the per-shard committer spawn, over the real multiplexed transport.
//!
//! `put_intent_and_fan` is NO-LOSS critical: it must return `Ok` ONLY once the
//! intent is durable on `>= min_acks` replicas (local + remote acks), and `Err`
//! on a shortfall so the gateway never fast-acks a write that is not quorum-
//! durable. These tests stand up real `RaftShardStore` nodes in-process over
//! loopback and assert:
//!
//! - **quorum reached** — on a real 3-node durable shard, `put_intent_and_fan`
//!   succeeds (local + >= 1 peer) and the intent lands on the local store AND
//!   at least one peer's store.
//! - **shortfall is Err** — with no reachable peers (single-node durable shard,
//!   `min_acks = 2`), only the local copy lands, the call returns `Err`, and it
//!   does NOT falsely succeed.
//! - **non-durable guard** — an in-memory (`data_dir = None`) intent store
//!   refuses `put_intent_and_fan` (F-P5b-rpc-1) without writing.
//! - **committer spawn drains to the log** — with `decoupled_ack` on and a
//!   durable single-node shard, an intent put via `put_intent_and_fan` is
//!   incorporated into the Raft log by the spawned committer within a short
//!   poll (producer → committer → Raft, end to end on one node-group).
//!
//! Plain `#[test]` (not `#[tokio::test]`): `OpenRaftLogStore` paths internally
//! `tokio::spawn`, and dropping a per-test runtime mid-flight panics. Each test
//! drives its async section through an explicit runtime `block_on`, matching
//! `intent_sync_transport.rs` / `multi_shard_transport.rs`.

use std::collections::BTreeMap;
use std::time::Duration;

use kiseki_common::ids::{ChunkId, NodeId, OrgId, ShardId};
use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
use kiseki_log::delta::OperationType;
use kiseki_log::intent::{IdempotencyKey, PerspectiveSeq, WriteIntent};
use kiseki_log::raft_store::NewChunkMeta;
use kiseki_log::shard::ShardConfig;
use kiseki_log::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};
use kiseki_log::RaftShardStore;

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(0x0475_b0fa_u128))
}

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(0x0475_b07e_u128))
}

fn find_ports(n: usize) -> Vec<u16> {
    (0..n)
        .map(|_| {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        })
        .collect()
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

fn seq(physical_ms: u64, logical: u32, node: u64) -> PerspectiveSeq {
    PerspectiveSeq(HybridLogicalClock {
        physical_ms,
        logical,
        node_id: NodeId(node),
    })
}

/// A non-trivial append so the proto round-trip over the wire is exercised on
/// every field the `intent_put` fan carries.
fn rich_intent(s: PerspectiveSeq, key: Option<IdempotencyKey>) -> WriteIntent {
    WriteIntent {
        perspective_seq: s,
        idempotency_key: key,
        append: AppendChunkAndDeltaRequest {
            delta: AppendDeltaRequest {
                shard_id: test_shard(),
                tenant_id: test_tenant(),
                operation: OperationType::Update,
                timestamp: DeltaTimestamp {
                    hlc: s.0,
                    wall: WallTime {
                        millis_since_epoch: s.0.physical_ms,
                        timezone: "UTC".into(),
                    },
                    quality: ClockQuality::Ptp,
                },
                hashed_key: [0x2bu8; 32],
                chunk_refs: vec![ChunkId([0x11u8; 32]), ChunkId([0x22u8; 32])],
                payload: vec![0xde, 0xad, 0xbe, 0xef],
                has_inline_data: true,
            },
            new_chunks: vec![NewChunkMeta {
                chunk_id: [0x33u8; 32],
                placement: vec![2, 3],
                original_len: 4096,
            }],
        },
    }
}

/// A durable, multi-node `RaftShardStore` on `addr` with its own data dir.
/// `decoupled_ack` arms the committer spawn. `peers` is the full cluster map so
/// membership init makes every node a voter (the voter set `put_intent_and_fan`
/// fans to). The `TempDir` is returned so the caller keeps the data dir alive.
fn spawn_durable_node(
    node_id: u64,
    addr: &str,
    peers: &BTreeMap<u64, String>,
    decoupled_ack: bool,
) -> (RaftShardStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = RaftShardStore::new(
        node_id,
        peers.clone(),
        Some(dir.path().to_path_buf()),
        decoupled_ack,
    );
    store.create_shard(
        test_shard(),
        test_tenant(),
        NodeId(node_id),
        ShardConfig::default(),
        Some(addr),
    );
    (store, dir)
}

/// On a real 3-node durable shard, `put_intent_and_fan` reaches `min_acks` (the
/// default 2 = local + >= 1 peer) and the intent is present on the LOCAL store
/// plus at least one peer's store.
#[test]
fn put_intent_and_fan_reaches_quorum_on_three_nodes() {
    std::env::remove_var("KISEKI_MIN_ACKS"); // default = 2
    let ports = find_ports(3);
    let peers = peers_map(&ports);
    let addrs: Vec<String> = (0..3).map(|i| format!("127.0.0.1:{}", ports[i])).collect();

    let (n1, _d1) = spawn_durable_node(1, &addrs[0], &peers, false);
    let (n2, _d2) = spawn_durable_node(2, &addrs[1], &peers, false);
    let (n3, _d3) = spawn_durable_node(3, &addrs[2], &peers, false);

    // Initialize membership on the seed; wait for election so voter_ids() is
    // populated (the set put_intent_and_fan resolves its fan targets from).
    n1.initialize_shard(test_shard()).expect("init membership");
    std::thread::sleep(Duration::from_secs(4));

    let intent = rich_intent(seq(10, 0, 1), Some([0xa1u8; 16]));
    let rt = make_runtime();
    rt.block_on(n1.put_intent_and_fan(test_shard(), intent.clone()))
        .expect("quorum intent-write should succeed (local + >= 1 peer)");

    // Local copy is present.
    let local = n1.intent_store(test_shard()).unwrap().pending().unwrap();
    assert_eq!(local.len(), 1, "local intent store has the intent");
    assert_eq!(local[0].perspective_seq, intent.perspective_seq);

    // At least one peer received the fanned intent.
    let on_n2 = n2.intent_store(test_shard()).unwrap().pending().unwrap();
    let on_n3 = n3.intent_store(test_shard()).unwrap().pending().unwrap();
    let peer_copies = on_n2.len() + on_n3.len();
    assert!(
        peer_copies >= 1,
        "fanned intent reached >= 1 peer (n2={} n3={})",
        on_n2.len(),
        on_n3.len(),
    );
}

/// With no reachable peers (single-node durable shard, `min_acks = 2`), only the
/// local copy lands, so `put_intent_and_fan` returns `Err` (quorum shortfall)
/// and does NOT falsely succeed. The local copy IS written (one durable copy)
/// but a single copy is below the floor, so the caller MUST NOT ack.
#[test]
fn put_intent_and_fan_errs_on_quorum_shortfall() {
    std::env::set_var("KISEKI_MIN_ACKS", "2");
    let ports = find_ports(1);
    let peers = peers_map(&ports); // a single-node peers map → no fan targets
    let addr = format!("127.0.0.1:{}", ports[0]);

    let (n1, _d1) = spawn_durable_node(1, &addr, &peers, false);
    n1.initialize_shard(test_shard()).expect("init membership");
    std::thread::sleep(Duration::from_secs(2));

    let intent = rich_intent(seq(20, 0, 1), None);
    let rt = make_runtime();
    let res = rt.block_on(n1.put_intent_and_fan(test_shard(), intent));
    assert!(
        res.is_err(),
        "single durable copy < min_acks=2 must NOT report success",
    );
    std::env::remove_var("KISEKI_MIN_ACKS");
}

/// A non-durable (in-memory, `data_dir = None`) intent store refuses
/// `put_intent_and_fan` WITHOUT writing — the F-P5b-rpc-1 obligation (acking on
/// a non-durable intent loses data on crash).
#[test]
fn put_intent_and_fan_refuses_non_durable_store() {
    std::env::set_var("KISEKI_MIN_ACKS", "1");
    let ports = find_ports(1);
    let peers = peers_map(&ports);
    let addr = format!("127.0.0.1:{}", ports[0]);

    // data_dir = None → in-memory intent store → non-durable.
    let n1 = RaftShardStore::new(1, peers, None, true);
    n1.create_shard(
        test_shard(),
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
        Some(&addr),
    );
    n1.initialize_shard(test_shard()).expect("init membership");
    std::thread::sleep(Duration::from_secs(2));

    let intent = rich_intent(seq(30, 0, 1), None);
    let rt = make_runtime();
    let res = rt.block_on(n1.put_intent_and_fan(test_shard(), intent));
    assert!(
        res.is_err(),
        "decoupled-ack must refuse a non-durable intent store",
    );
    // Nothing was written (refused before the local put).
    assert_eq!(
        n1.intent_store(test_shard())
            .unwrap()
            .pending_len()
            .unwrap(),
        0,
        "non-durable refusal writes nothing",
    );
    std::env::remove_var("KISEKI_MIN_ACKS");
}

/// End-to-end producer → committer → Raft: with `decoupled_ack` on, the spawned
/// per-shard committer drains the local intent store into the Raft log.
///
/// The committer's stability watermark is the MAJORITY low-water-mark: it
/// incorporates the local store's intents that a majority of voters have closed
/// below (ADR-047 §3 — phase-5b logic, exercised here through the real spawn).
/// To exercise the spawn → drain → Raft wiring deterministically we put the
/// intents directly on the LEADER's local intent store (so the two peers report
/// nothing pending → FullyClosed watermark → the leader incorporates all). This
/// is exactly the "loop is spawned + drains a pre-populated store" check, plus
/// the assertion that the drained intents land in the shard's Raft log.
///
/// (`put_intent_and_fan`'s quorum semantics are pinned by the dedicated tests
/// above; this one isolates the committer-spawn wiring from the watermark's
/// own convergence timing, which depends on where the fan placed copies.)
#[test]
fn committer_spawn_incorporates_intent_into_the_log() {
    std::env::remove_var("KISEKI_MIN_ACKS");
    let ports = find_ports(3);
    let peers = peers_map(&ports);
    let addrs: Vec<String> = (0..3).map(|i| format!("127.0.0.1:{}", ports[i])).collect();

    // decoupled_ack = true + durable on all three → each spawns a committer.
    let (n1, _d1) = spawn_durable_node(1, &addrs[0], &peers, true);
    let (n2, _d2) = spawn_durable_node(2, &addrs[1], &peers, true);
    let (n3, _d3) = spawn_durable_node(3, &addrs[2], &peers, true);

    n1.initialize_shard(test_shard()).expect("init membership");
    std::thread::sleep(Duration::from_secs(4)); // election + committers running

    // Pre-populate ONLY the leader's local intent store (peers stay empty so
    // they report None → the leader's committer sees a FullyClosed majority
    // watermark and incorporates). This is the spawned committer draining a
    // pre-populated store, end to end into the Raft log.
    let store = n1.intent_store(test_shard()).unwrap();
    store.put(rich_intent(seq(40, 0, 1), None)).unwrap();
    store.put(rich_intent(seq(41, 0, 1), None)).unwrap();

    let rt = make_runtime();
    let mut incorporated = false;
    for _ in 0..240 {
        let drained = store.pending_len().unwrap() == 0;
        let tip = rt
            .block_on(async {
                use kiseki_log::traits::LogOps;
                LogOps::shard_health(&n1, test_shard()).await
            })
            .map_or(0, |i| i.tip.0);
        if drained && tip >= 2 {
            incorporated = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        incorporated,
        "spawned committer should incorporate the pre-populated intents into the Raft log + drain",
    );

    n1.shutdown();
    n2.shutdown();
    n3.shutdown();
}
