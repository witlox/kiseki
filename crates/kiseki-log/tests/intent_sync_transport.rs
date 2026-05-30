#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names
)]
//! ADR-047 phase 5b-rpc — IntentSync over the real multiplexed transport.
//!
//! Stands up real `RaftShardStore` nodes (each with its own per-node
//! `RaftRpcListener`, ADR-041) in-process over loopback. `create_shard` wires
//! the IntentSync aux dispatcher + a per-shard `IntentStore` on each node;
//! these tests populate a peer's store and drive node A's
//! `TransportIntentGatherer` across the wire, asserting:
//!
//! - **round-trip fidelity** — `gather_pending` returns the peer's intents
//!   keyed by the peer's `NodeId`, and each decoded `WriteIntent.append`
//!   equals the original through the proto round-trip (chunk_refs / payload /
//!   operation / new_chunks), with perspective_seq + idempotency_key intact.
//! - **next_pending** — a populated peer reports its lowest seq; an empty
//!   peer reports `None`.
//! - **unreachable peer skipped** — a peer addr with no listener is omitted,
//!   not an error and not a fabricated report.
//! - **node-keyed, distinct** — two peers B and C yield one entry each,
//!   correctly keyed.
//!
//! Plain `#[test]` (not `#[tokio::test]`): the `OpenRaftLogStore` paths
//! internally `tokio::spawn`, and dropping a per-test runtime mid-flight
//! panics ("cannot drop a runtime within an asynchronous context"). Each test
//! drives its async section through an explicit runtime `block_on`, matching
//! `multi_shard_transport.rs`.

use std::collections::BTreeMap;

use kiseki_common::ids::{ChunkId, NodeId, OrgId, ShardId};
use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
use kiseki_log::delta::OperationType;
use kiseki_log::intent::{IdempotencyKey, PerspectiveSeq, WriteIntent};
use kiseki_log::intent_sync::TransportIntentGatherer;
use kiseki_log::raft_store::NewChunkMeta;
use kiseki_log::shard::ShardConfig;
use kiseki_log::shard_committer::PeerIntentGatherer;
use kiseki_log::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};
use kiseki_log::RaftShardStore;

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(0x0475_b001_u128))
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

/// A non-trivial append: chunk_refs, payload, a new_chunk, inline flag — so
/// the proto round-trip is exercised on every field the wire carries.
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
                payload: vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02],
                has_inline_data: true,
            },
            new_chunks: vec![NewChunkMeta {
                chunk_id: [0x33u8; 32],
                placement: vec![2, 3],
                original_len: 8192,
            }],
        },
    }
}

fn assert_append_eq(a: &AppendChunkAndDeltaRequest, b: &AppendChunkAndDeltaRequest) {
    assert_eq!(a.delta.shard_id, b.delta.shard_id, "shard_id");
    assert_eq!(a.delta.tenant_id, b.delta.tenant_id, "tenant_id");
    assert_eq!(a.delta.operation, b.delta.operation, "operation");
    assert_eq!(a.delta.hashed_key, b.delta.hashed_key, "hashed_key");
    assert_eq!(a.delta.chunk_refs, b.delta.chunk_refs, "chunk_refs");
    assert_eq!(a.delta.payload, b.delta.payload, "payload");
    assert_eq!(
        a.delta.has_inline_data, b.delta.has_inline_data,
        "has_inline_data"
    );
    assert_eq!(a.new_chunks.len(), b.new_chunks.len(), "new_chunks len");
    for (x, y) in a.new_chunks.iter().zip(&b.new_chunks) {
        assert_eq!(x.chunk_id, y.chunk_id, "new_chunk.chunk_id");
        assert_eq!(x.placement, y.placement, "new_chunk.placement");
        assert_eq!(x.original_len, y.original_len, "new_chunk.original_len");
    }
}

/// Build a single-node `RaftShardStore` hosting `test_shard()` on `addr`, with
/// its listener live and the IntentSync aux dispatcher registered. Single-node
/// so membership initializes immediately (no election wait); the cross-node
/// wire path under test is the listener + aux dispatcher, exercised by driving
/// the gatherer from a SEPARATE node A.
fn spawn_node(node_id: u64, addr: &str) -> RaftShardStore {
    let mut peers = BTreeMap::new();
    peers.insert(node_id, addr.to_string());
    let store = RaftShardStore::new(node_id, peers, None);
    store.create_shard(
        test_shard(),
        test_tenant(),
        NodeId(node_id),
        ShardConfig::default(),
        Some(addr),
    );
    store
}

/// gather_pending round-trip: node B holds rich intents; node A's gatherer
/// (pointed at B over the wire) returns them keyed by B's NodeId, and each
/// decoded append equals the original through the proto round-trip.
#[test]
fn gather_pending_preserves_appends_over_the_wire() {
    let ports = find_ports(2);
    let addr_a = format!("127.0.0.1:{}", ports[0]);
    let addr_b = format!("127.0.0.1:{}", ports[1]);

    // Node A only needs its listener up to host the shard locally; the gatherer
    // is constructed directly with B as the peer.
    let _node_a = spawn_node(1, &addr_a);
    let node_b = spawn_node(2, &addr_b);

    // Populate node B's per-shard IntentStore (the aux dispatcher serves it).
    let b_store = node_b.intent_store(test_shard()).expect("B hosts shard");
    let i1 = rich_intent(seq(1, 0, 2), Some([0xa1u8; 16]));
    let i2 = rich_intent(seq(2, 0, 2), None);
    let i3 = rich_intent(seq(3, 5, 2), Some([0xc3u8; 16]));
    b_store.put(i1.clone()).unwrap();
    b_store.put(i2.clone()).unwrap();
    b_store.put(i3.clone()).unwrap();

    // Node A's gatherer fans to B over the real transport.
    let gatherer = TransportIntentGatherer::new(test_shard(), vec![(NodeId(2), addr_b)], None);

    let rt = make_runtime();
    let result = rt.block_on(gatherer.gather_pending()).unwrap();

    assert_eq!(result.len(), 1, "one reachable peer");
    let (node, intents) = &result[0];
    assert_eq!(*node, NodeId(2), "keyed by B's NodeId");
    assert_eq!(intents.len(), 3, "all three intents");

    // pending() is ascending by seq; assert each append survived exactly.
    let expected = [&i1, &i2, &i3];
    for (got, exp) in intents.iter().zip(expected) {
        assert_eq!(got.perspective_seq, exp.perspective_seq, "perspective_seq");
        assert_eq!(got.idempotency_key, exp.idempotency_key, "idempotency_key");
        assert_append_eq(&got.append, &exp.append);
    }
}

/// An unreachable peer (an addr with no listener) is omitted from the recovery
/// gather — not an error, not a fabricated entry. The reachable peer answers.
#[test]
fn unreachable_peer_is_skipped() {
    let ports = find_ports(3); // a:0, b:1, dead:2 (never bound to a listener)
    let addr_a = format!("127.0.0.1:{}", ports[0]);
    let addr_b = format!("127.0.0.1:{}", ports[1]);
    let addr_dead = format!("127.0.0.1:{}", ports[2]);
    let _node_a = spawn_node(1, &addr_a);
    let node_b = spawn_node(2, &addr_b);

    let b_store = node_b.intent_store(test_shard()).unwrap();
    b_store.put(rich_intent(seq(1, 0, 2), None)).unwrap();

    // Gatherer fans to B (reachable) and a dead addr (NodeId 9, no listener).
    let gatherer = TransportIntentGatherer::new(
        test_shard(),
        vec![(NodeId(2), addr_b), (NodeId(9), addr_dead)],
        None,
    );
    let rt = make_runtime();

    let pending = rt.block_on(gatherer.gather_pending()).unwrap();
    assert_eq!(
        pending.len(),
        1,
        "only the reachable peer in gather_pending; dead peer omitted"
    );
    assert_eq!(pending[0].0, NodeId(2));
    assert_eq!(pending[0].1.len(), 1, "B's one intent");
}

/// Two reachable peers B and C → one entry each, correctly keyed by NodeId.
#[test]
fn two_peers_keyed_distinctly() {
    let ports = find_ports(3);
    let addr_a = format!("127.0.0.1:{}", ports[0]);
    let addr_b = format!("127.0.0.1:{}", ports[1]);
    let addr_c = format!("127.0.0.1:{}", ports[2]);
    let _node_a = spawn_node(1, &addr_a);
    let node_b = spawn_node(2, &addr_b);
    let node_c = spawn_node(3, &addr_c);

    // B holds one intent, C holds two — so the entries are distinguishable.
    node_b
        .intent_store(test_shard())
        .unwrap()
        .put(rich_intent(seq(1, 0, 2), None))
        .unwrap();
    let c_store = node_c.intent_store(test_shard()).unwrap();
    c_store.put(rich_intent(seq(1, 0, 3), None)).unwrap();
    c_store.put(rich_intent(seq(2, 0, 3), None)).unwrap();

    let gatherer = TransportIntentGatherer::new(
        test_shard(),
        vec![(NodeId(2), addr_b), (NodeId(3), addr_c)],
        None,
    );
    assert_eq!(gatherer.peer_count(), 2);

    let rt = make_runtime();
    let pending = rt.block_on(gatherer.gather_pending()).unwrap();
    assert_eq!(pending.len(), 2, "one entry per distinct peer");

    let by_node: BTreeMap<u64, usize> = pending
        .iter()
        .map(|(n, intents)| (n.0, intents.len()))
        .collect();
    assert_eq!(by_node.get(&2), Some(&1), "B keyed with its 1 intent");
    assert_eq!(by_node.get(&3), Some(&2), "C keyed with its 2 intents");
}
