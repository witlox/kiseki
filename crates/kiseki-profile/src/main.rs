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
    c.provision_bench_topology(args.admin_bin.as_deref(), shard_count)
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
        let payload: Arc<[u8]> = vec![0xa5u8; args.object_size].into();
        let mut keys = Vec::with_capacity(args.warmup_objects);
        for _ in 0..args.warmup_objects {
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
    let deadline = Instant::now() + Duration::from_secs(args.duration_secs);

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

    let elapsed = (deadline - Instant::now())
        .checked_sub(Duration::from_secs(0))
        .map_or(Duration::from_secs(args.duration_secs), |_| {
            Duration::from_secs(args.duration_secs)
        });
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

async fn worker(
    worker_id: usize,
    driver: Arc<dyn protocols::Driver>,
    payload: Arc<[u8]>,
    warmup_keys: Arc<Vec<protocols::Key>>,
    shape: Shape,
    stats: Arc<stats::Stats>,
    deadline: Instant,
) {
    use std::cell::Cell;
    let counter: Cell<u64> = Cell::new(worker_id as u64);
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
            Shape::Mixed => {
                // Cheap rotating selector: 0..6 → put, 7..9 → get.
                let n = counter.get();
                counter.set(n.wrapping_add(1));
                (n % 10) >= 7
            }
        };
        let start = Instant::now();
        let result = if pick_get {
            if warmup_keys.is_empty() {
                stats.record_error();
                continue;
            }
            let n = counter.get();
            counter.set(n.wrapping_add(1));
            let key = &warmup_keys[usize::try_from(n).unwrap_or(0) % warmup_keys.len()];
            driver.get(key).await.map(|_| ())
        } else {
            // Stamp a unique (worker, op, salt) prefix so each PUT's
            // chunk_id is unique. 16 bytes is sufficient for SHA-256
            // to produce a different output even on a 64 KiB buffer
            // whose remaining bytes are identical.
            if put_buf.len() >= 16 {
                put_buf[0..8]
                    .copy_from_slice(&((worker_id as u64).wrapping_shl(40) ^ put_n).to_le_bytes());
                put_buf[8..16].copy_from_slice(&salt.to_le_bytes());
            }
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
