#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::items_after_statements
)]
//! GH #230 step-0 probe — the per-shard committer's incorporate-round
//! latency shape, measured locally against a REAL 3-node Raft group
//! (loopback TCP, fjall persistent log, sync-per-write fsync — the
//! exact multi-node production configuration).
//!
//! The issue's premise (from the 2026-06-10 GCP run): one
//! `committer.sink_incorporate` round ≈ 449 ms per ~608-intent batch,
//! "believed dominated by the 100 ms group-commit log-flush interval
//! (`KISEKI_RAFT_FLUSH_INTERVAL_MS`)". This probe tests that premise:
//!
//! 1. **Code fact (no measurement needed):** `KISEKI_RAFT_FLUSH_INTERVAL_MS`
//!    never reaches the multi-node Raft path. It only gates the
//!    single-node `PersistentShardStore` in `kiseki-server/runtime.rs`.
//!    The per-shard `OpenRaftLogStore` opens `FjallRaftLogStore` with
//!    `sync_per_write = true` (one inline `PersistMode::SyncAll` per
//!    openraft append batch) unless `KISEKI_RAFT_FSYNC_WINDOW_US` +
//!    `KISEKI_RAFT_FSYNC_BATCH` opt into the #151 coalescer. There is
//!    NO 100 ms periodic gate in `client_write`'s completion path.
//! 2. **Round latency vs batch size** (serial `append_intents`, the
//!    production sink call): does a 1000-item batch cost ~the same as
//!    a 76-item batch (latency-bound) or scale with items
//!    (bandwidth/apply-bound)?
//! 3. **Pipelining headroom**: same total intents, but submitted with
//!    2–3 rounds in flight (`client_write_ff` + `ProgressResponder`,
//!    single submitter so log order = submission order). The ratio
//!    serial/pipelined is the depth-N lift the #230 fix can claim.
//!
//! `#[ignore]` — manual perf probes, run with:
//! ```sh
//! cargo nextest run -p kiseki-log --run-ignored=only committer_round
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kiseki_common::ids::{NodeId, OrgId, ShardId};
use kiseki_common::time::HybridLogicalClock;
use kiseki_log::raft::OpenRaftLogStore;
use kiseki_log::raft_store::{IncorporateItem, InlinePayloadEntry};
use kiseki_raft::tcp_transport::RaftRpcListener;

fn shard_a() -> ShardId {
    ShardId(uuid::Uuid::from_u128(0xc0c0_c0c0_u128))
}

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(0xe041_0230_u128))
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

/// One intent shaped like the GCP run's: ~512 B delta payload (the
/// composition record), one chunk ref, no inline payload. `seq_ms`
/// keys uniqueness (the SM dedups on perspective-seq, so every probe
/// item MUST have a distinct seq or later rounds no-op).
fn item(seq_ms: u64, payload_bytes: usize, inline_bytes: usize) -> IncorporateItem {
    let mut hashed_key = [0u8; 32];
    hashed_key[..8].copy_from_slice(&seq_ms.to_be_bytes());
    IncorporateItem {
        tenant_id_bytes: *test_tenant().0.as_bytes(),
        operation: 0, // Create
        hashed_key,
        chunk_refs: vec![[0xcd; 32]],
        payload: vec![0xab; payload_bytes],
        has_inline_data: inline_bytes > 0,
        new_chunks: vec![],
        perspective_seq: HybridLogicalClock {
            physical_ms: seq_ms,
            logical: 0,
            node_id: NodeId(1),
        },
        inline_payloads: if inline_bytes > 0 {
            vec![InlinePayloadEntry {
                chunk_id: hashed_key,
                payload: vec![0xee; inline_bytes],
            }]
        } else {
            vec![]
        },
    }
}

fn batch(
    start_ms: u64,
    n: usize,
    payload_bytes: usize,
    inline_bytes: usize,
) -> Vec<IncorporateItem> {
    (0..n as u64)
        .map(|i| item(start_ms + i, payload_bytes, inline_bytes))
        .collect()
}

struct Cluster {
    leader: Arc<OpenRaftLogStore>,
    _followers: Vec<Arc<OpenRaftLogStore>>,
    _tmp: tempfile::TempDir,
}

/// Spin up an N-node Raft group over loopback TCP with fjall
/// persistent log stores — the production multi-node configuration
/// (sync-per-write fsync, no coalescer, no flush interval).
async fn spawn_cluster(nodes: usize) -> Cluster {
    let tmp = tempfile::tempdir().unwrap();
    let ports = find_ports(nodes);
    let peers = peers_map(&ports);
    let mut stores = Vec::with_capacity(nodes);
    for (i, port) in ports.iter().enumerate() {
        let node_id = (i + 1) as u64;
        let listener = RaftRpcListener::new(format!("127.0.0.1:{port}"), None);
        let registry = listener.registry();
        tokio::spawn(async move {
            let _ = listener.run().await;
        });
        let dir = tmp.path().join(format!("n{node_id}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(
            OpenRaftLogStore::new(node_id, shard_a(), test_tenant(), &peers, Some(&dir), None)
                .await
                .unwrap(),
        );
        registry.register_shard(shard_a(), store.raft_handle());
        stores.push(store);
    }
    stores[0].initialize_membership(&peers).await.unwrap();
    // Let the election settle and the leader stabilize.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let leader = Arc::clone(&stores[0]);
    Cluster {
        leader,
        _followers: stores.split_off(1),
        _tmp: tmp,
    }
}

fn stats(label: &str, lat: &mut [Duration], items_per_round: usize) {
    lat.sort_unstable();
    let n = lat.len();
    let mean = lat.iter().sum::<Duration>() / u32::try_from(n).unwrap_or(1);
    let per_s = if mean.as_secs_f64() > 0.0 {
        items_per_round as f64 / mean.as_secs_f64()
    } else {
        f64::INFINITY
    };
    println!(
        "COMMITTER-ROUND {label}: rounds={n} batch={items_per_round} \
         mean={mean:?} p50={:?} min={:?} max={:?} → {per_s:.0} intents/s/shard",
        lat[n / 2],
        lat[0],
        lat[n - 1]
    );
}

/// (2) Serial round latency vs batch size — the production
/// `append_intents` call (one `client_write`, awaited to apply),
/// exactly what `RaftLogIncorporationSink::incorporate` drives.
#[test]
#[ignore = "slow: #230 step-0 perf probe — 3-node Raft round latency vs batch size"]
fn measure_incorporate_round_serial_vs_batch_size() {
    let rt = make_runtime();
    let cluster = rt.block_on(spawn_cluster(3));
    let leader = Arc::clone(&cluster.leader);

    rt.block_on(async move {
        // Warmup — populate connections + steady-state fjall.
        for w in 0..5u64 {
            leader
                .append_intents(batch(1_000 + w * 10, 8, 512, 0))
                .await
                .expect("warmup append_intents");
        }

        let mut base_ms = 1_000_000u64;
        for &size in &[76usize, 152, 304, 608, 1000] {
            let rounds = 20usize;
            let mut lat = Vec::with_capacity(rounds);
            for _ in 0..rounds {
                let b = batch(base_ms, size, 512, 0);
                base_ms += size as u64;
                let t0 = Instant::now();
                leader.append_intents(b).await.expect("append_intents");
                lat.push(t0.elapsed());
            }
            stats("serial payload=512B", &mut lat, size);
        }

        // Inline-payload shape (≤4 KiB objects ride IN the intent):
        // each item carries a 3.5 KiB inline payload — a 608-item
        // batch is a ~2.2 MiB Raft entry.
        for &size in &[152usize, 608] {
            let rounds = 10usize;
            let mut lat = Vec::with_capacity(rounds);
            for _ in 0..rounds {
                let b = batch(base_ms, size, 512, 3_584);
                base_ms += size as u64;
                let t0 = Instant::now();
                leader.append_intents(b).await.expect("append_intents");
                lat.push(t0.elapsed());
            }
            stats("serial payload=512B+3.5KiB-inline", &mut lat, size);
        }
    });
}

/// (4) STEP-2 A/B — the actual production drain paths over the same
/// 3-node cluster + the same preloaded intent backlog:
///
/// - **BEFORE**: legacy serial `ShardCommitter::drain_local`
///   (`Committer::run` → sync `RaftLogIncorporationSink` → one
///   awaited `append_intents` per `DRAIN_BATCH_CAP` batch).
/// - **AFTER**: `PipelinedCommitter::drain_tick` at depth 2 and 3
///   (`client_write_ff` ordered submits, ≤ depth rounds in flight),
///   then `flush_in_flight` so the wall includes the last apply —
///   honest end-to-end comparison.
#[test]
#[ignore = "slow: #230 step-2 A/B probe — legacy serial drain vs pipelined drain"]
fn measure_drain_path_before_vs_after() {
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, WallTime};
    use kiseki_log::committer_pipeline::PipelinedCommitter;
    use kiseki_log::delta::OperationType;
    use kiseki_log::intent::{InMemIntentStore, IntentStore, PerspectiveSeq, WriteIntent};
    use kiseki_log::raft_intent_sink::{IntentLogAppender, RaftLogIncorporationSink};
    use kiseki_log::shard_committer::ShardCommitter;
    use kiseki_log::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};
    use kiseki_log::LogError;

    /// Same role as the private `OpenRaftAppender` in
    /// `raft_shard_store.rs` — forwards the sync sink's appends
    /// through the `Arc`.
    struct ArcAppender(Arc<OpenRaftLogStore>);
    impl IntentLogAppender for ArcAppender {
        async fn append_intents(&self, items: Vec<IncorporateItem>) -> Result<(), LogError> {
            IntentLogAppender::append_intents(&*self.0, items).await
        }
    }

    fn write_intent(seq_ms: u64) -> WriteIntent {
        let mut hashed_key = [0u8; 32];
        hashed_key[..8].copy_from_slice(&seq_ms.to_be_bytes());
        let hlc = HybridLogicalClock {
            physical_ms: seq_ms,
            logical: 0,
            node_id: NodeId(1),
        };
        WriteIntent {
            perspective_seq: PerspectiveSeq(hlc),
            idempotency_key: None,
            append: AppendChunkAndDeltaRequest {
                delta: AppendDeltaRequest {
                    shard_id: shard_a(),
                    tenant_id: test_tenant(),
                    operation: OperationType::Create,
                    timestamp: DeltaTimestamp {
                        hlc,
                        wall: WallTime {
                            millis_since_epoch: seq_ms,
                            timezone: "UTC".into(),
                        },
                        quality: ClockQuality::Ntp,
                    },
                    hashed_key,
                    chunk_refs: vec![kiseki_common::ChunkId([0xcd; 32])],
                    payload: vec![0xab; 512],
                    has_inline_data: false,
                },
                new_chunks: vec![],
                inline_payloads: vec![],
            },
        }
    }

    fn preload(start_ms: u64, n: usize) -> Arc<InMemIntentStore> {
        let store = Arc::new(InMemIntentStore::new());
        for i in 0..n as u64 {
            store.put(write_intent(start_ms + i)).unwrap();
        }
        store
    }

    let rt = make_runtime();
    let cluster = rt.block_on(spawn_cluster(3));
    let leader = Arc::clone(&cluster.leader);
    let handle = rt.handle().clone();

    // Warmup.
    rt.block_on(async {
        leader
            .append_intents(batch(3_000, 8, 512, 0))
            .await
            .expect("warmup");
    });

    const BACKLOG: usize = 15_000; // 15 DRAIN_BATCH_CAP batches.
    let mut base_ms = 3_000_000u64;

    // BEFORE — legacy serial drain, production thread shape (dedicated
    // std::thread driving block_on, sync sink inside).
    {
        let store = preload(base_ms, BACKLOG);
        base_ms += BACKLOG as u64;
        let sink = RaftLogIncorporationSink::new(ArcAppender(Arc::clone(&leader)), handle.clone());
        let mut committer = ShardCommitter::new(store as Arc<dyn IntentStore>, sink, 3, 2);
        let h = handle.clone();
        let t0 = Instant::now();
        std::thread::spawn(move || {
            h.block_on(async move {
                committer.drain_local().expect("legacy drain");
            });
        })
        .join()
        .unwrap();
        let wall = t0.elapsed();
        println!(
            "DRAIN-AB before(serial drain_local): intents={BACKLOG} wall={wall:?} \
             → {:.0} intents/s/shard",
            BACKLOG as f64 / wall.as_secs_f64()
        );
    }

    // AFTER — pipelined drain at depth 2 and 3.
    for &depth in &[2usize, 3] {
        let store = preload(base_ms, BACKLOG);
        base_ms += BACKLOG as u64;
        let leader2 = Arc::clone(&leader);
        let wall = rt.block_on(async move {
            let mut pc = PipelinedCommitter::new(
                store as Arc<dyn IntentStore>,
                leader2,
                depth,
                "probe-shard".into(),
            );
            let t0 = Instant::now();
            let n = pc.drain_tick().await.expect("pipelined drain");
            assert_eq!(n, BACKLOG, "one tick submits the whole backlog");
            pc.flush_in_flight().await;
            t0.elapsed()
        });
        println!(
            "DRAIN-AB after(pipelined depth={depth}): intents={BACKLOG} wall={wall:?} \
             → {:.0} intents/s/shard",
            BACKLOG as f64 / wall.as_secs_f64()
        );
    }
}

/// (3) Pipelined rounds — single submitter, `client_write_ff` +
/// `ProgressResponder` (submission order = log order), depth 1/2/3.
/// Measures TOTAL wall time for the same number of intents; the
/// serial/pipelined ratio is the lift available to the #230 fix.
#[test]
#[ignore = "slow: #230 step-0 perf probe — pipelined incorporate rounds (depth 1-3)"]
fn measure_incorporate_round_pipelined_depth() {
    use kiseki_log::raft_store::LogCommand;
    use openraft::impls::ProgressResponder;

    let rt = make_runtime();
    let cluster = rt.block_on(spawn_cluster(3));
    let leader = Arc::clone(&cluster.leader);

    rt.block_on(async move {
        let raft = leader.raft_handle();
        // Warmup.
        for w in 0..5u64 {
            leader
                .append_intents(batch(2_000 + w * 10, 8, 512, 0))
                .await
                .expect("warmup");
        }

        let mut base_ms = 2_000_000u64;
        let size = 608usize;
        let total_rounds = 24usize;
        for &depth in &[1usize, 2, 3] {
            let t0 = Instant::now();
            let mut in_flight: std::collections::VecDeque<_> = std::collections::VecDeque::new();
            for _ in 0..total_rounds {
                let b = batch(base_ms, size, 512, 0);
                base_ms += size as u64;
                let cmd = LogCommand::IncorporateIntents { items: b };
                let (responder, _commit_rx, rx) = ProgressResponder::new();
                raft.client_write_ff(cmd, Some(responder))
                    .await
                    .expect("client_write_ff");
                in_flight.push_back(rx);
                while in_flight.len() >= depth {
                    let rx = in_flight.pop_front().unwrap();
                    rx.await
                        .expect("responder dropped")
                        .expect("client_write failed");
                }
            }
            while let Some(rx) = in_flight.pop_front() {
                rx.await
                    .expect("responder dropped")
                    .expect("client_write failed");
            }
            let wall = t0.elapsed();
            let total_intents = size * total_rounds;
            println!(
                "COMMITTER-PIPELINE depth={depth}: rounds={total_rounds} batch={size} \
                 wall={wall:?} → {:.0} intents/s/shard",
                total_intents as f64 / wall.as_secs_f64()
            );
        }
    });
}
