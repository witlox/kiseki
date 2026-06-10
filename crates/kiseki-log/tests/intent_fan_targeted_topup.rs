#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names
)]
//! GH #228 — the targeted min-acks top-up, over the real multiplexed
//! transport.
//!
//! Pre-#228 the coalescer's top-up was an all-remaining-voters broadcast
//! with cancel-on-first-ack: the 2026-06-10 GCP runs measured 39 % of all
//! `intent_put` RPCs redundant (2.64 store-applies/write vs the 2.0
//! `min_acks` needs) and ~1.9 k/s/ingress TCP redial churn from the
//! cancelled pooled streams. The replacement walks ONE rotation-picked
//! candidate at a time under the short `KISEKI_INTENT_TOPUP_TIMEOUT_MS`
//! budget, falling back to the next candidate on timeout/error.
//!
//! These tests drive the [`IntentFanCoalescer`] directly against real
//! single-node `RaftShardStore` peers (each with its own ADR-041
//! listener + IntentSync aux dispatcher) over loopback, and count
//! store-applies per peer — the wire-level truth of how many
//! `intent_put` RPCs landed:
//!
//! - **one extra RPC** — leader-is-local + 3 healthy peers + min_acks=2
//!   stores the intent on EXACTLY one peer (2.0 applies/write); the old
//!   broadcast landed it on all three.
//! - **leader-first fast path preserved** — a remote leader satisfies
//!   quorum on the leader-first hop alone; zero top-up RPCs.
//! - **fallback on dead peer / on timeout** — the first candidate
//!   failing (connection refused, or accepted-but-never-answered) falls
//!   back to the next candidate and min_acks is still met.
//! - **rotation spreads load** — sequential flushes walk the candidate
//!   ring deterministically, one peer per flush.
//! - **total failure refuses** — all candidates dead → `QuorumLost`,
//!   never a false ack.
//!
//! Plain `#[test]` (not `#[tokio::test]`): `RaftShardStore` internally
//! spawns its own runtime, and dropping a per-test runtime mid-flight
//! panics. Each test drives its async section through an explicit
//! runtime `block_on`, matching `intent_sync_transport.rs`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kiseki_common::ids::{ChunkId, NodeId, OrgId, ShardId};
use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
use kiseki_log::delta::OperationType;
use kiseki_log::intent::{InMemIntentStore, IntentStore, PerspectiveSeq, WriteIntent};
use kiseki_log::intent_fan_coalescer::{spawn as spawn_coalescer, CoalescerConfig};
use kiseki_log::raft_store::NewChunkMeta;
use kiseki_log::shard::ShardConfig;
use kiseki_log::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};
use kiseki_log::{LogError, RaftShardStore};

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(0x0228_70b0_u128))
}

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(0x0228_b07e_u128))
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

/// A non-trivial append so the proto round-trip over the wire is
/// exercised on every field the `intent_put` fan carries.
fn rich_intent(s: PerspectiveSeq) -> WriteIntent {
    WriteIntent {
        perspective_seq: s,
        idempotency_key: None,
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
                chunk_refs: vec![ChunkId([0x11u8; 32])],
                payload: vec![0xde, 0xad],
                has_inline_data: true,
            },
            new_chunks: vec![NewChunkMeta {
                chunk_id: [0x33u8; 32],
                placement: vec![2, 3],
                original_len: 4096,
            }],
            inline_payloads: vec![],
        },
    }
}

/// A single-node `RaftShardStore` hosting `test_shard()` on `addr`,
/// listener live + IntentSync aux dispatcher registered (the server
/// half of `intent_put`). Single-node so there is no election wait;
/// the wire path under test is the coalescer's fan INTO this node.
fn spawn_peer(node_id: u64, addr: &str) -> RaftShardStore {
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

/// Block until `addr` accepts TCP connections (the listener bind is
/// async inside `create_shard`; without this the first fan attempt can
/// race the bind and skew the per-peer counts the tests assert).
fn wait_listening(addr: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match std::net::TcpStream::connect(addr) {
            Ok(_) => return,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("listener at {addr} never came up: {e}"),
        }
    }
}

/// Accepts connections and never answers — a "slow peer" whose RPCs
/// only ever end by the caller's timeout. Sockets are held open so the
/// client sees neither EOF nor a reset.
fn spawn_hanging_acceptor() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept() {
            held.push(sock);
        }
    });
    addr
}

/// Build a coalescer over an in-memory local store with a fixed peer
/// set + leader. `min_acks = 2` throughout: local + exactly one peer.
fn coalescer_config(
    local_store: &Arc<dyn IntentStore>,
    peers: Vec<(NodeId, String)>,
    leader_id: Option<u64>,
    topup_rpc_timeout: Duration,
) -> CoalescerConfig {
    CoalescerConfig {
        shard_id: test_shard(),
        local_node: NodeId(1),
        store: Arc::clone(local_store),
        resolver: Arc::new(move || (peers.clone(), leader_id)),
        min_acks: 2,
        cap_max: 16,
        cap_timeout: Duration::from_micros(100),
        peer_rpc_timeout: Duration::from_secs(1),
        topup_rpc_timeout,
    }
}

fn peer_pending(node: &RaftShardStore) -> usize {
    node.intent_store(test_shard())
        .unwrap()
        .pending_len()
        .unwrap()
}

/// Leader-is-local + 3 healthy peers + min_acks=2: the top-up stores
/// the intent on EXACTLY one peer — 2.0 store-applies/write. The
/// pre-#228 broadcast landed it on all three (cancel-on-first-ack
/// notwithstanding, every launched RPC that completed applied).
#[test]
fn targeted_topup_contacts_exactly_one_peer() {
    let ports = find_ports(3);
    let addrs: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();
    let n2 = spawn_peer(2, &addrs[0]);
    let n3 = spawn_peer(3, &addrs[1]);
    let n4 = spawn_peer(4, &addrs[2]);
    for a in &addrs {
        wait_listening(a);
    }

    let local: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
    let peers = vec![
        (NodeId(2), addrs[0].clone()),
        (NodeId(3), addrs[1].clone()),
        (NodeId(4), addrs[2].clone()),
    ];
    let rt = make_runtime();
    let coalescer = spawn_coalescer(
        rt.handle(),
        // leader_id = Some(1) → leader IS local → no leader-first hop;
        // quorum needs exactly one top-up ack.
        coalescer_config(&local, peers, Some(1), Duration::from_millis(500)),
    );
    rt.block_on(coalescer.submit(rich_intent(seq(1, 0, 1))))
        .expect("local + one targeted peer reaches min_acks=2");

    assert_eq!(local.pending_len().unwrap(), 1, "local durable copy");
    let per_peer = [peer_pending(&n2), peer_pending(&n3), peer_pending(&n4)];
    assert_eq!(
        per_peer.iter().sum::<usize>(),
        1,
        "targeted top-up applies on EXACTLY one peer (got {per_peer:?}); \
         the pre-#228 broadcast applied on all 3",
    );
}

/// A remote leader satisfies quorum on the leader-first hop alone —
/// the top-up never fires and the non-leader peers see ZERO RPCs.
#[test]
fn leader_first_quorum_skips_topup_entirely() {
    let ports = find_ports(3);
    let addrs: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();
    let n2 = spawn_peer(2, &addrs[0]);
    let n3 = spawn_peer(3, &addrs[1]);
    let n4 = spawn_peer(4, &addrs[2]);
    for a in &addrs {
        wait_listening(a);
    }

    let local: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
    let peers = vec![
        (NodeId(2), addrs[0].clone()),
        (NodeId(3), addrs[1].clone()),
        (NodeId(4), addrs[2].clone()),
    ];
    let rt = make_runtime();
    let coalescer = spawn_coalescer(
        rt.handle(),
        // leader is node 2 (remote) → MF-3 leader-first hop reaches
        // quorum (local + leader) before any top-up.
        coalescer_config(&local, peers, Some(2), Duration::from_millis(500)),
    );
    rt.block_on(coalescer.submit(rich_intent(seq(2, 0, 1))))
        .expect("local + leader-first reaches min_acks=2");

    assert_eq!(local.pending_len().unwrap(), 1, "local durable copy");
    assert_eq!(peer_pending(&n2), 1, "leader got the leader-first hop");
    assert_eq!(peer_pending(&n3), 0, "no top-up RPC");
    assert_eq!(peer_pending(&n4), 0, "no top-up RPC");
}

/// First candidate dead (connection refused): the walk falls back to
/// the NEXT candidate and min_acks is still met.
#[test]
fn topup_falls_back_when_first_candidate_is_dead() {
    let ports = find_ports(2); // [dead, live]
    let dead_addr = format!("127.0.0.1:{}", ports[0]); // never bound
    let live_addr = format!("127.0.0.1:{}", ports[1]);
    let n3 = spawn_peer(3, &live_addr);
    wait_listening(&live_addr);

    let local: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
    let peers = vec![(NodeId(9), dead_addr), (NodeId(3), live_addr)];
    let rt = make_runtime();
    let coalescer = spawn_coalescer(
        rt.handle(),
        coalescer_config(&local, peers, Some(1), Duration::from_millis(500)),
    );
    rt.block_on(coalescer.submit(rich_intent(seq(3, 0, 1))))
        .expect("fallback to the second candidate still reaches min_acks=2");

    assert_eq!(local.pending_len().unwrap(), 1);
    assert_eq!(peer_pending(&n3), 1, "the fallback candidate applied");
}

/// First candidate hangs (accepted, never answers): the SHORT top-up
/// timeout expires, the walk falls back to the next candidate, and
/// min_acks is still met — a slow peer costs one bounded stall, not
/// the flush.
#[test]
fn topup_falls_back_when_first_candidate_times_out() {
    let hang_addr = spawn_hanging_acceptor();
    let ports = find_ports(1);
    let live_addr = format!("127.0.0.1:{}", ports[0]);
    let n3 = spawn_peer(3, &live_addr);
    wait_listening(&live_addr);

    let local: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
    let peers = vec![(NodeId(9), hang_addr), (NodeId(3), live_addr)];
    let rt = make_runtime();
    let topup_timeout = Duration::from_millis(200);
    let coalescer = spawn_coalescer(
        rt.handle(),
        coalescer_config(&local, peers, Some(1), topup_timeout),
    );
    let started = std::time::Instant::now();
    rt.block_on(coalescer.submit(rich_intent(seq(4, 0, 1))))
        .expect("timeout fallback still reaches min_acks=2");
    let elapsed = started.elapsed();

    assert_eq!(local.pending_len().unwrap(), 1);
    assert_eq!(peer_pending(&n3), 1, "the fallback candidate applied");
    assert!(
        elapsed < Duration::from_secs(3),
        "slow peer costs ~one topup timeout ({topup_timeout:?}), not the \
         3 s peer budget; took {elapsed:?}",
    );
}

/// Sequential flushes rotate the starting candidate deterministically:
/// 6 flushes across 3 healthy peers land exactly 2 intents on each —
/// steady-state top-up load spreads instead of hammering peers[0].
#[test]
fn rotation_spreads_topup_load_across_peers() {
    let ports = find_ports(3);
    let addrs: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();
    let n2 = spawn_peer(2, &addrs[0]);
    let n3 = spawn_peer(3, &addrs[1]);
    let n4 = spawn_peer(4, &addrs[2]);
    for a in &addrs {
        wait_listening(a);
    }

    let local: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
    let peers = vec![
        (NodeId(2), addrs[0].clone()),
        (NodeId(3), addrs[1].clone()),
        (NodeId(4), addrs[2].clone()),
    ];
    let rt = make_runtime();
    let coalescer = spawn_coalescer(
        rt.handle(),
        coalescer_config(&local, peers, Some(1), Duration::from_millis(500)),
    );
    // Sequential submits → one flush each → rotation start walks
    // 0,1,2,0,1,2 across the candidate ring.
    for i in 0..6u32 {
        rt.block_on(coalescer.submit(rich_intent(seq(5, i, 1))))
            .expect("healthy ring always reaches min_acks=2");
    }

    assert_eq!(local.pending_len().unwrap(), 6);
    let per_peer = [peer_pending(&n2), peer_pending(&n3), peer_pending(&n4)];
    assert_eq!(
        per_peer,
        [2, 2, 2],
        "deterministic rotation spreads 6 flushes as 2 per peer",
    );
}

/// Every candidate dead → quorum shortfall is a refusal (`QuorumLost`),
/// never a false ack. The local copy IS written (it precedes the fan)
/// but one copy is below min_acks=2.
#[test]
fn topup_exhausting_all_candidates_refuses_with_quorum_lost() {
    let ports = find_ports(2); // neither ever bound
    let dead_a = format!("127.0.0.1:{}", ports[0]);
    let dead_b = format!("127.0.0.1:{}", ports[1]);

    let local: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
    let peers = vec![(NodeId(8), dead_a), (NodeId(9), dead_b)];
    let rt = make_runtime();
    let coalescer = spawn_coalescer(
        rt.handle(),
        coalescer_config(&local, peers, Some(1), Duration::from_millis(200)),
    );
    let res = rt.block_on(coalescer.submit(rich_intent(seq(6, 0, 1))));
    assert!(
        matches!(res, Err(LogError::QuorumLost(s)) if s == test_shard()),
        "all-candidates-dead must refuse with QuorumLost, got {res:?}",
    );
    assert_eq!(
        local.pending_len().unwrap(),
        1,
        "local copy precedes the fan (one durable copy, below the floor)",
    );
}
