#![allow(clippy::cast_precision_loss)] // format_bytes: display-only f64 cast is fine
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! Kiseki client CLI -- staging, cache management, FUSE mount, diagnostics.
//!
//! Usage:
//!   kiseki-client mount --endpoint kiseki://<host>:<port> --mountpoint /mnt/kiseki [--cache-mode organic] [--cache-dir /cache]
//!   kiseki-client mount --endpoint http://<host>:<port> --mountpoint /mnt/kiseki  # S3 fallback
//!   kiseki-client mount --in-memory --mountpoint /mnt/kiseki [--cache-mode organic]
//!   kiseki-client stage --dataset /training/imagenet [--timeout 300]
//!   kiseki-client stage --status
//!   kiseki-client stage --release /training/imagenet
//!   kiseki-client stage --release-all
//!   kiseki-client cache --stats
//!   kiseki-client cache --wipe
//!   kiseki-client version
//!   kiseki-client help

use std::path::PathBuf;

use kiseki_client::cache::{CacheConfig, CacheManager, CacheMode};
use kiseki_client::staging::{StagingConfig, StagingManager};

/// Evict any stale FUSE mount left over at `mountpoint` from a
/// prior kiseki-client daemon that died without unmounting.
///
/// 2026-05-09 GCP finding (bug #3): when a kiseki-client process is
/// SIGKILL'd, the kernel keeps its FUSE mountpoint registered as a
/// `kiseki on /mnt/X type fuse` entry, but the userspace daemon is
/// gone. Subsequent `stat` calls on that path block in the kernel
/// waiting for the dead daemon. The next mount attempt either fails
/// outright (path already mounted) or succeeds-but-unresponsive
/// (overlapping mounts produce a wedged dentry).
///
/// This runs `fusermount3 -uz <mountpoint>` before every `mount`
/// invocation. `-z` is "lazy unmount" — it detaches immediately
/// even if the mountpoint has pending operations, so a wedged
/// zombie clears in ~ms. If nothing is mounted, fusermount3 exits
/// non-zero with "not a mountpoint" or similar; we ignore that.
///
/// Tested by `tests/fuse_mount_cleanup.rs` against a real path
/// (no FUSE state required — the spawn semantics are what matters).
#[cfg(feature = "fuse")]
fn evict_stale_fuse_mount(mountpoint: &str) {
    let out = std::process::Command::new("fusermount3")
        .args(["-uz", mountpoint])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            tracing::info!(mountpoint, "evicted stale FUSE mount before fresh mount",);
        }
        Ok(o) => {
            // Common case: nothing was mounted. fusermount3 emits
            // "entry for X not found in /etc/mtab" or similar.
            // Logged at debug; not an error.
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::debug!(
                mountpoint,
                exit = ?o.status.code(),
                stderr = %stderr.trim(),
                "fusermount3 -uz: no stale mount to evict (expected on first run)",
            );
        }
        Err(e) => {
            // fusermount3 binary missing — print a hint but don't
            // abort. The subsequent mount call will surface the
            // real error if any.
            tracing::warn!(
                mountpoint,
                error = %e,
                "could not run fusermount3 — install fuse3 if FUSE mounts misbehave",
            );
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        return;
    }

    match args[1].as_str() {
        "mount" => handle_mount(&args[2..]),
        "stage" => handle_stage(&args[2..]),
        "cache" => handle_cache(&args[2..]),
        "whoami" => handle_whoami(&args[2..]),
        "namespaces" => handle_namespaces(&args[2..]),
        "quota" => handle_quota(&args[2..]),
        "topology" => handle_topology(&args[2..]),
        "bench" => handle_bench(&args[2..]),
        "version" | "--version" | "-V" => {
            println!("kiseki-client {}", env!("CARGO_PKG_VERSION"));
        }
        "--help" | "-h" | "help" => print_usage(),
        _ => {
            eprintln!("Unknown command: {}", args[1]);
            print_usage();
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    println!(
        "\
kiseki-client -- Kiseki storage client CLI

USAGE:
    kiseki-client <COMMAND> [OPTIONS]

COMMANDS:
    mount       Mount a Kiseki filesystem via FUSE
    stage       Dataset staging (pre-fetch, status, release)
    cache       Cache management (stats, wipe)
    whoami      Show authenticated tenant identity (scrapes /cluster/info)
    namespaces  List namespaces this client is authorized for
    quota       Show this tenant's quota usage
    topology    Show the client's local topology cache
    bench       Drive PUT/GET workload against an external cluster
    version     Print version
    help        Print this help

MOUNT OPTIONS:
    --endpoint <url>         Gateway endpoint (required unless --in-memory or --seeds):
                               kiseki://host:9103   ADR-042 native TCP-framed (preferred)
                               http(s)://host:9000  S3 listener (one HTTP RTT per FUSE op)
    --seeds host1,host2,...  Multi-seed dial; takes precedence over --endpoint.
                             First seed that accepts wins. Native (kiseki://) only.
    --mountpoint <path>      Local mount path (required)
    --in-memory              Run against an in-process sandbox (dev only)
    --cache-mode <mode>      Cache mode: pinned, organic, bypass (default: organic)
    --read-only              Mount RO (default: RW). Use for read-only datasets
                             where accidental writes should fail with EROFS.
    --read-write             Compatibility alias for the default (RW) — kept so
                             existing scripts that opt in explicitly still work.
    --cache-dir <path>       Cache directory (default: /tmp/kiseki-cache)

STAGE OPTIONS:
    --dataset <path>     Stage a dataset (pre-fetch chunks into L2 cache)
    --timeout <seconds>  Staging timeout (default: no timeout)
    --status             Show staged datasets
    --release <path>     Release a staged dataset
    --release-all        Release all staged datasets

CACHE OPTIONS:
    --stats              Print cache statistics
    --wipe               Wipe all cached data (L1 + L2)

ENVIRONMENT:
    KISEKI_CACHE_DIR     Cache directory (default: /tmp/kiseki-cache)
    KISEKI_CACHE_MODE    Cache mode: pinned, organic, bypass (default: organic)
    KISEKI_CACHE_L1_MAX  L1 max bytes (default: 268435456 = 256 MB)
    KISEKI_CACHE_L2_MAX  L2 max bytes (default: 53687091200 = 50 GB)"
    );
}

/// Resolve the cache directory from the environment or default.
fn cache_dir() -> PathBuf {
    std::env::var("KISEKI_CACHE_DIR")
        .map_or_else(|_| PathBuf::from("/tmp/kiseki-cache"), PathBuf::from)
}

/// Resolve the pool directory (`cache_dir` / `default-tenant` / pool).
fn pool_dir() -> PathBuf {
    cache_dir().join("default-tenant").join("pool")
}

#[allow(clippy::too_many_lines)] // mount has lots of CLI flag wiring
fn handle_mount(args: &[String]) {
    let mut endpoint: Option<String> = None;
    let mut seeds: Vec<String> = Vec::new();
    let mut mountpoint: Option<String> = None;
    let mut cache_mode = String::from("organic");
    let mut _cache_dir: Option<String> = None;
    // F-2 (2026-05-15): default is RW. A filesystem mount that defaults
    // RO surprises every operator + script that interacts with it (write
    // returns EROFS with no log); the "HPC compute-node convention"
    // framing of the prior default was post-hoc rationalisation. RO is
    // still available via `--read-only`; `--read-write` stays as an
    // explicit opt-in alias for backwards compatibility.
    let mut read_write = true;
    let mut in_memory = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--endpoint" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --endpoint requires a value");
                    std::process::exit(2);
                }
                endpoint = Some(args[i + 1].clone());
                i += 2;
            }
            "--seeds" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --seeds requires host1,host2,... value");
                    std::process::exit(2);
                }
                seeds = args[i + 1]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                i += 2;
            }
            "--mountpoint" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --mountpoint requires a value");
                    std::process::exit(2);
                }
                mountpoint = Some(args[i + 1].clone());
                i += 2;
            }
            "--cache-mode" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --cache-mode requires a value");
                    std::process::exit(2);
                }
                cache_mode.clone_from(&args[i + 1]);
                i += 2;
            }
            "--cache-dir" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --cache-dir requires a value");
                    std::process::exit(2);
                }
                _cache_dir = Some(args[i + 1].clone());
                i += 2;
            }
            "--read-write" => {
                // Explicit opt-in: matches the new default but kept so
                // pre-F-2 scripts continue to work unmodified.
                read_write = true;
                i += 1;
            }
            "--read-only" => {
                read_write = false;
                i += 1;
            }
            "--in-memory" => {
                in_memory = true;
                i += 1;
            }
            other => {
                eprintln!("Unknown mount option: {other}");
                std::process::exit(2);
            }
        }
    }

    let mountpoint = mountpoint.unwrap_or_else(|| {
        eprintln!("Error: --mountpoint is required");
        std::process::exit(2);
    });

    // Validate endpoint vs sandbox mode. The previous behavior silently
    // fell through to an in-process sandbox whenever `--endpoint` was
    // not an http(s):// URL — that masked a 2026-05-07 GCP run where
    // every read/write was hitting an in-daemon HashMap, not the
    // cluster. Sandbox is now opt-in via `--in-memory`.
    //
    // `--seeds` takes precedence over `--endpoint` when both are given
    // — multi-seed dial is the recommended HPC posture (any one node
    // can serve the discovery, and the client routes to leaders on
    // its own via topology cache).
    if in_memory {
        if endpoint.is_some() || !seeds.is_empty() {
            eprintln!("Error: --in-memory and --endpoint / --seeds are mutually exclusive");
            std::process::exit(2);
        }
    } else if !seeds.is_empty() {
        // Promote the first seed to `endpoint` (kiseki:// scheme is
        // implied for seeds). The native code path below loops over
        // the rest if the first one is unreachable.
        let first = seeds[0].clone();
        let promoted = if first.starts_with("kiseki://") || first.starts_with("http") {
            first
        } else {
            format!("kiseki://{first}")
        };
        endpoint = Some(promoted);
        eprintln!(
            "info: --seeds={} → first reachable seed dialled (native kiseki:// only)",
            seeds.join(",")
        );
    } else {
        let ep = endpoint.as_deref().unwrap_or_else(|| {
            eprintln!(
                "Error: --endpoint or --seeds is required (or use --in-memory for a dev sandbox)"
            );
            std::process::exit(2);
        });
        let is_kiseki = ep.starts_with("kiseki://");
        let is_http = ep.starts_with("http://") || ep.starts_with("https://");
        if !(is_kiseki || is_http) {
            eprintln!(
                "Error: --endpoint must start with kiseki:// (native, preferred) \
                 or http(s):// (S3 fallback) (got '{ep}'). \
                 Use --in-memory to run an in-process sandbox.",
            );
            std::process::exit(2);
        }
    }

    let tenant = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));
    let namespace =
        kiseki_common::ids::NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default"));

    // Bug #3 fix (GCP 2026-05-09): evict any stale FUSE mount left
    // over from a prior daemon that died without unmounting. If
    // nothing is there, this is a fast no-op. Applies regardless
    // of `in_memory` mode — even sandbox runs can land on a path
    // that has a leftover from a previous mount. Without this,
    // a SIGKILL'd kiseki-client leaves the kernel with a
    // registered-but-orphaned FUSE mount and subsequent stat/access
    // blocks indefinitely.
    #[cfg(feature = "fuse")]
    {
        evict_stale_fuse_mount(&mountpoint);
    }

    // Native ADR-042 path: --endpoint is kiseki://… and `native` feature
    // is compiled in. Connects a pool of TCP-framed-postcard connections
    // to the server's TCP-framed listener (default port 9103 since the
    // 2026-05-07 fix that moved it off advisory's 9101). Returns from
    // the function on success.
    #[cfg(all(feature = "fuse", feature = "native"))]
    {
        use std::path::Path;
        if !in_memory {
            let ep_full = endpoint.as_deref().unwrap_or("");
            if let Some(rest) = ep_full.strip_prefix("kiseki://") {
                // No path component supported yet — strip any trailing
                // slashes so `kiseki://host:port/` is accepted.
                let addr = rest.trim_end_matches('/');
                if addr.is_empty() {
                    eprintln!("Error: --endpoint kiseki:// requires host:port (got '{ep_full}')");
                    std::process::exit(2);
                }
                let pool = std::env::var("KISEKI_NATIVE_GATEWAY_POOL")
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(kiseki_client::native_remote::DEFAULT_POOL_SIZE);
                println!(
                    "Mounting at {mountpoint} via native kiseki://{addr} (pool={pool}, cache_mode: {cache_mode})",
                );
                // Build the connection pool on a dedicated long-lived
                // runtime — `TcpFramedClient::connect_plaintext` spawns
                // a per-connection reader task that demuxes RPC
                // responses. If we built the pool on a one-shot
                // runtime that we then drop, those reader tasks would
                // get aborted and every subsequent FUSE op would block
                // forever on a oneshot receiver. KisekiFuse builds its
                // own multi-thread runtime via `std::thread::spawn` +
                // `mem::forget`; we mirror that pattern here so the
                // pool's reader tasks survive the connect call.
                let rt_handle = std::thread::spawn(|| {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .thread_name("kiseki-native-rt")
                        .build()
                        .expect("failed to build kiseki native runtime");
                    let handle = runtime.handle().clone();
                    std::mem::forget(runtime);
                    handle
                })
                .join()
                .expect("native runtime thread panicked");
                // Build the seed list: --seeds takes precedence and was
                // already promoted to `endpoint` above; the remaining
                // seeds are tried in turn if the first one is down.
                // The kiseki:// prefix on each entry is stripped here.
                let candidates: Vec<String> = if seeds.is_empty() {
                    vec![addr.to_owned()]
                } else {
                    seeds
                        .iter()
                        .map(|s| s.trim_start_matches("kiseki://").to_owned())
                        .collect()
                };
                let mut last_err: Option<std::io::Error> = None;
                let mut gw_opt = None;
                for candidate in &candidates {
                    match rt_handle.block_on(
                        kiseki_client::native_remote::NativeRemoteGateway::connect_plaintext(
                            candidate.clone(),
                            pool,
                        ),
                    ) {
                        Ok(g) => {
                            if candidates.len() > 1 {
                                println!("info: dialled seed {candidate}");
                            }
                            gw_opt = Some(g);
                            break;
                        }
                        Err(e) => {
                            eprintln!("warn: seed {candidate} unreachable: {e}");
                            last_err = Some(e);
                        }
                    }
                }
                let gw = gw_opt.unwrap_or_else(|| {
                    let msg = last_err
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "no seeds".into());
                    eprintln!("Error: native connect failed (all seeds): {msg}");
                    std::process::exit(1);
                });
                let fuse = kiseki_client::fuse_fs::KisekiFuse::new(gw, tenant, namespace);
                kiseki_client::fuse_daemon::mount(fuse, Path::new(&mountpoint), read_write)
                    .expect("FUSE mount failed");
                return;
            }
        }
    }

    // Networked HTTP path: --endpoint is http(s):// and `remote-http` is
    // compiled in. Returns from the function on success.
    #[cfg(all(feature = "fuse", feature = "remote-http"))]
    {
        use std::path::Path;
        if !in_memory {
            let endpoint = endpoint.clone().expect("validated above");
            if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
                println!(
                    "Mounting at {mountpoint} via remote {endpoint} (cache_mode: {cache_mode})",
                );
                let gw = kiseki_client::remote_http::RemoteHttpGateway::new(endpoint);
                let fuse = kiseki_client::fuse_fs::KisekiFuse::new(gw, tenant, namespace);
                kiseki_client::fuse_daemon::mount(fuse, Path::new(&mountpoint), read_write)
                    .expect("FUSE mount failed");
                return;
            }
        }
    }

    // Endpoint was passed but the binary was built without the feature
    // that handles it. Refuse rather than silently sandboxing — same
    // failure-mode lesson as the 2026-05-07 GCP run.
    #[cfg(all(feature = "fuse", not(feature = "native")))]
    if !in_memory {
        if let Some(ep) = endpoint.as_deref() {
            if ep.starts_with("kiseki://") {
                eprintln!(
                    "Error: this binary was built without `native` — \
                     kiseki:// endpoints cannot be served. Rebuild \
                     with `--features native` or use http://.",
                );
                std::process::exit(1);
            }
        }
    }
    #[cfg(all(feature = "fuse", not(feature = "remote-http")))]
    if !in_memory {
        if let Some(ep) = endpoint.as_deref() {
            if ep.starts_with("http://") || ep.starts_with("https://") {
                eprintln!(
                    "Error: this binary was built without `remote-http` — \
                     http:// endpoints cannot be served. Rebuild with \
                     `--features remote-http` or use kiseki://.",
                );
                std::process::exit(1);
            }
        }
    }

    #[cfg(feature = "fuse")]
    {
        use std::path::Path;
        // Sandbox path. Reaching here means `in_memory == true` (the
        // remote-http branch above already returned for the network
        // case, and the not(remote-http) branch exited if the
        // operator didn't ask for sandbox).
        let _ = endpoint;
        println!("Mounting at {mountpoint} via in-memory sandbox (cache_mode: {cache_mode})");
        let shard = kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1));
        let compositions = kiseki_composition::composition::CompositionStore::new();
        compositions.add_namespace(kiseki_composition::namespace::Namespace {
            id: namespace,
            tenant_id: tenant,
            shard_id: shard,
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        });
        let master_key = kiseki_crypto::keys::SystemMasterKey::new(
            [0x42; 32],
            kiseki_common::tenancy::KeyEpoch(1),
        );
        let gw = kiseki_gateway::InMemoryGateway::new(
            compositions,
            kiseki_chunk::arc_async(kiseki_chunk::store::ChunkStore::new()),
            master_key,
        );
        let fuse = kiseki_client::fuse_fs::KisekiFuse::new(gw, tenant, namespace);
        kiseki_client::fuse_daemon::mount(fuse, Path::new(&mountpoint), read_write)
            .expect("FUSE mount failed");
    }
    #[cfg(not(feature = "fuse"))]
    {
        let _ = (
            read_write, endpoint, in_memory, tenant, namespace, mountpoint, cache_mode,
        );
        eprintln!("FUSE support not compiled — rebuild with --features fuse");
        std::process::exit(1);
    }
}

fn handle_stage(args: &[String]) {
    if args.is_empty() {
        eprintln!(
            "Error: stage requires an option (--dataset, --status, --release, --release-all)"
        );
        std::process::exit(2);
    }

    match args[0].as_str() {
        "--dataset" => stage_dataset(&args[1..]),
        "--status" => stage_status(),
        "--release" => stage_release(&args[1..]),
        "--release-all" => stage_release_all(),
        other => {
            eprintln!("Unknown stage option: {other}");
            std::process::exit(2);
        }
    }
}

fn staging_mgr_from_pool() -> StagingManager {
    let pool = pool_dir();
    StagingManager::new(
        if pool.exists() { Some(pool) } else { None },
        StagingConfig::default(),
    )
}

fn stage_dataset(args: &[String]) {
    if args.is_empty() {
        eprintln!("Error: --dataset requires a path argument");
        std::process::exit(2);
    }
    let dataset_path = &args[0];

    // Parse optional --timeout.
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--timeout" {
            if i + 1 >= args.len() {
                eprintln!("Error: --timeout requires a value");
                std::process::exit(2);
            }
            let _timeout: u64 = args[i + 1].parse().unwrap_or_else(|_| {
                eprintln!("Error: --timeout value must be a number");
                std::process::exit(2);
            });
            i += 2;
        } else {
            eprintln!("Unknown option: {}", args[i]);
            std::process::exit(2);
        }
    }

    let pool = pool_dir();
    let _ = std::fs::create_dir_all(&pool);
    let mut mgr = StagingManager::new(Some(pool), StagingConfig::default());

    // Record a staging intent. Actual chunk fetching requires a gateway
    // connection which is not yet wired up in the CLI. For now we record
    // the manifest so that --status reports it.
    mgr.record_staged(dataset_path.clone(), &[], 0);

    println!("Staging request recorded for: {dataset_path}");
    println!("Note: actual chunk pre-fetch requires a running gateway connection.");
    println!("      Use the FUSE mount with --stage for live staging.");
}

fn stage_status() {
    let mgr = staging_mgr_from_pool();
    let datasets = mgr.list();
    if datasets.is_empty() {
        println!("No datasets currently staged.");
    } else {
        println!("{:<40} {:>10} {:>12}", "NAMESPACE PATH", "CHUNKS", "BYTES");
        println!("{}", "-".repeat(66));
        for ds in &datasets {
            println!(
                "{:<40} {:>10} {:>12}",
                ds.namespace_path,
                ds.chunk_ids.len(),
                format_bytes(ds.bytes),
            );
        }
        println!();
        println!(
            "Total: {} dataset(s), {}",
            datasets.len(),
            format_bytes(mgr.total_bytes())
        );
    }
}

fn stage_release(args: &[String]) {
    if args.is_empty() {
        eprintln!("Error: --release requires a path argument");
        std::process::exit(2);
    }
    let dataset_path = &args[0];
    let mut mgr = staging_mgr_from_pool();

    let released = mgr.release(dataset_path);
    if released.is_empty() {
        println!("No staged dataset found for: {dataset_path}");
    } else {
        println!("Released {} chunk(s) from: {dataset_path}", released.len());
    }
}

fn stage_release_all() {
    let mut mgr = staging_mgr_from_pool();
    let released = mgr.release_all();
    println!(
        "Released {} chunk(s) from all staged datasets.",
        released.len()
    );
}

fn handle_cache(args: &[String]) {
    if args.is_empty() {
        eprintln!("Error: cache requires an option (--stats, --wipe)");
        std::process::exit(2);
    }

    match args[0].as_str() {
        "--stats" => {
            let config = cache_config_from_env();
            match CacheManager::new(&config) {
                Ok(mgr) => {
                    let stats = mgr.stats();
                    println!("Cache mode:       {:?}", config.mode);
                    println!("L1 bytes used:    {}", format_bytes(stats.l1_bytes));
                    println!("L2 bytes used:    {}", format_bytes(stats.l2_bytes));
                    println!("L1 hits:          {}", stats.l1_hits);
                    println!("L2 hits:          {}", stats.l2_hits);
                    println!("Misses:           {}", stats.misses);
                    println!("Bypasses:         {}", stats.bypasses);
                    println!("Errors:           {}", stats.errors);
                    println!("Metadata hits:    {}", stats.meta_hits);
                    println!("Metadata misses:  {}", stats.meta_misses);
                    println!("Wipes:            {}", stats.wipes);
                }
                Err(e) => {
                    eprintln!("Error initializing cache: {e}");
                    std::process::exit(1);
                }
            }
        }
        "--wipe" => {
            let config = cache_config_from_env();
            match CacheManager::new(&config) {
                Ok(mut mgr) => {
                    mgr.wipe();
                    println!("Cache wiped (L1 + L2 + metadata).");
                }
                Err(e) => {
                    eprintln!("Error initializing cache: {e}");
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("Unknown cache option: {other}");
            std::process::exit(2);
        }
    }
}

fn cache_config_from_env() -> CacheConfig {
    let mode = match std::env::var("KISEKI_CACHE_MODE").as_deref() {
        Ok("pinned") => CacheMode::Pinned,
        Ok("bypass") => CacheMode::Bypass,
        _ => CacheMode::Organic,
    };
    let max_memory_bytes = std::env::var("KISEKI_CACHE_L1_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256 * 1024 * 1024);
    let max_cache_bytes = std::env::var("KISEKI_CACHE_L2_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50 * 1024 * 1024 * 1024);

    CacheConfig {
        mode,
        max_memory_bytes,
        max_cache_bytes,
        cache_dir: cache_dir(),
        ..CacheConfig::default()
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.1} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ---------------------------------------------------------------------------
// whoami / namespaces / quota / topology — minimal scrapers over HTTP.
//
// These commands hit the server's admin HTTP surface on port 9090. They
// take an optional --endpoint flag pointing at the metrics HTTP URL
// (defaults to KISEKI_ENDPOINT or http://localhost:9090). The client
// CLI is stdlib-only just like kiseki-admin — no extra deps for
// these read-only operations.
// ---------------------------------------------------------------------------

fn default_admin_endpoint() -> String {
    std::env::var("KISEKI_ENDPOINT").unwrap_or_else(|_| "http://localhost:9090".to_string())
}

fn parse_admin_endpoint(args: &[String]) -> String {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--endpoint" && i + 1 < args.len() {
            return args[i + 1].clone();
        }
        if let Some(v) = args[i].strip_prefix("--endpoint=") {
            return v.to_string();
        }
        i += 1;
    }
    default_admin_endpoint()
}

fn handle_whoami(args: &[String]) {
    let endpoint = parse_admin_endpoint(args);
    // Preferred: ask the server for the authenticated principal via
    // `/admin/whoami`. The server reports the SAN extracted from the
    // request's mTLS handshake when available, plus any
    // server-resolved tenant/workload mapping (ADR-038 §D4).
    //
    // Fallback chain when the dedicated endpoint is unavailable
    // (older server, plain HTTP listener): scrape `/cluster/info`
    // for node id only and merge the env tenant.
    let env_tenant = std::env::var("KISEKI_TENANT_ID").ok();
    let body_result = http_get_admin(&endpoint, "/admin/whoami")
        .or_else(|_| http_get_admin(&endpoint, "/cluster/info"));
    match body_result {
        Ok(body) => {
            let rendered = format_whoami(&body, &endpoint, env_tenant.as_deref());
            print!("{rendered}");
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

/// Render the `whoami` block from a JSON body (either `/admin/whoami`
/// or `/cluster/info`). The SAN-aware output takes precedence over the
/// env tenant fallback when the server surfaces a `san` field.
fn format_whoami(body: &str, endpoint: &str, env_tenant: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "Endpoint:   {endpoint}");
    let node_id = json_u64(body, "node_id").unwrap_or(0);
    let _ = writeln!(out, "Connected:  node {node_id}");

    let san = json_str(body, "san");
    let tenant_from_body = json_str(body, "tenant_id");
    let workload_from_body = json_str(body, "workload_id");

    match san {
        Some(s) if !s.is_empty() => {
            // The server saw an mTLS SAN — use it as the principal.
            let _ = writeln!(out, "Principal:  {s}");
        }
        _ => {
            // No SAN — the connection isn't mTLS-authenticated.
            let _ = writeln!(
                out,
                "Principal:  (no SAN) — connection is not mTLS-authenticated"
            );
        }
    }

    let tenant_display = tenant_from_body
        .map(str::to_string)
        .or_else(|| env_tenant.map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let _ = writeln!(out, "Tenant:     {tenant_display}");

    if let Some(wl) = workload_from_body {
        if !wl.is_empty() {
            let _ = writeln!(out, "Workload:   {wl}");
        }
    }
    out
}

/// Extract a string value for `key` from a flat JSON object. Mirrors
/// `kiseki-admin`'s parser — stdlib only.
fn json_str<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    let stripped = after_ws.strip_prefix('"')?;
    let end = stripped.find('"')?;
    Some(&stripped[..end])
}

fn handle_namespaces(args: &[String]) {
    let endpoint = parse_admin_endpoint(args);
    let sub = args.first().map_or("list", String::as_str);
    if sub != "list" {
        eprintln!("Usage: kiseki-client namespaces list [--endpoint URL]");
        std::process::exit(2);
    }
    match http_get_admin(&endpoint, "/admin/tenants/namespaces") {
        Ok(body) => {
            // Tenant filtering: production filters by the client's own
            // tenant. The HTTP endpoint already only exposes the
            // namespaces the responding node knows; for a single-
            // tenant deploy that is all of them.
            print!("{body}");
            println!();
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

fn handle_quota(args: &[String]) {
    let endpoint = parse_admin_endpoint(args);
    // Quotas live on the gRPC ControlService.SetQuota path; there's
    // no read-only quota endpoint over HTTP today. Scrape the gateway
    // bytes counters as a usage proxy.
    match http_get_admin(&endpoint, "/metrics") {
        Ok(body) => {
            let mut bytes_written = 0u64;
            let mut bytes_read = 0u64;
            for line in body.lines() {
                if line.starts_with("kiseki_gateway_bytes_written_total ") {
                    if let Some(v) = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()) {
                        bytes_written = v;
                    }
                } else if line.starts_with("kiseki_gateway_bytes_read_total ") {
                    if let Some(v) = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()) {
                        bytes_read = v;
                    }
                }
            }
            println!("Tenant usage (from gateway counters, no per-tenant breakdown today):");
            println!("  Bytes written: {}", format_bytes(bytes_written));
            println!("  Bytes read:    {}", format_bytes(bytes_read));
            println!("Note: per-tenant quota query is not yet exposed over HTTP — see followups.");
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

fn handle_topology(args: &[String]) {
    let endpoint = parse_admin_endpoint(args);
    match http_get_admin(&endpoint, "/admin/topology/shards") {
        Ok(body) => {
            print!("{body}");
            println!();
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

/// `kiseki-client bench` — drive PUT/GET against an externally-running
/// cluster. Closes #58 (no `kiseki-client bench` command for native).
///
/// Output format mirrors `kiseki-profile run`'s human / `--json`
/// shapes so numbers compare directly across the two tools.
#[cfg(any(feature = "native", feature = "remote-http"))]
#[allow(clippy::too_many_lines)] // hand-rolled flag parser + 7 flags + help
fn handle_bench(args: &[String]) {
    use kiseki_client::bench::{BenchConfig, NativeBinding, Shape};

    let mut endpoint: Option<String> = None;
    let mut shape = Shape::PutHeavy;
    let mut binding = NativeBinding::Tcp;
    let mut concurrency: usize = 16;
    let mut object_size: usize = 65_536;
    let mut duration_secs: u64 = 30;
    let mut warmup_objects: usize = 256;
    let mut json = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--endpoint" => {
                endpoint = args.get(i + 1).cloned();
                i += 2;
            }
            "--shape" => {
                shape = match args.get(i + 1).map(String::as_str) {
                    Some("put-heavy") => Shape::PutHeavy,
                    Some("get-heavy") => Shape::GetHeavy,
                    Some("mixed") => Shape::Mixed,
                    other => {
                        eprintln!("--shape expects put-heavy|get-heavy|mixed, got {other:?}");
                        std::process::exit(1);
                    }
                };
                i += 2;
            }
            "--binding" => {
                binding = match args.get(i + 1).map(String::as_str) {
                    Some("tcp") => NativeBinding::Tcp,
                    Some("grpc") => NativeBinding::Grpc,
                    other => {
                        eprintln!("--binding expects tcp|grpc, got {other:?}");
                        std::process::exit(1);
                    }
                };
                i += 2;
            }
            "--concurrency" => {
                concurrency = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(concurrency);
                i += 2;
            }
            "--object-size" => {
                object_size = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(object_size);
                i += 2;
            }
            "--duration-secs" => {
                duration_secs = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(duration_secs);
                i += 2;
            }
            "--warmup-objects" => {
                warmup_objects = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(warmup_objects);
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--help" | "-h" => {
                println!(
                    "\
kiseki-client bench -- drive PUT/GET against an externally-running cluster

OPTIONS:
    --endpoint <url>          REQUIRED. kiseki://host:9103 (native TCP-framed)
                              or kiseki://host:9100 (with --binding grpc) or
                              http(s)://host:9000 (S3 listener)
    --shape <s>               put-heavy | get-heavy | mixed     (default: put-heavy)
    --binding <b>             tcp | grpc       (default: tcp; only for kiseki://)
    --concurrency <N>         in-flight ops    (default: 16)
    --object-size <bytes>     payload size     (default: 65536)
    --duration-secs <N>       wall-clock cap   (default: 30)
    --warmup-objects <N>      pre-populate for GET shapes (default: 256)
    --json                    machine-readable single-line JSON
"
                );
                return;
            }
            other => {
                eprintln!("unknown bench arg: {other}");
                std::process::exit(1);
            }
        }
    }

    let Some(endpoint) = endpoint else {
        eprintln!("--endpoint is required (try `kiseki-client bench --help`)");
        std::process::exit(1);
    };

    let cfg = BenchConfig {
        endpoint,
        binding,
        shape,
        concurrency,
        object_size,
        duration: std::time::Duration::from_secs(duration_secs),
        warmup_objects,
        json,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("kiseki-bench")
        .build()
        .expect("tokio runtime");
    if let Err(e) = rt.block_on(kiseki_client::bench::run(cfg)) {
        eprintln!("bench failed: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(any(feature = "native", feature = "remote-http")))]
fn handle_bench(_args: &[String]) {
    eprintln!(
        "bench requires the `native` and/or `remote-http` feature; \
         rebuild kiseki-client with --features native,remote-http"
    );
    std::process::exit(1);
}

// --- HTTP helpers (stdlib only, mirrors kiseki-admin's helpers) ---

fn http_get_admin(endpoint: &str, path: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let host_port = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .and_then(|rest| rest.split('/').next())
        .ok_or("invalid endpoint URL")?;
    let mut stream = TcpStream::connect(host_port)
        .map_err(|e| format!("connection failed ({host_port}): {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let body_start = text
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .ok_or("malformed HTTP response")?;
    let body = &text[body_start..];
    if text[..body_start]
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        Ok(decode_chunked(body))
    } else {
        Ok(body.to_string())
    }
}

fn decode_chunked(input: &str) -> String {
    let mut result = String::new();
    let mut remaining = input;
    loop {
        let trimmed = remaining.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let line_end = trimmed.find("\r\n").unwrap_or(trimmed.len());
        let size = usize::from_str_radix(trimmed[..line_end].trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let data_start = line_end + 2;
        if data_start + size <= trimmed.len() {
            result.push_str(&trimmed[data_start..data_start + size]);
            remaining = &trimmed[data_start + size..];
            if remaining.starts_with("\r\n") {
                remaining = &remaining[2..];
            }
        } else {
            result.push_str(&trimmed[data_start..]);
            break;
        }
    }
    result
}

fn json_u64(json: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    let end = after_ws
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(after_ws.len());
    after_ws[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_admin_endpoint_picks_flag_value() {
        let args = vec!["--endpoint".to_string(), "http://x:9090".to_string()];
        assert_eq!(parse_admin_endpoint(&args), "http://x:9090");
    }

    #[test]
    fn parse_admin_endpoint_falls_back_to_default() {
        let args: Vec<String> = vec![];
        // Default may be overridden by KISEKI_ENDPOINT in the env;
        // verify it's an http(s) URL.
        let ep = parse_admin_endpoint(&args);
        assert!(ep.starts_with("http://") || ep.starts_with("https://"));
    }

    #[test]
    fn json_u64_extracts_simple_value() {
        let body = r#"{"node_id": 42, "other": "x"}"#;
        assert_eq!(json_u64(body, "node_id"), Some(42));
    }

    // --- D6: SAN identity is surfaced in whoami when the server reports it ---

    #[test]
    fn whoami_san_from_body_preferred_over_env() {
        // `/admin/whoami` response. When `san` is present, the CLI MUST
        // print it as the authenticated principal. The env fallback
        // only fires when the server didn't report one.
        let body = r#"{"node_id": 1, "san": "spiffe://kiseki/tenant/acme/wl/trainer", "tenant_id": "acme", "workload_id": "trainer"}"#;
        let rendered = format_whoami(body, "http://localhost:9090", Some("env-fallback"));
        assert!(
            rendered.contains("spiffe://kiseki/tenant/acme/wl/trainer"),
            "SAN principal missing from output: {rendered}"
        );
        assert!(rendered.contains("acme"), "tenant missing: {rendered}");
        assert!(rendered.contains("trainer"), "workload missing: {rendered}");
    }

    #[test]
    fn whoami_falls_back_to_env_when_san_absent() {
        let body = r#"{"node_id": 7}"#;
        let rendered = format_whoami(body, "http://localhost:9090", Some("env-fallback"));
        assert!(
            rendered.contains("env-fallback"),
            "env tenant fallback missing: {rendered}"
        );
        // We should signal that no mTLS SAN was negotiated.
        assert!(
            rendered.contains("none")
                || rendered.contains("not authenticated")
                || rendered.contains("(no SAN)"),
            "absent-SAN marker missing: {rendered}"
        );
    }

    #[test]
    fn whoami_handles_no_env_fallback() {
        let body = r#"{"node_id": 7}"#;
        let rendered = format_whoami(body, "http://localhost:9090", None);
        // Tolerant of either explicit `unknown` or empty: just check
        // the helper doesn't panic and emits node info.
        assert!(rendered.contains("node 7"), "node line missing: {rendered}");
    }
}
