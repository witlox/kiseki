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
//! - **committer spawn drains to the log** — on a durable shard the
//!   spawned committer incorporates intents into the Raft log within a
//!   short poll (producer → committer → Raft, end to end on one
//!   node-group).
//!
//! Plain `#[test]` (not `#[tokio::test]`): `OpenRaftLogStore` paths internally
//! `tokio::spawn`, and dropping a per-test runtime mid-flight panics. Each test
//! drives its async section through an explicit runtime `block_on`, matching
//! `intent_sync_transport.rs` / `multi_shard_transport.rs`.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use kiseki_common::ids::{ChunkId, NodeId, OrgId, ShardId};
use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
use kiseki_log::delta::OperationType;
use kiseki_log::intent::{IdempotencyKey, PerspectiveSeq, WriteIntent};
use kiseki_log::raft_store::NewChunkMeta;
use kiseki_log::shard::ShardConfig;
use kiseki_log::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest, LogOps};
use kiseki_log::RaftShardStore;

/// Serializes the tests that mutate the process-global `KISEKI_MIN_ACKS` env
/// var, so a parallel run cannot have one test clear the var while another is
/// mid-construction (the var is read once in `RaftShardStore::new`). Held for
/// the whole body of each env-sensitive test.
static MIN_ACKS_ENV_LOCK: Mutex<()> = Mutex::new(());

/// CI-hang forensics (bdd.yml run 27310159008 + the 06-06 / 06-09 history):
/// `put_intent_and_fan_reaches_quorum_on_three_nodes` TIMED OUT at nextest's
/// 720 s terminate budget with ZERO captured output — libtest buffers panics
/// and prints until test COMPLETION, so a wedge anywhere (including teardown
/// drops during a panic's unwind) leaves nothing to diagnose when nextest
/// SIGTERMs the process. Locally the wedge reproduced exactly once in ~70
/// 2-core `taskset` runs (479 s+, matching the CI signature) and never with a
/// debugger attached.
///
/// The watchdog makes the NEXT occurrence diagnosable from the CI log alone:
/// phase markers stream to stderr as the test progresses (nextest prints the
/// captured stream on timeout/abort), and if the test has not finished inside
/// the budget the watchdog dumps every thread's name + state + kernel wait
/// channel (`/proc/self/task` — readable without ptrace) and ABORTS, so the
/// run fails fast with evidence instead of silently eating the full 720 s.
///
/// Declare it FIRST in the test body so its disarm (`Drop`) runs LAST —
/// after every store/runtime drop — covering teardown wedges too.
struct Watchdog {
    disarm: Arc<(Mutex<bool>, Condvar)>,
    phase: Arc<Mutex<&'static str>>,
}

impl Watchdog {
    fn arm(test: &'static str, budget: Duration) -> Self {
        let disarm = Arc::new((Mutex::new(false), Condvar::new()));
        let phase = Arc::new(Mutex::new("start"));
        let disarm2 = Arc::clone(&disarm);
        let phase2 = Arc::clone(&phase);
        std::thread::Builder::new()
            .name("test-watchdog".into())
            .spawn(move || {
                let (lock, cv) = &*disarm2;
                let deadline = Instant::now() + budget;
                let mut done = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while !*done {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let (guard, _) = cv
                        .wait_timeout(done, deadline - now)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    done = guard;
                }
                if !*done {
                    let ph = *phase2
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    eprintln!(
                        "[watchdog] {test} exceeded {budget:?} wedged in phase '{ph}' — \
                         per-thread dump follows, then abort (fail fast with evidence \
                         instead of nextest's silent 720 s SIGTERM):",
                    );
                    dump_threads();
                    std::process::abort();
                }
            })
            .expect("watchdog thread spawn");
        Self { disarm, phase }
    }

    /// Record + print a progress marker (visible in the CI log on a
    /// timeout/abort, since nextest prints the captured stderr).
    fn phase(&self, p: &'static str) {
        eprintln!("[pif-test] phase: {p}");
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = p;
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let (lock, cv) = &*self.disarm;
        *lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        cv.notify_all();
    }
}

/// Dump every thread's name, scheduler state, and kernel wait channel.
/// `/proc/self/task` needs no ptrace capability, so this works inside
/// the CI sandbox where attaching gdb would not.
fn dump_threads() {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        eprintln!("[watchdog] /proc/self/task unreadable — no thread dump");
        return;
    };
    for t in tasks.flatten() {
        let tid = t.file_name();
        let comm = std::fs::read_to_string(t.path().join("comm")).unwrap_or_default();
        let stat = std::fs::read_to_string(t.path().join("stat")).unwrap_or_default();
        let state = stat.split_whitespace().nth(2).unwrap_or("?").to_owned();
        let wchan = std::fs::read_to_string(t.path().join("wchan")).unwrap_or_default();
        eprintln!(
            "[watchdog]   tid={} state={state} wchan={} name={}",
            tid.to_string_lossy(),
            wchan.trim(),
            comm.trim(),
        );
    }
}

/// #234 pattern (mirrors `intent_sync_transport.rs`): the Raft RPC listener
/// lazy-inits on the store's Raft runtime — under suite load that spawn can
/// lag past the test's first use of the address. Probe-connect until the
/// listener accepts (10 ms backoff, 5 s cap); a zero-byte connection is a
/// benign disconnect to the listener.
fn wait_listening(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::net::TcpStream::connect(addr) {
            Ok(_) => return,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("listener at {addr} never came up: {e}"),
        }
    }
}

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
            inline_payloads: vec![],
        },
    }
}

/// A durable, multi-node `RaftShardStore` on `addr` with its own data dir.
/// `peers` is the full cluster map so membership init makes every node a
/// voter (the voter set `put_intent_and_fan` fans to). The `TempDir` is
/// returned so the caller keeps the data dir alive.
fn spawn_durable_node(
    node_id: u64,
    addr: &str,
    peers: &BTreeMap<u64, String>,
) -> (RaftShardStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = RaftShardStore::new(node_id, peers.clone(), Some(dir.path().to_path_buf()));
    store.create_shard(
        test_shard(),
        test_tenant(),
        NodeId(node_id),
        ShardConfig::default(),
        Some(addr),
    );
    // #234: don't hand the node back until its listener actually accepts —
    // otherwise the seed's election votes (and the intent fan) race the
    // lazy listener spawn under load.
    wait_listening(addr);
    (store, dir)
}

/// On a real 3-node durable shard, `put_intent_and_fan` reaches `min_acks` (the
/// default 2 = local + >= 1 peer) and the intent is present on the LOCAL store
/// plus at least one peer's store.
///
/// Slow: 3-node Raft election (bounded leader-elected poll, up to 120 s under
/// CI's 4-vCPU workspace-parallel starvation — observed 240 s+ historically
/// with the old blind 4 s sleep) then an intent fan-out. Runs in ~1-4 s on a
/// dev box. Picked up by Tier 2 (`make test-slow`) and the BDD smoke gate,
/// where the same invariant (intent fan reaches quorum on a real 3-node
/// shard) is exercised end-to-end against spawned `kiseki-server` children
/// with their own runtimes — no test-thread oversubscription.
///
/// Wedge forensics: this test timed out at nextest's 720 s terminate budget
/// on CI (2/2 on 2026-06-10's bdd.yml runs, sporadic 06-06/06-09) with no
/// captured output; locally it wedged once in ~70 2-core taskset runs. The
/// [`Watchdog`] + phase markers exist so the next occurrence reports WHERE
/// it stuck (named-thread dump) instead of a silent SIGTERM.
#[test]
#[ignore = "slow: 3-node Raft election + intent fan-out; flakes under CI workspace-parallel load"]
fn put_intent_and_fan_reaches_quorum_on_three_nodes() {
    // FIRST declaration → LAST drop: the watchdog stays armed through every
    // store/runtime teardown drop (where a silent wedge is least
    // diagnosable). Budget 600 s — comfortably past the worst observed
    // CI starvation (240 s+) and comfortably before nextest's 720 s
    // terminate, so a wedge fails fast WITH the thread dump.
    let wd = Watchdog::arm(
        "put_intent_and_fan_reaches_quorum_on_three_nodes",
        Duration::from_secs(600),
    );
    let _env = MIN_ACKS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var("KISEKI_MIN_ACKS"); // default = 2
    let ports = find_ports(3);
    let peers = peers_map(&ports);
    let addrs: Vec<String> = (0..3).map(|i| format!("127.0.0.1:{}", ports[i])).collect();

    wd.phase("spawning 3 durable nodes");
    let (n1, _d1) = spawn_durable_node(1, &addrs[0], &peers);
    let (n2, _d2) = spawn_durable_node(2, &addrs[1], &peers);
    let (n3, _d3) = spawn_durable_node(3, &addrs[2], &peers);

    // Initialize membership on the seed, then wait for the election to
    // actually converge instead of a blind 4 s sleep: under CI's 4-vCPU
    // workspace-parallel load the election has been observed to take
    // 240 s+, and a put issued mid-election fails `QuorumLost` — whose
    // `.expect` panic then tears down three starved stores during
    // unwind, the least diagnosable path in this test. Polling the
    // shard's own health metric keeps the invariant the PUT pins
    // untouched (the fan + min_acks floor are still exercised for real).
    wd.phase("initializing membership");
    n1.initialize_shard(test_shard()).expect("init membership");
    wd.phase("waiting for leader election");
    let rt = make_runtime();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match rt.block_on(n1.shard_health(test_shard())) {
            Ok(info) if info.leader.is_some() => break,
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "no shard leader elected within 120s",
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    wd.phase("put_intent_and_fan");
    let intent = rich_intent(seq(10, 0, 1), Some([0xa1u8; 16]));
    rt.block_on(n1.put_intent_and_fan(test_shard(), intent.clone()))
        .expect("quorum intent-write should succeed (local + >= 1 peer)");

    wd.phase("asserting copies");
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
    wd.phase("teardown (store + runtime drops)");
}

/// With no reachable peers (single-node durable shard, `min_acks = 2`), only the
/// local copy lands, so `put_intent_and_fan` returns `Err` (quorum shortfall)
/// and does NOT falsely succeed. The local copy IS written (one durable copy)
/// but a single copy is below the floor, so the caller MUST NOT ack.
#[test]
fn put_intent_and_fan_errs_on_quorum_shortfall() {
    let _env = MIN_ACKS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("KISEKI_MIN_ACKS", "2");
    let ports = find_ports(1);
    let peers = peers_map(&ports); // a single-node peers map → no fan targets
    let addr = format!("127.0.0.1:{}", ports[0]);

    let (n1, _d1) = spawn_durable_node(1, &addr, &peers);
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
    let _env = MIN_ACKS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("KISEKI_MIN_ACKS", "1");
    let ports = find_ports(1);
    let peers = peers_map(&ports);
    let addr = format!("127.0.0.1:{}", ports[0]);

    // data_dir = None → in-memory intent store → non-durable.
    let n1 = RaftShardStore::new(1, peers, None);
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

/// End-to-end producer → committer → Raft (ADR-047 `LeaderSink`): the
/// per-shard committer SUPERVISOR on the leader runs election recovery
/// then drains its local intent store into the Raft log.
///
/// `LeaderSink` is single-incorporator-on-the-leader: no watermark, no peer
/// gossip. The supervisor on the elected leader detects leadership, runs
/// `recover()` once (gathering peers' pending — here empty), then `drain_local`
/// on each tick, draining ALL local pending intents above the F-2 floor. We
/// drive that deterministically by putting the intents directly on the leader's
/// local intent store and asserting they land in the shard's Raft log + the
/// store drains.
///
/// (`put_intent_and_fan`'s quorum semantics — incl. fan-includes-leader — are
/// pinned by the dedicated tests above; this one isolates the supervisor's
/// become-leader → recover → drain wiring.)
///
/// Slow: same shape as `put_intent_and_fan_reaches_quorum_on_three_nodes` —
/// 3-node durable Raft + a 4 s election sleep + a 6 s polling loop for the
/// committer to incorporate. Fast (~5 s) on a dev box, but the 4 s election
/// floor isn't enough under CI's workspace-parallel oversubscription.
#[test]
#[ignore = "slow: 3-node Raft election + committer-supervisor incorporate loop; flakes under CI workspace-parallel load"]
fn committer_spawn_incorporates_intent_into_the_log() {
    let _env = MIN_ACKS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var("KISEKI_MIN_ACKS");
    let ports = find_ports(3);
    let peers = peers_map(&ports);
    let addrs: Vec<String> = (0..3).map(|i| format!("127.0.0.1:{}", ports[i])).collect();

    // Durable on all three → each spawns a supervisor.
    let (n1, _d1) = spawn_durable_node(1, &addrs[0], &peers);
    let (n2, _d2) = spawn_durable_node(2, &addrs[1], &peers);
    let (n3, _d3) = spawn_durable_node(3, &addrs[2], &peers);

    n1.initialize_shard(test_shard()).expect("init membership");
    std::thread::sleep(Duration::from_secs(4)); // election + supervisors running

    // Pre-populate the leader's local intent store. Under LeaderSink the leader
    // is the sole incorporator and drains its OWN store (the fan includes the
    // leader), so its supervisor incorporates these on the next drain tick —
    // end to end into the Raft log. (n1 is the seed and the elected leader.)
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
