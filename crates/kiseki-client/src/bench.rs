//! Bench driver for kiseki-client.
//!
//! Drives the data plane (PUT/GET) against an **externally-running**
//! cluster — distinct from `kiseki-profile` which spawns its own
//! single-node server. The two share the same workload shape so
//! numbers are directly comparable.
//!
//! Protocol selection is by endpoint URL scheme:
//! - `kiseki://host:port` — native TCP-framed (ADR-042 §2.2) when the
//!   `--binding tcp` flag (default) is used.
//! - `kiseki://host:port` + `--binding grpc` — native gRPC (ADR-042 §2.1).
//! - `http://host:port` / `https://host:port` — S3 HTTP listener.
//!
//! The S3 path requires the `remote-http` feature; native paths
//! require `native`. Both off-by-default, matching the rest of the
//! crate. The `bench` subcommand of the CLI gates at runtime and
//! errors clearly if an unbuilt-in protocol is selected.

#![cfg(any(feature = "native", feature = "remote-http"))]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

/// Workload shape — what mix of operations to drive. Mirrors
/// `kiseki-profile::Shape` so the output tables align.
#[derive(Copy, Clone, Debug)]
pub enum Shape {
    /// 100 % PUT.
    PutHeavy,
    /// 100 % GET against objects pre-populated during warmup.
    GetHeavy,
    /// 70 % PUT, 30 % GET (the same mix kiseki-profile uses).
    Mixed,
}

/// ADR-042 binding for native. Only meaningful when the endpoint is
/// `kiseki://`.
#[derive(Copy, Clone, Debug)]
pub enum NativeBinding {
    /// TCP-framed-postcard (ADR-042 §2.2). Default — matches the
    /// fastest path measured in the local matrix (55 k PUT, 147 k GET
    /// single-host).
    Tcp,
    /// gRPC over h2 + rustls (ADR-042 §2.1).
    Grpc,
}

/// Bench configuration. Names + defaults mirror `kiseki-profile run`.
#[derive(Clone, Debug)]
pub struct BenchConfig {
    /// Endpoint URL: `kiseki://host:port` for native, `http(s)://host:port` for S3.
    pub endpoint: String,
    /// ADR-042 binding selector (only consulted for `kiseki://`).
    pub binding: NativeBinding,
    /// Workload shape (PUT/GET mix).
    pub shape: Shape,
    /// In-flight ops.
    pub concurrency: usize,
    /// Number of TCP connections in the client pool. Workers round-robin
    /// across the pool. Defaults to 1 — the realistic shape for a single
    /// FUSE / NFS-as-client / S3-keepalive process running concurrency
    /// in-flight requests over one connection. Raise it to model a
    /// load-generator that fans concurrent ops across multiple sockets
    /// (older bench shape: `connections = concurrency`).
    pub connections: usize,
    /// Per-object payload size in bytes.
    pub object_size: usize,
    /// Wall-clock cap.
    pub duration: Duration,
    /// For GetHeavy/Mixed: how many objects to PUT during warmup.
    pub warmup_objects: usize,
    /// Discarded warm-up pass: run the same closed-loop workload for
    /// this many seconds before the measured clock starts, throwing
    /// away every sample and counter. Keeps the known cold-ramp ~2×
    /// artifact out of the measured window. 0 disables.
    pub warmup_secs: u64,
    /// Error-rate gate threshold: `failed / total` above this means
    /// the run is functionally broken. The bin enforces it (exit 2)
    /// after the report prints — see `error_rate_exceeded`.
    pub max_error_rate: f64,
    /// Client identity recorded in the report so multi-client sweep
    /// artifacts are attributable. `None` → kernel hostname.
    pub client_id: Option<String>,
    /// Emit machine-readable JSON instead of the human table.
    pub json: bool,
    /// Override the `tenant_id` used by the bench. `None` → the
    /// deterministic perf-tenant from `bench_default_ids` so bench
    /// writes never land on the system bootstrap tenant.
    pub tenant_id: Option<OrgId>,
    /// Override the `namespace_id` used by the bench. `None` → the
    /// deterministic perf-namespace from `bench_default_ids`.
    /// Per ADR-033 §1, multi-shard fanout is a property of the
    /// namespace's `NamespaceShardMap`: operators bring it up via
    /// `POST /admin/topology/namespaces` (see
    /// `infra/gcp/benchmarks/setup-shards.sh`) BEFORE the bench run;
    /// first-touch from this client would create a single-shard
    /// namespace which defeats the perf measurement.
    pub namespace_id: Option<NamespaceId>,
    /// When `Some(url)`, the bench POSTs `/admin/topology/namespaces`
    /// before driving load — provisioning the bench namespace +
    /// requested shard count + waiting for the gateway's per-namespace
    /// registry to converge. Lets local docker-compose runs do native
    /// benches without running `setup-shards.sh` by hand. `None`
    /// preserves the GCP runbook contract: operator provisions
    /// externally; bench just measures.
    pub admin_endpoint: Option<String>,
    /// Shard count for the provision step. Ignored when
    /// `admin_endpoint` is `None`. Defaults to 3 (one shard per node
    /// in the typical 3-node compose).
    pub bench_shards: u64,
}

/// Deterministic bench tenant + namespace. Distinct from the system
/// bootstrap tenant (`OrgId(Uuid::from_u128(1))`) and the system
/// "default" namespace (`UUIDv5(NAMESPACE_DNS, "default")`) so the
/// bench's high-concurrency workload doesn't compete with casual S3
/// traffic and isn't auto-created at boot with the wrong shard count.
#[must_use]
pub fn bench_default_ids() -> (OrgId, NamespaceId) {
    (
        OrgId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            b"kiseki-bench-tenant",
        )),
        NamespaceId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            b"kiseki-bench",
        )),
    )
}

/// Final benchmark report.
#[derive(Debug, serde::Serialize)]
pub struct BenchReport {
    /// Driver label (`native-tcp`, `native-grpc`, `s3`).
    pub protocol: String,
    /// Shape label (`PutHeavy`, `GetHeavy`, `Mixed`).
    pub shape: String,
    /// Effective in-flight worker count.
    pub concurrency: usize,
    /// TCP connections in the client pool (the second sweep axis —
    /// without it, conn×conc sweep cells are indistinguishable).
    pub connections: usize,
    /// Per-object payload size in bytes.
    pub object_size: usize,
    /// Wall-clock elapsed for the measurement phase.
    pub duration_secs: f64,
    /// Successful op count.
    pub ops: u64,
    /// Failed op count.
    pub errors: u64,
    /// GETs whose returned byte length != `object_size`. Also counted
    /// in `errors`; broken out so truncation (#127-class) is
    /// distinguishable from transport failures.
    pub get_length_mismatches: u64,
    /// Successful ops per second.
    pub ops_per_sec: f64,
    /// Throughput in MiB/sec (= `ops` × `object_size` / MiB / seconds).
    pub mib_per_sec: f64,
    /// p50 latency in microseconds (sampled — 1 in 16 ops).
    pub p50_us: u32,
    /// p95 latency in microseconds.
    pub p95_us: u32,
    /// p99 latency in microseconds.
    pub p99_us: u32,
    /// Per-process payload-seed nonce (hex). Provenance: lets a
    /// post-run dedup audit tie chunks back to this run.
    pub run_nonce: String,
    /// Client host identity (`--client-id`, default kernel hostname).
    pub client_id: String,
    /// Length of the discarded warm-up pass.
    pub warmup_secs: u64,
    /// Error-rate gate threshold the bin enforces after printing.
    pub max_error_rate: f64,
}

/// Error-rate gate (C-2): `true` when `errors / (ops + errors)`
/// exceeds the allowed rate. A run that completed zero ops total is
/// always a functional break — it must not pass any gate.
#[must_use]
#[allow(clippy::cast_precision_loss)] // bench totals stay far below 2^52
pub fn error_rate_exceeded(ops: u64, errors: u64, max_error_rate: f64) -> bool {
    let total = ops + errors;
    if total == 0 {
        return true;
    }
    (errors as f64) / (total as f64) > max_error_rate
}

#[derive(Clone, Debug)]
struct Key {
    /// Used by native drivers to key GET requests.
    /// `allow(dead_code)` because the s3-only build doesn't read it.
    #[allow(dead_code)]
    composition_id: CompositionId,
    /// Reserved for protocols that support name-keyed reads (`S3`).
    /// `allow(dead_code)` because native drivers key by `composition_id`;
    /// only s3 reads this field, and the s3 module is feature-gated.
    #[allow(dead_code)]
    name: Option<String>,
}

#[async_trait::async_trait]
trait Driver: Send + Sync {
    async fn put(&self, payload: &[u8]) -> Result<Key, String>;
    async fn get(&self, key: &Key) -> Result<usize, String>;
    fn label(&self) -> &'static str;
}

/// Resolve the (`tenant_id`, `namespace_id`) the bench should use.
/// CLI / config overrides win; otherwise falls back to
/// `bench_default_ids()` (the deterministic perf-tenant + perf-ns
/// separate from the system bootstrap tenant + "default" namespace).
fn resolve_bench_ids(cfg: &BenchConfig) -> (OrgId, NamespaceId) {
    let (tenant_default, namespace_default) = bench_default_ids();
    (
        cfg.tenant_id.unwrap_or(tenant_default),
        cfg.namespace_id.unwrap_or(namespace_default),
    )
}

/// Build the right driver from the endpoint URL and binding selector.
async fn build_driver(cfg: &BenchConfig) -> Result<Arc<dyn Driver>, String> {
    if let Some(addr) = cfg.endpoint.strip_prefix("kiseki://") {
        #[cfg(feature = "native")]
        {
            let (tenant_id, namespace_id) = resolve_bench_ids(cfg);
            match cfg.binding {
                NativeBinding::Tcp => {
                    return native::build_tcp(addr, cfg.connections, tenant_id, namespace_id).await
                }
                NativeBinding::Grpc => {
                    return native::build_grpc(addr, cfg.connections, tenant_id, namespace_id).await
                }
            }
        }
        #[cfg(not(feature = "native"))]
        {
            let _ = addr;
            return Err(
                "kiseki:// endpoint requires the `native` feature; rebuild kiseki-client with `--features native`"
                    .to_string(),
            );
        }
    }
    if cfg.endpoint.starts_with("http://") || cfg.endpoint.starts_with("https://") {
        #[cfg(feature = "remote-http")]
        return Ok(Arc::new(s3::S3Driver::new(&cfg.endpoint).await?));
        #[cfg(not(feature = "remote-http"))]
        return Err(
            "http(s):// endpoint requires the `remote-http` feature; rebuild kiseki-client with `--features remote-http`"
                .to_string(),
        );
    }
    Err(format!(
        "unsupported endpoint scheme: {} — use kiseki://host:port or http(s)://host:port",
        cfg.endpoint
    ))
}

/// Seed-space phase tags. Distinct constants per phase make the three
/// seed spaces provably disjoint for any fixed nonce — warmup-object,
/// warmup-loop, and measured payloads can never collide with each
/// other (a collision would dedup away exactly the commit under test).
const PHASE_WARMUP_OBJECTS: u64 = 1;
const PHASE_WARMUP_LOOP: u64 = 2;
const PHASE_MEASURED: u64 = 3;

/// One splitmix64 finalization round (the Stafford mix13 variant).
const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Payload-seed derivation: hash(`run_nonce`, phase, worker,
/// `op_index`) via chained splitmix64 rounds (C-1 fix). The per-process nonce
/// keeps re-runs against a non-wiped cluster and concurrent
/// multi-client sweeps from emitting identical payload streams —
/// which content-addressed dedup would otherwise collapse, skipping
/// the very fsync path the A/B varies.
fn mix_seed(nonce: u128, phase: u64, worker_id: u64, op_index: u64) -> u64 {
    #[allow(clippy::cast_possible_truncation)] // intentional: fold u128 nonce into two u64 absorbs
    let (hi, lo) = ((nonce >> 64) as u64, nonce as u64);
    let mut h = mix64(hi ^ 0x9E37_79B9_7F4A_7C15);
    h = mix64(h ^ lo);
    h = mix64(h ^ phase);
    h = mix64(h ^ worker_id);
    h = mix64(h ^ op_index);
    h
}

/// Default client identity: the kernel hostname. Falls back to
/// "unknown" off-Linux or when /proc is unreadable.
fn default_client_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Generate DISTINCT, high-entropy payload bytes seeded by `seed`.
///
/// Kiseki dedups by content address, so a bench that writes identical
/// bytes for every object collapses the whole run onto ONE chunk —
/// making throughput meaningless and (pre-ADR-044) tripping the #102
/// torn-chunk read path. Seeding per object keeps every chunk distinct.
/// Cheap xorshift fill — not cryptographic, just non-dedupable.
fn make_payload(size: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut buf = vec![0u8; size];
    for chunk in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = bytes[i];
        }
    }
    buf
}

/// Best-effort provision the bench namespace via the admin HTTP API.
/// `POST /admin/topology/namespaces` is idempotent: 201 on first
/// create, 409 "already exists" on re-runs — both are success. Any
/// other failure mode (admin endpoint unreachable, single-node
/// "cluster control not wired", network error) returns `Err` so the
/// caller can surface it before driving load.
fn provision_bench_namespace(
    admin_endpoint: &str,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
    shards: u64,
) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let host_port = admin_endpoint
        .strip_prefix("http://")
        .or_else(|| admin_endpoint.strip_prefix("https://"))
        .and_then(|rest| rest.split('/').next())
        .ok_or("invalid admin endpoint URL (expected http[s]://host:port)")?;
    let body = format!(
        "{{\"namespace_id\":\"{}\",\"tenant_id\":\"{}\",\"shards\":{}}}",
        namespace_id.0, tenant_id.0, shards
    );
    let req = format!(
        "POST /admin/topology/namespaces HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
    );
    let mut stream = TcpStream::connect(host_port)
        .map_err(|e| format!("admin connect failed ({host_port}): {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("admin write failed: {e}"))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("admin read failed: {e}"))?;
    let head = String::from_utf8_lossy(&buf);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| {
            format!(
                "admin reply has no HTTP status: {}",
                &head[..head.len().min(120)]
            )
        })?;
    match status {
        // 200/201 = created, 409 = already exists — both are success.
        200 | 201 | 409 => Ok(()),
        _ => Err(format!(
            "POST /admin/topology/namespaces returned {status}: {}",
            head.lines()
                .last()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>()
        )),
    }
}

/// Counters + samples from one closed-loop workload pass.
struct PassResult {
    ops: u64,
    errors: u64,
    get_length_mismatches: u64,
    /// Sorted latency samples (1 in 16 ops), microseconds.
    samples: Vec<u32>,
    elapsed: Duration,
}

/// One closed-loop workload pass: `cfg.concurrency` workers, each
/// keeping exactly one op in flight until `duration` elapses. The
/// warm-up pass and the measured pass run this same loop — only the
/// seed-space `phase` tag and what happens to the result differ.
async fn run_pass(
    driver: &Arc<dyn Driver>,
    cfg: &BenchConfig,
    warmup_keys: &[Key],
    nonce: u128,
    phase: u64,
    duration: Duration,
) -> PassResult {
    let ops = Arc::new(AtomicU64::new(0));
    let errs = Arc::new(AtomicU64::new(0));
    let mismatches = Arc::new(AtomicU64::new(0));
    // Bounded latency sample buffer per worker — we sample every Nth
    // op to keep memory flat for long runs.
    let latency_samples = Arc::new(parking_lot::Mutex::new(Vec::<u32>::new()));

    let started = Instant::now();
    let deadline = started + duration;
    let mut handles = Vec::with_capacity(cfg.concurrency);

    for worker_id in 0..cfg.concurrency {
        let driver = Arc::clone(driver);
        let ops = Arc::clone(&ops);
        let errs = Arc::clone(&errs);
        let mismatches = Arc::clone(&mismatches);
        let latency_samples = Arc::clone(&latency_samples);
        let object_size = cfg.object_size;
        let warmup_keys = warmup_keys.to_vec();
        let shape = cfg.shape;
        handles.push(tokio::spawn(async move {
            // Each worker keeps its own local latency vec, flushed at
            // exit. Avoids hot-lock on the shared Mutex.
            let mut local = Vec::with_capacity(256);
            let mut n = 0u64;
            while Instant::now() < deadline {
                // Mix selection: cast n to usize via narrowing is fine
                // here since n grows monotonically with wall clock; on
                // 32-bit targets we'd need many billions of ops to
                // wrap, and the deadline triggers first.
                #[allow(clippy::cast_possible_truncation)]
                let n_usize = n as usize;
                let is_put = match shape {
                    Shape::PutHeavy => true,
                    Shape::GetHeavy => false,
                    Shape::Mixed => (worker_id + n_usize) % 10 < 7,
                };
                let t0 = Instant::now();
                let res = if is_put {
                    // Distinct content per (nonce, phase, worker, op)
                    // so dedup can't collapse the write workload —
                    // across workers, passes, clients, OR re-runs.
                    let seed = mix_seed(nonce, phase, worker_id as u64, n);
                    driver
                        .put(&make_payload(object_size, seed))
                        .await
                        .map(|_| ())
                } else {
                    let idx = (worker_id + n_usize) % warmup_keys.len();
                    // A GET must return exactly object_size bytes —
                    // 0-byte / truncated responses are failures, not
                    // throughput (#127-class regressions).
                    match driver.get(&warmup_keys[idx]).await {
                        Ok(len) if len == object_size => Ok(()),
                        Ok(len) => {
                            mismatches.fetch_add(1, Ordering::Relaxed);
                            Err(format!("get returned {len} bytes, want {object_size}"))
                        }
                        Err(e) => Err(e),
                    }
                };
                let elapsed_us = u32::try_from(t0.elapsed().as_micros()).unwrap_or(u32::MAX);
                match res {
                    Ok(()) => {
                        ops.fetch_add(1, Ordering::Relaxed);
                        // Sample 1 in 16 to keep memory bounded.
                        if n % 16 == 0 {
                            local.push(elapsed_us);
                        }
                    }
                    Err(_) => {
                        errs.fetch_add(1, Ordering::Relaxed);
                    }
                }
                n += 1;
            }
            latency_samples.lock().extend_from_slice(&local);
        }));
    }

    for h in handles {
        let _ = h.await;
    }
    let elapsed = started.elapsed();

    let mut samples = latency_samples.lock().clone();
    samples.sort_unstable();
    PassResult {
        ops: ops.load(Ordering::Relaxed),
        errors: errs.load(Ordering::Relaxed),
        get_length_mismatches: mismatches.load(Ordering::Relaxed),
        samples,
        elapsed,
    }
}

/// Run the benchmark and emit a report on stdout. Returns the report
/// so callers can inspect it programmatically.
///
/// # Errors
/// Returns the underlying driver error if the endpoint scheme is
/// unsupported, the connection fails, or the warmup PUTs fail.
#[allow(clippy::too_many_lines)] // provision + warmup + measured pass + report wiring is intrinsically long
pub async fn run(cfg: BenchConfig) -> Result<BenchReport, String> {
    // Per-process nonce: every payload seed this run derives mixes it
    // in, so no two runs (or concurrent clients) share a seed space.
    let run_nonce = uuid::Uuid::new_v4();
    let (tenant_id, namespace_id) = resolve_bench_ids(&cfg);
    // Best-effort: provision the bench namespace before driving load.
    // Only when --admin-endpoint is set; GCP runbook expects external
    // provisioning via setup-shards.sh and leaves this unset.
    if let Some(admin) = cfg.admin_endpoint.as_deref() {
        provision_bench_namespace(admin, tenant_id, namespace_id, cfg.bench_shards)?;
        // The control-plane apply hook hydrates each gateway's
        // per-namespace registry asynchronously after the admin POST
        // returns. Probe-and-wait so the first warmup PUT lands on a
        // gateway that knows the namespace.
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let driver = build_driver(&cfg).await?;

    // Warmup keys for GetHeavy / Mixed. Each object gets DISTINCT,
    // high-entropy content (`make_payload`) so content-addressed dedup
    // does not collapse the whole run onto one chunk — which would make
    // throughput meaningless (GH #102 / bench-realism). The
    // PHASE_WARMUP_OBJECTS tag keeps these seeds disjoint from both
    // loop phases.
    let warmup_keys: Vec<Key> = if matches!(cfg.shape, Shape::GetHeavy | Shape::Mixed) {
        let n = cfg.warmup_objects.max(1);
        let mut keys = Vec::with_capacity(n);
        for i in 0..n {
            let seed = mix_seed(run_nonce.as_u128(), PHASE_WARMUP_OBJECTS, 0, i as u64);
            keys.push(driver.put(&make_payload(cfg.object_size, seed)).await?);
        }
        keys
    } else {
        Vec::new()
    };

    // Discarded warm-up pass: same closed-loop shape, separate seed
    // space, nothing lands in the report. Keeps the cold-ramp artifact
    // out of the measured window for every shape (put-heavy included).
    if cfg.warmup_secs > 0 {
        let _ = run_pass(
            &driver,
            &cfg,
            &warmup_keys,
            run_nonce.as_u128(),
            PHASE_WARMUP_LOOP,
            Duration::from_secs(cfg.warmup_secs),
        )
        .await;
    }

    // Measured pass — the only one the report sees.
    let pass = run_pass(
        &driver,
        &cfg,
        &warmup_keys,
        run_nonce.as_u128(),
        PHASE_MEASURED,
        cfg.duration,
    )
    .await;

    let samples = &pass.samples;
    // Integer percentile: idx = floor(len * q). Avoids f64 casts at
    // the expense of slight non-interpolation — bench precision is
    // ±1 sample anyway.
    let p = |q_numer: usize, q_denom: usize| -> u32 {
        if samples.is_empty() {
            0
        } else {
            let idx = (samples.len() * q_numer / q_denom).min(samples.len() - 1);
            samples[idx]
        }
    };

    // u64/usize → f64 casts: bench totals never exceed 2^52 within a
    // sane run (that's 4.5 PiB of data or 4.5 quadrillion ops), so
    // mantissa truncation cannot trigger here.
    #[allow(clippy::cast_precision_loss)]
    let ops_per_sec = (pass.ops as f64) / pass.elapsed.as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let mib_per_sec =
        (pass.ops as f64 * cfg.object_size as f64) / (1024.0 * 1024.0 * pass.elapsed.as_secs_f64());

    let report = BenchReport {
        protocol: driver.label().into(),
        shape: format!("{:?}", cfg.shape),
        concurrency: cfg.concurrency,
        connections: cfg.connections,
        object_size: cfg.object_size,
        duration_secs: pass.elapsed.as_secs_f64(),
        ops: pass.ops,
        errors: pass.errors,
        get_length_mismatches: pass.get_length_mismatches,
        ops_per_sec,
        mib_per_sec,
        p50_us: p(50, 100),
        p95_us: p(95, 100),
        p99_us: p(99, 100),
        run_nonce: run_nonce.simple().to_string(),
        client_id: cfg.client_id.clone().unwrap_or_else(default_client_id),
        warmup_secs: cfg.warmup_secs,
        max_error_rate: cfg.max_error_rate,
    };

    if cfg.json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|e| format!("serialize: {e}"))?
        );
    } else {
        println!(
            "protocol={} shape={} concurrency={} connections={} object_size={}",
            report.protocol,
            report.shape,
            report.concurrency,
            report.connections,
            report.object_size
        );
        println!(
            "ops={} throughput={:.1} op/s {:.2} MiB/s",
            report.ops, report.ops_per_sec, report.mib_per_sec
        );
        println!(
            "latency_us p50={} p95={} p99={}",
            report.p50_us, report.p95_us, report.p99_us
        );
        println!(
            "client_id={} run_nonce={} warmup_secs={} max_error_rate={}",
            report.client_id, report.run_nonce, report.warmup_secs, report.max_error_rate
        );
        if report.errors > 0 {
            println!(
                "errors={} get_length_mismatches={}",
                report.errors, report.get_length_mismatches
            );
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Native (TCP-framed + gRPC) drivers
// ---------------------------------------------------------------------------

#[cfg(feature = "native")]
mod native {
    use super::{Driver, Key};
    use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn ctrl(tenant_id: OrgId) -> kiseki_proto::v1::native::ControlFields {
        kiseki_proto::v1::native::ControlFields {
            tenant_id: Some(kiseki_proto::v1::OrgId {
                value: tenant_id.0.to_string(),
            }),
            idempotency_key: uuid::Uuid::new_v4().as_bytes().to_vec(),
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
            forwarded_from_node: None,
        }
    }

    // -- TCP-framed -----------------------------------------------------------

    pub(super) async fn build_tcp(
        addr: &str,
        pool_size: usize,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Arc<dyn Driver>, String> {
        let pool_size = pool_size.max(1);
        let mut clients = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let client =
                crate::native::tcp_framed::client::TcpFramedClient::connect_plaintext(addr)
                    .await
                    .map_err(|e| format!("tcp-framed connect: {e}"))?;
            clients.push(client);
        }
        Ok(Arc::new(TcpFramedDriver {
            clients,
            next: AtomicUsize::new(0),
            tenant_id,
            namespace_id,
        }))
    }

    pub(super) struct TcpFramedDriver {
        clients: Vec<Arc<crate::native::tcp_framed::client::TcpFramedClient>>,
        next: AtomicUsize,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    }

    impl TcpFramedDriver {
        fn pick(&self) -> Arc<crate::native::tcp_framed::client::TcpFramedClient> {
            let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len();
            Arc::clone(&self.clients[idx])
        }
    }

    #[async_trait::async_trait]
    impl Driver for TcpFramedDriver {
        async fn put(&self, payload: &[u8]) -> Result<Key, String> {
            // V3: meta = postcard(PutObjectRequest with empty .data),
            // bulk = the actual payload bytes. The server attaches bulk
            // onto req.data before calling the handler.
            let req = kiseki_proto::v1::native::PutObjectRequest {
                control: Some(ctrl(self.tenant_id)),
                namespace_id: Some(kiseki_proto::v1::NamespaceId {
                    value: self.namespace_id.0.to_string(),
                }),
                name: format!("bench-{}", uuid::Uuid::new_v4().simple()),
                data: Vec::new(),
            };
            let req_meta =
                postcard::to_allocvec(&req).map_err(|e| format!("tcp-framed put encode: {e}"))?;
            let (resp_meta, _) = self
                .pick()
                .call_ok("put_object", req_meta, payload.to_vec())
                .await
                .map_err(|e| format!("tcp-framed put: {e}"))?;
            let resp: kiseki_proto::v1::native::PutObjectResponse =
                postcard::from_bytes(&resp_meta)
                    .map_err(|e| format!("tcp-framed put decode: {e}"))?;
            let comp = resp
                .composition_id
                .ok_or_else(|| "tcp-framed put: missing composition_id".to_string())?;
            let uuid = uuid::Uuid::parse_str(&comp.value)
                .map_err(|e| format!("tcp-framed put: uuid: {e}"))?;
            Ok(Key {
                composition_id: CompositionId(uuid),
                name: None,
            })
        }

        async fn get(&self, key: &Key) -> Result<usize, String> {
            let req = kiseki_proto::v1::native::GetObjectRequest {
                control: Some(ctrl(self.tenant_id)),
                namespace_id: Some(kiseki_proto::v1::NamespaceId {
                    value: self.namespace_id.0.to_string(),
                }),
                range_start: 0,
                range_end: 0,
                key: Some(
                    kiseki_proto::v1::native::get_object_request::Key::CompositionId(
                        kiseki_proto::v1::CompositionId {
                            value: key.composition_id.0.to_string(),
                        },
                    ),
                ),
            };
            let req_meta =
                postcard::to_allocvec(&req).map_err(|e| format!("tcp-framed get encode: {e}"))?;
            let (_, resp_bulk) = self
                .pick()
                .call_ok("get_object", req_meta, Vec::new())
                .await
                .map_err(|e| format!("tcp-framed get: {e}"))?;
            Ok(resp_bulk.len())
        }

        fn label(&self) -> &'static str {
            "native-tcp"
        }
    }

    // -- gRPC ----------------------------------------------------------------

    pub(super) async fn build_grpc(
        addr: &str,
        pool_size: usize,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Arc<dyn Driver>, String> {
        let pool_size = pool_size.max(1);
        let mut clients = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
                .map_err(|e| format!("grpc endpoint: {e}"))?
                .tcp_nodelay(true)
                .initial_stream_window_size(Some(16 * 1024 * 1024))
                .initial_connection_window_size(Some(32 * 1024 * 1024))
                .timeout(std::time::Duration::from_secs(30));
            let channel = endpoint
                .connect()
                .await
                .map_err(|e| format!("grpc connect: {e}"))?;
            let client =
                kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient::new(channel)
                    .max_decoding_message_size(64 * 1024 * 1024)
                    .max_encoding_message_size(64 * 1024 * 1024);
            clients.push(client);
        }
        Ok(Arc::new(GrpcDriver {
            clients,
            next: AtomicUsize::new(0),
            tenant_id,
            namespace_id,
        }))
    }

    pub(super) struct GrpcDriver {
        clients: Vec<
            kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient<
                tonic::transport::Channel,
            >,
        >,
        next: AtomicUsize,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    }

    impl GrpcDriver {
        fn pick(
            &self,
        ) -> kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient<
            tonic::transport::Channel,
        > {
            let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len();
            self.clients[idx].clone()
        }
    }

    #[async_trait::async_trait]
    impl Driver for GrpcDriver {
        async fn put(&self, payload: &[u8]) -> Result<Key, String> {
            let req = tonic::Request::new(kiseki_proto::v1::native::PutObjectRequest {
                control: Some(ctrl(self.tenant_id)),
                namespace_id: Some(kiseki_proto::v1::NamespaceId {
                    value: self.namespace_id.0.to_string(),
                }),
                name: format!("bench-{}", uuid::Uuid::new_v4().simple()),
                data: payload.to_vec(),
            });
            let resp = self
                .pick()
                .put_object(req)
                .await
                .map_err(|e| format!("grpc put: {e}"))?
                .into_inner();
            let comp = resp
                .composition_id
                .ok_or_else(|| "grpc put: missing composition_id".to_string())?;
            let uuid =
                uuid::Uuid::parse_str(&comp.value).map_err(|e| format!("grpc put: uuid: {e}"))?;
            Ok(Key {
                composition_id: CompositionId(uuid),
                name: None,
            })
        }

        async fn get(&self, key: &Key) -> Result<usize, String> {
            let req = tonic::Request::new(kiseki_proto::v1::native::GetObjectRequest {
                control: Some(ctrl(self.tenant_id)),
                namespace_id: Some(kiseki_proto::v1::NamespaceId {
                    value: self.namespace_id.0.to_string(),
                }),
                range_start: 0,
                range_end: 0,
                key: Some(
                    kiseki_proto::v1::native::get_object_request::Key::CompositionId(
                        kiseki_proto::v1::CompositionId {
                            value: key.composition_id.0.to_string(),
                        },
                    ),
                ),
            });
            let resp = self
                .pick()
                .get_object(req)
                .await
                .map_err(|e| format!("grpc get: {e}"))?
                .into_inner();
            Ok(resp.data.len())
        }

        fn label(&self) -> &'static str {
            "native-grpc"
        }
    }
}

// ---------------------------------------------------------------------------
// S3 driver
// ---------------------------------------------------------------------------

#[cfg(feature = "remote-http")]
mod s3 {
    use super::{Driver, Key};
    use kiseki_common::ids::CompositionId;

    /// S3 HTTP driver. Each PUT goes to /bench/obj-<uuid>; reads
    /// resolve by name (kiseki S3 maps GET /<bucket>/<key> directly to
    /// the composition's chunk stream).
    pub(super) struct S3Driver {
        base: String,
        client: reqwest::Client,
        bucket: String,
    }

    impl S3Driver {
        pub async fn new(base: &str) -> Result<Self, String> {
            let client = reqwest::Client::builder()
                .pool_max_idle_per_host(64)
                .build()
                .map_err(|e| format!("reqwest client: {e}"))?;
            let base = base.trim_end_matches('/').to_string();
            // Best-effort bucket create — ignore failure (may already
            // exist from a prior run).
            let _ = client.put(format!("{base}/bench")).send().await;
            Ok(Self {
                base,
                client,
                bucket: "bench".to_string(),
            })
        }
    }

    #[async_trait::async_trait]
    impl Driver for S3Driver {
        async fn put(&self, payload: &[u8]) -> Result<Key, String> {
            let name = format!("obj-{}", uuid::Uuid::new_v4().simple());
            let url = format!("{}/{}/{}", self.base, self.bucket, name);
            let resp = self
                .client
                .put(&url)
                .body(payload.to_vec())
                .send()
                .await
                .map_err(|e| format!("s3 put send: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("s3 put: HTTP {}", resp.status()));
            }
            Ok(Key {
                // S3 doesn't return composition_id directly in the
                // PUT response body; the bench loop doesn't need it
                // for GETs (we keyed by name).
                composition_id: CompositionId(uuid::Uuid::nil()),
                name: Some(name),
            })
        }

        async fn get(&self, key: &Key) -> Result<usize, String> {
            let name = key
                .name
                .as_deref()
                .ok_or_else(|| "s3 get: key has no name".to_string())?;
            let url = format!("{}/{}/{}", self.base, self.bucket, name);
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("s3 get send: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("s3 get: HTTP {}", resp.status()));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| format!("s3 get body: {e}"))?;
            Ok(bytes.len())
        }

        fn label(&self) -> &'static str {
            "s3"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn build_driver_rejects_unsupported_scheme() {
        let cfg = BenchConfig {
            endpoint: "ftp://example.com".into(),
            binding: NativeBinding::Tcp,
            shape: Shape::PutHeavy,
            concurrency: 1,
            connections: 1,
            object_size: 1024,
            duration: Duration::from_secs(1),
            warmup_objects: 0,
            warmup_secs: 0,
            max_error_rate: 0.0,
            client_id: None,
            json: false,
            tenant_id: None,
            namespace_id: None,
            admin_endpoint: None,
            bench_shards: 1,
        };
        let Err(err) = build_driver(&cfg).await else {
            panic!("ftp:// must be rejected");
        };
        assert!(
            err.contains("unsupported endpoint scheme"),
            "error message must explain the rejected scheme; got: {err}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_driver_rejects_kiseki_when_native_feature_off() {
        // This test passes regardless of whether the `native` feature
        // is on — when it's on we get a connect error, when it's off
        // we get the feature-gate error. The key invariant is that
        // build_driver does NOT silently succeed against a stub.
        let cfg = BenchConfig {
            endpoint: "kiseki://127.0.0.1:1".into(),
            binding: NativeBinding::Tcp,
            shape: Shape::PutHeavy,
            concurrency: 1,
            connections: 1,
            object_size: 1024,
            duration: Duration::from_secs(1),
            warmup_objects: 0,
            warmup_secs: 0,
            max_error_rate: 0.0,
            client_id: None,
            json: false,
            tenant_id: None,
            namespace_id: None,
            admin_endpoint: None,
            bench_shards: 1,
        };
        let Err(err) = build_driver(&cfg).await else {
            panic!("port 1 is reserved — no listener can answer");
        };
        // Either "connect failed" (native built) or "requires the `native` feature".
        assert!(
            !err.is_empty(),
            "build_driver must produce a non-empty error message"
        );
    }

    // --- C-1: payload-seed derivation ---

    #[test]
    fn seed_spaces_disjoint_across_phases_workers_ops() {
        let nonce = uuid::Uuid::new_v4().as_u128();
        let mut seen = std::collections::HashSet::new();
        for phase in [PHASE_WARMUP_OBJECTS, PHASE_WARMUP_LOOP, PHASE_MEASURED] {
            for worker in 0..16u64 {
                for op in 0..256u64 {
                    assert!(
                        seen.insert(mix_seed(nonce, phase, worker, op)),
                        "seed collision at phase={phase} worker={worker} op={op}"
                    );
                }
            }
        }
    }

    #[test]
    fn seed_is_nonce_sensitive() {
        // Two runs (or two clients) must never share a payload stream —
        // identical (phase, worker, op) under different nonces must
        // produce different seeds AND different payload bytes.
        let (n1, n2) = (
            uuid::Uuid::new_v4().as_u128(),
            uuid::Uuid::new_v4().as_u128(),
        );
        for op in 0..64u64 {
            let (s1, s2) = (
                mix_seed(n1, PHASE_MEASURED, 0, op),
                mix_seed(n2, PHASE_MEASURED, 0, op),
            );
            assert_ne!(s1, s2, "nonce did not perturb seed at op={op}");
            assert_ne!(
                make_payload(4096, s1),
                make_payload(4096, s2),
                "payloads collided at op={op} — dedup trap"
            );
        }
    }

    // --- C-2: error-rate gate ---

    #[test]
    fn error_rate_gate_thresholds() {
        // Default 0.0: any error trips the gate.
        assert!(!error_rate_exceeded(100, 0, 0.0));
        assert!(error_rate_exceeded(99, 1, 0.0));
        // Strictly-greater-than semantics at a nonzero threshold.
        assert!(!error_rate_exceeded(90, 10, 0.1)); // 10/100 == 0.1, not >
        assert!(error_rate_exceeded(89, 11, 0.1)); // 11/100 > 0.1
    }

    #[test]
    fn error_rate_gate_rejects_zero_op_runs() {
        // A run that completed nothing is a functional break, not a
        // clean pass — it must trip the gate at any threshold.
        assert!(error_rate_exceeded(0, 0, 0.0));
        assert!(error_rate_exceeded(0, 0, 1.0));
    }
}
