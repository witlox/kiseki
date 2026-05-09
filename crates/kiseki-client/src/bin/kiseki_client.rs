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
        "version" => println!("kiseki-client {}", env!("CARGO_PKG_VERSION")),
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
    version     Print version
    help        Print this help

MOUNT OPTIONS:
    --endpoint <url>         Gateway endpoint (required unless --in-memory):
                               kiseki://host:9103   ADR-042 native TCP-framed (preferred)
                               http(s)://host:9000  S3 listener (one HTTP RTT per FUSE op)
    --mountpoint <path>      Local mount path (required)
    --in-memory              Run against an in-process sandbox (dev only)
    --cache-mode <mode>      Cache mode: pinned, organic, bypass (default: organic)
    --read-write             Mount RW (default: RO — HPC compute-node default)
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
    let mut mountpoint: Option<String> = None;
    let mut cache_mode = String::from("organic");
    let mut _cache_dir: Option<String> = None;
    let mut read_write = false;
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
                read_write = true;
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
    // Three URL schemes:
    //   `kiseki://host:9103` — ADR-042 native TCP-framed binding
    //                          (preferred for FUSE: streaming, no h2/HTTP
    //                          framing tax, kernel-many-small-reads
    //                          friendly).
    //   `http(s)://host:9000` — S3 HTTP listener (one HTTP RTT per FUSE
    //                          read; fine for a quick smoke or tiny
    //                          objects, terrible for sequential I/O).
    if in_memory {
        if endpoint.is_some() {
            eprintln!("Error: --in-memory and --endpoint are mutually exclusive");
            std::process::exit(2);
        }
    } else {
        let ep = endpoint.as_deref().unwrap_or_else(|| {
            eprintln!("Error: --endpoint is required (or use --in-memory for a dev sandbox)");
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
                let gw = rt_handle
                    .block_on(
                        kiseki_client::native_remote::NativeRemoteGateway::connect_plaintext(
                            addr.to_owned(),
                            pool,
                        ),
                    )
                    .unwrap_or_else(|e| {
                        eprintln!("Error: native connect to {addr} failed: {e}");
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
