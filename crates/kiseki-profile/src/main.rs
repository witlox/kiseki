#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! Standalone profiling driver for Kiseki data paths.
//!
//! Spawns a real `kiseki-server` (single-node), then drives a
//! configurable concurrent workload against one of S3, `NFSv3`,
//! NFSv4.1, pNFS, or FUSE. Reports throughput + p50/p95/p99
//! latency. Designed to be wrapped by `cargo flamegraph` for CPU
//! profiles and `--features dhat` for heap profiles.
//!
//! Usage:
//!
//! ```text
//!   kiseki-profile run --protocol s3 --shape put-heavy --concurrency 16 \
//!                       --object-size 65536 --duration-secs 30
//! ```
//!
//! Output (stdout, plain):
//!
//! ```text
//!   protocol=s3 shape=put-heavy concurrency=16 object_size=65536
//!   ops=4230 throughput=141.0 op/s 9.65 MiB/s
//!   latency_us p50=84001 p95=178304 p99=251392
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};

mod harness;
mod protocols;
mod stats;

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Which protocol to drive.
#[derive(Copy, Clone, Debug, ValueEnum)]
enum Protocol {
    /// S3 HTTP gateway.
    S3,
    /// `NFSv3` (RFC 1813) over TCP — client-side library.
    Nfs3,
    /// NFSv4.1 (RFC 8881) — single-COMPOUND OPEN+WRITE+COMMIT for
    /// create, COMPOUND OPEN+READ for read.
    Nfs4,
    /// pNFS Flexible Files (RFC 8435) — write via NFSv4.1 to the MDS,
    /// read via the per-stripe DS endpoint advertised by LAYOUTGET.
    Pnfs,
    /// FUSE → `GatewayOps` → S3 wire. Drives `KisekiFuse` against a
    /// `RemoteHttpGateway` connected to the running server.
    Fuse,
    /// In-process gateway floor (ADR-042 §"graduation gate"). Drives
    /// `InMemoryGateway` directly with no server, no IPC, no gRPC, no
    /// HTTP — pure compute path. Measures the upper bound any wire
    /// protocol could possibly serve at this hardware. The
    /// graduation gate from `A-NG11` requires this floor to clear
    /// 100 k op/s 64 KiB GET before ADR-042's protocol shape commits.
    InProcess,
    /// Native gRPC `GatewayDataService` (ADR-042). Drives the
    /// `kiseki.v1.native` service on the harness's data port. The
    /// gRPC tax this measures vs. the in-process floor is what
    /// ADR-042's perf gate (A-NG11: ≥80 k op/s GET, ≥56 k op/s PUT
    /// per node) bounds.
    Native,
    /// In-process gateway with the SAME persistent stores the
    /// spawned `kiseki-server` uses (fjall-backed `CompositionStore`,
    /// raw-block `PersistentChunkStore` with group-commit fsync).
    /// Skips the gRPC + tonic + h2 stack but pays every persistence
    /// cost the production gateway pays. The right floor for the
    /// "transport tax" measurement: the gap between this and
    /// `Native` is whatever the protocol layer adds; the gap
    /// between this and `InProcess` is the persistence tax.
    InProcessPersistent,
}

/// Workload shape — what mix of operations to drive.
#[derive(Copy, Clone, Debug, ValueEnum)]
enum Shape {
    /// 100% creates / writes.
    PutHeavy,
    /// 100% reads of objects pre-populated during a warmup phase.
    GetHeavy,
    /// 70% creates, 30% reads.
    Mixed,
}

/// Native binding selector for `--protocol native`. ADR-042 §2 lists
/// the binding set; `auto` follows server-side ranking
/// (`Rdma > Low > Standard`). For comparing transport-layer cost
/// pin one binding at a time and re-run.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum NativeBinding {
    /// gRPC/h2 over rustls/TCP (ADR-042 §2.1).
    Grpc,
    /// TCP-framed-postcard over rustls/TCP (ADR-042 §2.2). Tuned IP
    /// path; no h2 framing tax.
    Tcp,
    /// Honor `KISEKI_NATIVE_TRANSPORT` (or default to highest-ranked
    /// available — same logic as the server-side `BindingSelector`).
    Auto,
}

#[derive(Parser, Debug)]
#[command(name = "kiseki-profile", about = "Profile Kiseki data paths.")]
enum Cli {
    /// Spawn a server, run the workload, print stats, and exit.
    Run(RunArgs),
}

#[derive(Parser, Debug)]
#[command(
    after_help = "Data dirs default to tempfile::tempdir(), which on most Linux \
distros lands on tmpfs /tmp — there fsync is ~free, so durability/fsync A/Bs (e.g. \
KISEKI_SMALL_OBJECT_FLUSH_INTERVAL_MS / KISEKI_INTENT_FLUSH_INTERVAL_MS arms) measure \
nothing. Set KISEKI_PROFILE_DATA_ROOT to a real-disk path (or TMPDIR) to put per-run \
data dirs on durable media; they are removed on exit like the tempdir."
)]
struct RunArgs {
    /// Which data-path protocol to drive.
    #[arg(long, value_enum)]
    protocol: Protocol,

    /// Workload shape.
    #[arg(long, value_enum)]
    shape: Shape,

    /// Concurrent in-flight ops.
    #[arg(long, default_value_t = 16)]
    concurrency: usize,

    /// Per-object payload size in bytes.
    #[arg(long, default_value_t = 65_536)]
    object_size: usize,

    /// Total wall-clock duration of the measurement phase.
    #[arg(long, default_value_t = 30)]
    duration_secs: u64,

    /// For GetHeavy/Mixed: how many objects to pre-create.
    /// Each get pulls one of these at random.
    #[arg(long, default_value_t = 256)]
    warmup_objects: usize,

    /// Path to the kiseki-server binary. Defaults to
    /// `target/release/kiseki-server` (then `target/debug/kiseki-server`)
    /// next to this profile binary.
    #[arg(long)]
    server_bin: Option<std::path::PathBuf>,

    /// ADR-042 native binding. Only meaningful for
    /// `--protocol native`; ignored for other protocols. Defaults
    /// to `tcp` (TCP-framed-postcard, ADR-042 §2.2) — measured
    /// 36 k PUT / 78 k GET single-host vs gRPC's 21 k / 27 k. Pass
    /// `--binding grpc` to drive the gRPC binding for per-binding
    /// comparison, or `--binding auto` to honor `KISEKI_NATIVE_TRANSPORT`.
    #[arg(long, value_enum, default_value_t = NativeBinding::Tcp)]
    binding: NativeBinding,

    /// Number of `kiseki-server` nodes to spawn. Default 1 (uses the
    /// historic single-node [`ProfileServer`] code path; no behavioral
    /// change vs. pre-`--nodes` invocations). When `>1`, spawns an
    /// N-node local Raft cluster via [`Cluster`], provisions the
    /// `kiseki-bench` namespace + a multi-shard topology (ADR-033 §1
    /// formula: `max(min(3*N, 64), 3)`), and drives the bench
    /// workload against the leader's endpoints. The
    /// `InProcess`/`InProcessPersistent` protocols ignore this flag —
    /// they don't spawn a server at all.
    #[arg(long, default_value_t = 1)]
    nodes: usize,

    /// Path to the kiseki-admin binary (used by `--nodes >1` to
    /// provision the bench namespace + shards). Defaults to
    /// `kiseki-admin` next to `--server-bin`. Ignored for
    /// single-node runs.
    #[arg(long)]
    admin_bin: Option<std::path::PathBuf>,

    /// ADR-048 §"Decision" — when set, create a Replication pool
    /// with `requires_migration = true` named
    /// `slab-ec-bench` BEFORE provisioning the bench namespace,
    /// and wire `namespace.size_band_pools.replicated` at that
    /// pool. The runtime's per-pool slab-EC compactor task picks
    /// it up at boot and migrates chunks to cold-tier slabs
    /// while the workload runs. Ignored on single-node clusters
    /// (`--nodes 1`).
    #[arg(long, default_value_t = false)]
    slab_ec: bool,
}

fn main() {
    #[cfg(feature = "dhat")]
    let _profiler = dhat::Profiler::new_heap();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let Cli::Run(args) = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("kiseki-profile")
        .build()
        .expect("tokio runtime");

    // For the in-process driver, the work happens *inside this
    // process* (not the spawned server), so the server's pprof guard
    // doesn't help. Wrap the run in a local pprof guard when
    // KISEKI_PROFILE_PPROF_OUT is set. Output is the same SVG
    // flamegraph format. Only active for InProcess; no-op for
    // protocols that drive a separately-instrumented server.
    let local_pprof_path = match args.protocol {
        Protocol::InProcess | Protocol::InProcessPersistent => {
            std::env::var("KISEKI_PROFILE_PPROF_OUT").ok()
        }
        _ => None,
    };
    let local_pprof_guard = local_pprof_path.as_ref().and_then(|_| {
        pprof::ProfilerGuardBuilder::default()
            .frequency(99)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .ok()
    });

    rt.block_on(async move {
        if let Err(e) = run(args).await {
            eprintln!("profile run failed: {e}");
            std::process::exit(1);
        }
    });

    if let (Some(guard), Some(path)) = (local_pprof_guard, local_pprof_path) {
        if let Ok(report) = guard.report().build() {
            if let Ok(file) = std::fs::File::create(&path) {
                let _ = report.flamegraph(file);
                eprintln!("[pprof] flamegraph written to {path}");
            }
        }
    }
}

/// Either a single-node `ProfileServer` or an N-node `Cluster`.
/// Holding the harness here keeps the server(s) alive until `run`
/// returns; Drop sends SIGTERM and (for `--features pprof` builds)
/// renders each node's flamegraph. Fields are write-only — the
/// variants exist to keep Drop deferred until the end of `run()`.
#[allow(dead_code)]
enum Harness {
    Single(harness::ProfileServer),
    Cluster(harness::Cluster),
}

/// Spawn the harness for `args.protocol` / `args.nodes` and return
/// the `(Harness, Endpoints)` pair (or `(None, None)` when the
/// protocol drives an in-process gateway and doesn't need a server).
async fn build_harness(
    args: &RunArgs,
) -> Result<(Option<Harness>, Option<protocols::Endpoints>), String> {
    let needs_server = !matches!(
        args.protocol,
        Protocol::InProcess | Protocol::InProcessPersistent
    );
    if !needs_server {
        return Ok((None, None));
    }
    if args.nodes == 1 {
        // Single-node path — unchanged from pre-`--nodes` behavior.
        let s = harness::ProfileServer::start(args.server_bin.as_deref()).await?;
        eprintln!(
            "[harness] single-node up; s3={} nfs={} ds={} metrics={}",
            s.s3_base,
            s.nfs_addr,
            s.ds_addr,
            s.metrics_url(),
        );
        let ep = protocols::Endpoints::from_profile_server(&s);
        return Ok((Some(Harness::Single(s)), Some(ep)));
    }
    // Multi-node path. Each node gets a deterministic per-node
    // pprof output filename when KISEKI_PPROF_OUT is set on the
    // profile process: e.g. `OUT.node1.svg`, `OUT.node2.svg`.
    let pprof_base = std::env::var("KISEKI_PPROF_OUT")
        .ok()
        .map(std::path::PathBuf::from);
    let c = harness::Cluster::start(
        args.nodes,
        args.server_bin.as_deref(),
        pprof_base.as_deref(),
    )
    .await?;
    let shard_count = harness::Cluster::shard_count_for(args.nodes);
    eprintln!(
        "[harness] cluster up; nodes={} leader_node_id={} leader_s3={} leader_tcp_framed={} metrics={}",
        c.node_count(),
        c.leader_node_id(),
        c.leader_s3_base(),
        c.leader_tcp_framed(),
        c.leader_metrics_url(),
    );
    // Per-node metrics URLs — needed by the perf harness to scrape
    // `aux.*` follower histograms (every shard leader currently sits
    // on node 1 per the GH #99 fix, so a leader-only scrape sees no
    // follower work).
    for (nid, url) in c.all_node_metrics_urls() {
        eprintln!("[harness] node {nid} metrics={url}");
    }
    eprintln!(
        "[harness] provisioning bench namespace + {shard_count} shards (ADR-033 §1: max(min(3*N, 64), 3))"
    );
    let slab_pool: Option<&str> = if args.slab_ec {
        Some("slab-ec-bench")
    } else {
        None
    };
    c.provision_bench_topology_with_pool(args.admin_bin.as_deref(), shard_count, slab_pool)
        .await?;
    let ep = protocols::Endpoints::from_cluster_bench(&c);
    Ok((Some(Harness::Cluster(c)), Some(ep)))
}

async fn run(args: RunArgs) -> Result<(), String> {
    if args.nodes == 0 {
        return Err("--nodes must be at least 1".into());
    }
    // _harness_opt: declared so the spawned server(s) live until the
    // function returns. The Drop impl on ProfileServer / Cluster sends
    // SIGTERM and (for `--features pprof` builds) renders the
    // flamegraph SVG, so it MUST survive past the stats output.
    let (_harness_opt, endpoints) = build_harness(&args).await?;

    // Size the NFS connection pool to match concurrency: each
    // worker gets its own session, no FIFO queueing on a shared
    // connection. Capped at 32 to avoid runaway server-side
    // session memory if someone runs at extreme concurrency.
    let pool_size = args.concurrency.clamp(1, 32);
    let driver: Arc<dyn protocols::Driver> =
        protocols::build(args.protocol, args.binding, endpoints.as_ref(), pool_size).await?;

    let warmup_keys = if matches!(args.shape, Shape::PutHeavy) {
        Arc::new(Vec::new())
    } else {
        eprintln!(
            "[warmup] pre-creating {} objects of {} bytes",
            args.warmup_objects, args.object_size,
        );
        // Stamp the object index + a per-run nonce into every warmup
        // buffer. A shared constant-0xa5 payload made all warmup
        // objects dedup server-side to ONE stored object — the GET
        // working set was a single composition, not warmup_objects of
        // them. The nonce keeps re-runs against a persisted store
        // from deduping into the previous run's objects.
        let run_nonce: u64 = uuid::Uuid::new_v4().as_u64_pair().0;
        let mut payload = vec![0xa5u8; args.object_size];
        let mut keys = Vec::with_capacity(args.warmup_objects);
        for i in 0..args.warmup_objects {
            stamp_prefix(&mut payload, i as u64, run_nonce);
            let key = driver
                .put(&payload)
                .await
                .map_err(|e| format!("warmup put: {e}"))?;
            keys.push(key);
        }
        Arc::new(keys)
    };

    eprintln!(
        "[run] protocol={:?} shape={:?} concurrency={} object_size={} duration_secs={}",
        args.protocol, args.shape, args.concurrency, args.object_size, args.duration_secs,
    );

    let payload: Arc<[u8]> = vec![0xa5u8; args.object_size].into();
    let stats = Arc::new(stats::Stats::new());
    let workload_start = Instant::now();
    let deadline = workload_start + Duration::from_secs(args.duration_secs);

    let mut handles = Vec::with_capacity(args.concurrency);
    for worker_id in 0..args.concurrency {
        let driver = driver.clone();
        let payload = payload.clone();
        let warmup_keys = warmup_keys.clone();
        let stats = stats.clone();
        let shape = args.shape;
        handles.push(tokio::spawn(async move {
            worker(
                worker_id,
                driver,
                payload,
                warmup_keys,
                shape,
                stats,
                deadline,
            )
            .await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    // Actual wall clock from workload start to last worker joined —
    // workers overrun the deadline by up to one in-flight op, so the
    // nominal --duration-secs under-states the window and inflates
    // ops/s.
    let elapsed = workload_start.elapsed();
    let report = stats.report(args.object_size, elapsed);

    println!(
        "protocol={:?} shape={:?} concurrency={} object_size={}",
        args.protocol, args.shape, args.concurrency, args.object_size,
    );
    println!(
        "ops={} throughput={:.1} op/s {:.2} MiB/s",
        report.ops, report.ops_per_sec, report.mib_per_sec,
    );
    println!(
        "latency_us p50={} p95={} p99={}",
        report.p50_us, report.p95_us, report.p99_us,
    );
    if report.errors > 0 {
        println!("errors={}", report.errors);
    }
    Ok(())
}

/// Mixed-shape op selector: over every 10 consecutive op indices,
/// 0..6 → PUT and 7..9 → GET — exactly 70/30. The index MUST advance
/// by exactly one per issued op: the previous implementation shared
/// this counter with the GET key cursor, consuming two ticks per GET
/// and skewing the mix to ~75/25.
fn mixed_pick_get(op_index: u64) -> bool {
    (op_index % 10) >= 7
}

/// Stamp `(a, b)` little-endian into the first (up to) 16 bytes of
/// `buf` so the payload's `chunk_id = SHA-256(plaintext)` is unique
/// per stamp. Buffers shorter than 16 bytes get a truncated stamp.
fn stamp_prefix(buf: &mut [u8], a: u64, b: u64) {
    let mut stamp = [0u8; 16];
    stamp[..8].copy_from_slice(&a.to_le_bytes());
    stamp[8..].copy_from_slice(&b.to_le_bytes());
    let n = buf.len().min(16);
    buf[..n].copy_from_slice(&stamp[..n]);
}

async fn worker(
    worker_id: usize,
    driver: Arc<dyn protocols::Driver>,
    payload: Arc<[u8]>,
    warmup_keys: Arc<Vec<protocols::Key>>,
    shape: Shape,
    stats: Arc<stats::Stats>,
    deadline: Instant,
) {
    // op_n drives the Mixed put/get selector and advances exactly
    // once per issued op; get_n is the separate GET key round-robin
    // cursor. Both start at worker_id to stagger workers' phases.
    let mut op_n: u64 = worker_id as u64;
    let mut get_n: u64 = worker_id as u64;
    // Per-worker mutable PUT buffer. Pre-prod the harness used a
    // single shared `Arc<[u8]>` of `0xa5` bytes for every worker on
    // every PUT — same content, same `chunk_id = SHA-256(plaintext)`,
    // and the chunk-store's dedup short-circuit then fires on
    // 99.99 % of writes (replication mode) or skips silently
    // (EC mode pre-`register_ec_chunk` fix). Either way the bench
    // wasn't measuring real writes. We stamp a 16-byte
    // `(worker_id, op_counter, salt)` prefix into a fresh per-worker
    // buffer to make every PUT's content distinct.
    let mut put_buf: Vec<u8> = (*payload).to_vec();
    let salt_nanos: u64 = match std::time::UNIX_EPOCH.elapsed() {
        Ok(d) => u64::try_from(d.as_nanos()).unwrap_or(u64::MAX),
        Err(_) => 0,
    };
    let salt: u64 =
        salt_nanos.wrapping_mul((worker_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut put_n: u64 = 0;
    while Instant::now() < deadline {
        let pick_get = match shape {
            Shape::PutHeavy => false,
            Shape::GetHeavy => true,
            Shape::Mixed => mixed_pick_get(op_n),
        };
        op_n = op_n.wrapping_add(1);
        let start = Instant::now();
        let result = if pick_get {
            if warmup_keys.is_empty() {
                stats.record_error();
                continue;
            }
            let key = &warmup_keys[usize::try_from(get_n).unwrap_or(0) % warmup_keys.len()];
            get_n = get_n.wrapping_add(1);
            driver.get(key).await.map(|_| ())
        } else {
            stamp_prefix(
                &mut put_buf,
                (worker_id as u64).wrapping_shl(40) ^ put_n,
                salt,
            );
            put_n = put_n.wrapping_add(1);
            driver.put(&put_buf).await.map(|_| ())
        };
        let dt = start.elapsed();
        match result {
            Ok(()) => stats.record(dt),
            Err(e) => {
                static FIRST: std::sync::OnceLock<()> = std::sync::OnceLock::new();
                if FIRST.set(()).is_ok() {
                    eprintln!("[error] first failure: {e}");
                }
                stats.record_error();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mixed_pick_get, stamp_prefix};

    #[test]
    fn mixed_shape_is_exactly_70_30() {
        // Period is 10, so any 1000-op window — regardless of the
        // worker's staggered start — must contain exactly 300 GETs.
        for start in [0u64, 1, 3, 7, 9, 12_345] {
            let gets = (start..start + 1000).filter(|&n| mixed_pick_get(n)).count();
            assert_eq!(gets, 300, "start={start}");
        }
    }

    #[test]
    fn stamp_prefix_distinguishes_payloads() {
        let mut a = vec![0xa5u8; 64];
        let mut b = vec![0xa5u8; 64];
        stamp_prefix(&mut a, 1, 99);
        stamp_prefix(&mut b, 2, 99);
        assert_ne!(a, b, "different index, same nonce");
        let mut c = vec![0xa5u8; 64];
        stamp_prefix(&mut c, 1, 100);
        assert_ne!(a, c, "same index, different nonce");
        // Tail beyond the 16-byte stamp is untouched.
        assert!(a[16..].iter().all(|&x| x == 0xa5));
    }

    #[test]
    fn stamp_prefix_truncates_on_short_buffers() {
        let mut short = vec![0u8; 4];
        stamp_prefix(&mut short, 0x0403_0201, u64::MAX);
        assert_eq!(short, [1, 2, 3, 4]);
        let mut empty: Vec<u8> = Vec::new();
        stamp_prefix(&mut empty, 7, 7);
        assert!(empty.is_empty());
    }
}
