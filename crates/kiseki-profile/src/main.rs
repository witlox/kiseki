//! Standalone profiling driver for Kiseki data paths.
//!
//! Spawns a real `kiseki-server` (single-node), then drives a
//! configurable concurrent workload against one of S3, NFSv3,
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
    /// NFSv3 (RFC 1813) over TCP — client-side library.
    Nfs3,
    /// NFSv4.1 (RFC 8881) — single-COMPOUND OPEN+WRITE+COMMIT for
    /// create, COMPOUND OPEN+READ for read.
    Nfs4,
    /// pNFS Flexible Files (RFC 8435) — write via NFSv4.1 to the MDS,
    /// read via the per-stripe DS endpoint advertised by LAYOUTGET.
    Pnfs,
    /// FUSE → GatewayOps → S3 wire. Drives `KisekiFuse` against a
    /// `RemoteHttpGateway` connected to the running server.
    Fuse,
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

    rt.block_on(async move {
        if let Err(e) = run(args).await {
            eprintln!("profile run failed: {e}");
            std::process::exit(1);
        }
    });
}

async fn run(args: RunArgs) -> Result<(), String> {
    let server = harness::ProfileServer::start(args.server_bin.as_deref()).await?;
    eprintln!(
        "[harness] server up; s3={} nfs={} ds={} metrics={}",
        server.s3_base, server.nfs_addr, server.ds_addr, server.metrics_url(),
    );

    // Size the NFS connection pool to match concurrency: each
    // worker gets its own session, no FIFO queueing on a shared
    // connection. Capped at 32 to avoid runaway server-side
    // session memory if someone runs at extreme concurrency.
    let pool_size = args.concurrency.clamp(1, 32);
    let driver: Arc<dyn protocols::Driver> =
        protocols::build(args.protocol, &server, pool_size).await?;

    let warmup_keys = if !matches!(args.shape, Shape::PutHeavy) {
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
    } else {
        Arc::new(Vec::new())
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
            worker(worker_id, driver, payload, warmup_keys, shape, stats, deadline).await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    let elapsed = (deadline - Instant::now())
        .checked_sub(Duration::from_secs(0))
        .map(|_| Duration::from_secs(args.duration_secs))
        .unwrap_or(Duration::from_secs(args.duration_secs));
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
            let key = &warmup_keys[(n as usize) % warmup_keys.len()];
            driver.get(key).await.map(|_| ())
        } else {
            driver.put(&payload).await.map(|_| ())
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
