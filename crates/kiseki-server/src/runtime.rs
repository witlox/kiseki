//! Runtime composition — wires all contexts and starts gRPC servers.

use std::net::SocketAddr;
use std::sync::Arc;

use kiseki_advisory::budget::BudgetConfig;
use kiseki_advisory::grpc::AdvisoryGrpc;
use kiseki_audit::AuditOps;
use kiseki_control::grpc::ControlGrpc;
use kiseki_control::tenant::TenantStore;
use kiseki_keymanager::grpc::KeyManagerGrpc;
use kiseki_log::grpc::LogGrpc;
use kiseki_proto::v1::control_service_server::ControlServiceServer;
use kiseki_proto::v1::key_manager_service_server::KeyManagerServiceServer;
use kiseki_proto::v1::log_service_server::LogServiceServer;
use kiseki_proto::v1::workflow_advisory_service_server::WorkflowAdvisoryServiceServer;
use kiseki_view::ViewOps;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use crate::config::{ServerConfig, TlsFiles};

/// Pick the DS uaddrs the MDS will advertise via GETDEVICEINFO.
///
/// Precedence:
/// 1. `KISEKI_DS_PEERS` (explicit per-node DS endpoints) — required
///    for localhost-multi-node where each node binds a distinct
///    ephemeral DS port.
/// 2. `raft_peers` host-substitution + `ds_addr.port()` — the
///    containerized/hostnamed case where every node uses the same
///    DS port (typically 2052).
/// 3. Single-node fallback: when no peers are known but the local
///    node has a `ds_addr` configured, advertise *that* address.
///    Without this branch single-node clusters with an ephemeral
///    DS port (e.g. the profile harness) silently advertised the
///    hard-coded `127.0.0.1:2052` from `MdsLayoutConfig::default()`,
///    so every pNFS read got `Connection refused`.
fn compute_storage_ds_addrs(
    ds_peers: &[(u64, String)],
    raft_peers: &[(u64, String)],
    ds_addr: Option<SocketAddr>,
) -> Vec<String> {
    if !ds_peers.is_empty() {
        return ds_peers.iter().map(|(_, addr)| addr.clone()).collect();
    }
    if !raft_peers.is_empty() {
        let ds_port = ds_addr.map_or(2052, |a| a.port());
        return raft_peers
            .iter()
            .map(|(_, addr)| {
                let host = addr.split(':').next().unwrap_or(addr);
                format!("{host}:{ds_port}")
            })
            .collect();
    }
    // Single-node: the local node *is* the storage DS.
    if let Some(addr) = ds_addr {
        return vec![addr.to_string()];
    }
    Vec::new()
}

/// Pick the per-node identity source for the at-rest key store
/// (Phase 14e). Precedence: SPIFFE > mTLS > file-in-data-dir.
///
/// Returns `Err` only if every source is unavailable — which shouldn't
/// happen here because the file fallback always succeeds when the
/// data dir exists.
fn select_node_identity_or_die(
    cfg: &ServerConfig,
    data_dir: &std::path::Path,
) -> Result<Box<dyn kiseki_keymanager::node_identity::NodeIdentitySource>, Box<dyn std::error::Error>>
{
    use kiseki_keymanager::node_identity::{select_node_identity, NodeIdentityInputs};
    let mtls_key = cfg.tls.as_ref().map(|t| t.key_path.as_path());
    select_node_identity(&NodeIdentityInputs {
        spiffe_path: cfg.spiffe_socket.as_deref(),
        mtls_key_path: mtls_key,
        data_dir: Some(data_dir),
    })
    .ok_or_else(|| "no node identity source available".into())
}

/// Build a tonic `ServerTlsConfig` from PEM files.
fn build_tls(files: &TlsFiles) -> Result<ServerTlsConfig, Box<dyn std::error::Error>> {
    let ca_pem = std::fs::read(&files.ca_path)
        .map_err(|e| format!("read CA {}: {e}", files.ca_path.display()))?;
    let cert_pem = std::fs::read(&files.cert_path)
        .map_err(|e| format!("read cert {}: {e}", files.cert_path.display()))?;
    let key_pem = std::fs::read(&files.key_path)
        .map_err(|e| format!("read key {}: {e}", files.key_path.display()))?;

    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(&cert_pem, &key_pem))
        .client_ca_root(Certificate::from_pem(&ca_pem));

    Ok(tls)
}

/// Build a per-peer fabric `Channel` to a peer's data-path gRPC. The
/// peer endpoint is host:port; this function strips the colon-port,
/// rewrites the URI scheme to `https://` (or `http://` for plaintext),
/// and applies mTLS using the shared cluster CA + this node's identity
/// when `tls_files` is `Some`. Phase 16a step 12.
fn build_fabric_channel(
    peer_addr: &str,
    tls_files: Option<&TlsFiles>,
) -> Result<tonic::transport::Channel, Box<dyn std::error::Error + Send + Sync>> {
    use tonic::transport::{ClientTlsConfig, Endpoint};

    let scheme = if tls_files.is_some() { "https" } else { "http" };
    // Default the SNI to the host portion of peer_addr; the
    // shared-cluster cert lists all node DNS names as SANs (see
    // tests/e2e/gen-tls-certs.sh).
    let host = peer_addr
        .split(':')
        .next()
        .ok_or("peer addr missing host")?
        .to_owned();
    let uri: tonic::transport::Uri = format!("{scheme}://{peer_addr}")
        .parse()
        .map_err(|e| format!("peer URI parse: {e}"))?;

    let mut endpoint = Endpoint::from(uri)
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(5))
        // HTTP/2 flow-control windows. Tonic / hyper default is
        // 64 KiB per stream — for kiseki's 64+ MiB fabric envelopes
        // that's 1024+ WINDOW_UPDATE round-trips per call. On a
        // cross-AZ link with ~1 ms RTT that adds ≥1 s of pure
        // bookkeeping latency. The 2026-05-03 GCP transport-profile
        // run measured 2 s avg fabric PutFragment on a 28 Gbps
        // wire, with 71 % of S3 PUTs hitting `quorum_lost` because
        // peer ops missed the 5 s timeout. 16 MiB stream window
        // collapses the round-trip count for a 64 MiB body to 4.
        .initial_stream_window_size(16 * 1024 * 1024)
        .initial_connection_window_size(32 * 1024 * 1024);

    if let Some(files) = tls_files {
        let ca_pem = std::fs::read(&files.ca_path)
            .map_err(|e| format!("read fabric CA {}: {e}", files.ca_path.display()))?;
        let cert_pem = std::fs::read(&files.cert_path)
            .map_err(|e| format!("read fabric cert {}: {e}", files.cert_path.display()))?;
        let key_pem = std::fs::read(&files.key_path)
            .map_err(|e| format!("read fabric key {}: {e}", files.key_path.display()))?;
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&ca_pem))
            .identity(Identity::from_pem(&cert_pem, &key_pem))
            .domain_name(host);
        endpoint = endpoint
            .tls_config(tls)
            .map_err(|e| format!("fabric TLS config: {e}"))?;
    }

    let channel = endpoint.connect_lazy(); // lazy: failed peers don't block startup
    Ok(channel)
}

/// Map a Raft peer address to the fabric endpoint on the same node.
///
/// `cfg.raft_peers` carries `host:RAFT_PORT` entries (the addresses
/// the consensus log uses), but `ClusterChunkService` binds to
/// `cfg.data_addr`'s port — a different gRPC server on the same
/// host. Without this remapping, fabric `PutFragment` fan-out lands
/// on the Raft port and quorum collapses to leader-only.
///
/// Strategy: split off the trailing `:port`, keep everything before
/// it as the host (preserving bracketed IPv6 literals), and append
/// `:data_port`. Returns the original string if it doesn't carry a
/// port (defensive — the caller logs and skips).
fn fabric_addr_from_raft_peer(raft_peer: &str, data_port: u16) -> String {
    raft_peer.rsplit_once(':').map_or_else(
        || raft_peer.to_owned(),
        |(host, _port)| format!("{host}:{data_port}"),
    )
}

/// Recursive directory size in bytes. Tolerates I/O errors (returns
/// the partial sum). Used by the periodic composition-store gauge —
/// fjall is a keyspace directory rather than a single file.
fn dir_size_recursive(path: &std::path::Path) -> u64 {
    let Ok(read) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total: u64 = 0;
    for entry in read.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total = total.saturating_add(dir_size_recursive(&entry.path()));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Run the main data-path server.
///
/// `workflow_table` is shared with the advisory runtime — see
/// `main.rs`. The data-path gateway uses it to validate the
/// `x-kiseki-workflow-ref` header (ADR-021 §3.b / I-WA1).
#[allow(clippy::too_many_lines)]
pub async fn run_main(
    cfg: ServerConfig,
    workflow_table: std::sync::Arc<std::sync::Mutex<kiseki_advisory::WorkflowTable>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // --- Context construction ---

    // System disk detection (ADR-030).
    if let Some(ref dir) = cfg.data_dir {
        let capacity = crate::system_disk::compute_capacity(
            dir,
            cfg.meta_soft_limit_pct,
            cfg.meta_hard_limit_pct,
        );
        crate::system_disk::warn_if_rotational(capacity.media_type);
        tracing::info!(
            media_type = ?capacity.media_type,
            total_gb = capacity.total_bytes / (1024 * 1024 * 1024),
            soft_limit_gb = capacity.soft_limit_bytes / (1024 * 1024 * 1024),
            budget_gb = capacity.small_file_budget_bytes / (1024 * 1024 * 1024),
            "system disk detected",
        );
    }

    // Node identity for multi-node Raft.
    if cfg.node_id > 0 {
        tracing::info!(
            node_id = cfg.node_id,
            peers = cfg.raft_peers.len(),
            raft_addr = ?cfg.raft_addr,
            "node identity configured",
        );
    }

    // Key Manager: persistent (redb) if KISEKI_DATA_DIR set, otherwise in-memory.
    // Phase 14e: every persisted entry is wrapped in AES-GCM keyed off
    // a per-node identity (SPIFFE > mTLS > file fallback).
    let salt = cfg.node_id.to_be_bytes();
    let key_store = if let Some(ref dir) = cfg.data_dir {
        std::fs::create_dir_all(dir.join("keys")).ok();
        let identity = select_node_identity_or_die(&cfg, dir)?;
        tracing::info!(source = identity.kind(), "key store at-rest identity");
        let store = kiseki_keymanager::PersistentKeyStore::open(
            &dir.join("keys").join("epochs"),
            &*identity,
            &salt,
        )
        .map_err(|e| format!("persistent key store: {e}"))?;
        tracing::info!(
            epoch = store.health().current_epoch.unwrap_or(0),
            "key manager: persistent (fjall) ready",
        );
        store
    } else {
        // In-memory: use a process-scoped tempdir for both the
        // keyspace and the file-based node identity. Ephemeral by
        // design.
        let tmp = std::env::temp_dir().join(format!("kiseki-keys-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).ok();
        let identity = kiseki_keymanager::node_identity::FileIdentitySource::new(
            tmp.join("node-identity.key"),
        )
        .map_err(|e| format!("ephemeral node identity: {e}"))?;
        let store =
            kiseki_keymanager::PersistentKeyStore::open(&tmp.join("epochs"), &identity, &salt)
                .map_err(|e| format!("key store init: {e}"))?;
        tracing::info!(
            epoch = store.health().current_epoch.unwrap_or(0),
            "key manager: in-memory (ephemeral) ready",
        );
        store
    };
    let key_store_inner = Arc::new(key_store);

    // Small object store for inline files (ADR-030).
    // Created before the log store so Raft state machines can use it.
    let small_store: Option<std::sync::Arc<kiseki_chunk::SmallObjectStore>> = if let Some(ref dir) =
        cfg.data_dir
    {
        std::fs::create_dir_all(dir.join("small")).ok();
        let store = kiseki_chunk::SmallObjectStore::open(&dir.join("small").join("objects.redb"))
            .map_err(|e| format!("small object store: {e}"))?;
        tracing::info!(
            path = %dir.display(),
            "small object store: persistent (redb)",
        );
        Some(std::sync::Arc::new(store))
    } else {
        None
    };

    // Metrics: built early so per-shard `RaftRpcListener` metrics
    // (ADR-041 §"Observability") are wired into the listener BEFORE
    // the first shard's lazy-init binds. Other subsystems (fabric,
    // gateway, etc.) thread `Arc<...>` clones from this same struct
    // throughout setup.
    let metrics = crate::metrics::KisekiMetrics::new();

    // Observability opt-out for performance benchmarks. Defaults
    // ON for production deployments; the `infra/gcp/transport`
    // perf profile sets `KISEKI_OBSERVABILITY=off` so the GCP run
    // gets a clean baseline (no metric-record overhead, no
    // wrapper vtable dispatch on the data path). The hot-path
    // tracing spans are already at `level = "debug"`, so they
    // short-circuit at production INFO/WARN regardless of this
    // flag.
    let observability_enabled = std::env::var("KISEKI_OBSERVABILITY").map_or(true, |v| {
        !matches!(
            v.to_lowercase().as_str(),
            "off" | "0" | "false" | "disabled"
        )
    });
    if !observability_enabled {
        tracing::info!(
            "observability: wrappers disabled via KISEKI_OBSERVABILITY \
             — metric records on the LogOps / KeyManagerOps hot paths skipped"
        );
    }

    // Wrap the key store with `InstrumentedKeyManager` for
    // metric recording + tracing spans. When observability is
    // disabled, hand the bare `Arc<PersistentKeyStore>` to the
    // gRPC handler instead. `KeyManagerGrpc<T: ?Sized>` accepts
    // both the wrapper concrete type and the underlying trait
    // object so the choice is at runtime construction.
    let key_store: Arc<dyn kiseki_keymanager::KeyManagerOps> = if observability_enabled {
        Arc::new(kiseki_keymanager::InstrumentedKeyManager::new(
            key_store_inner,
            Arc::clone(&metrics.keymanager),
        ))
    } else {
        key_store_inner
    };

    // Log store: Raft (multi-node), persistent (redb), or in-memory.
    let bootstrap_shard = kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1));
    let bootstrap_tenant = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));

    // Holds the control-plane Raft store on multi-node deployments.
    // None on single-node / persistent / in-memory paths — those run
    // without cluster consensus on the namespace shard map (ADR-033
    // §4 only applies when there's a real cluster to coordinate
    // across).
    let mut cluster_control_store: Option<Arc<crate::cluster_control::OpenRaftControlStore>> = None;
    // Typed `RaftShardStore` handle for the storage admin RPC's
    // `initialize_shard` call after `RecordSplit` commits. Same
    // gating as `cluster_control_store` — only set on multi-node.
    let mut raft_shard_store_for_admin: Option<Arc<kiseki_log::RaftShardStore>> = None;
    let log_store: Arc<dyn kiseki_log::LogOps + Send + Sync> = if cfg.node_id > 0
        && cfg.raft_peers.len() > 1
    {
        // Multi-node Raft: consensus-replicated log store.
        let peers: std::collections::BTreeMap<u64, String> =
            cfg.raft_peers.iter().cloned().collect();
        let raft_addr_str = cfg
            .raft_addr
            .map_or_else(|| "0.0.0.0:9300".to_owned(), |a| a.to_string());
        let mut store =
            kiseki_log::RaftShardStore::new(cfg.node_id, peers.clone(), cfg.data_dir.clone());
        if let Some(ref ss) = small_store {
            store = store.with_inline_store(std::sync::Arc::clone(ss)
                as std::sync::Arc<dyn kiseki_common::inline_store::InlineStore>);
        }
        // ADR-041 §"Observability": wire transport metrics BEFORE the
        // first create_shard so the lazy-init listener picks them up.
        store.set_transport_metrics(std::sync::Arc::clone(&metrics.raft_transport));

        // ADR-033 §4: bring up the multiplexed Raft listener BEFORE
        // any per-shard `create_shard` so the control-plane group can
        // share it. Returns the registry handle for the
        // control-plane group's `register_shard`.
        let registry = store.ensure_listener_started(&raft_addr_str);

        // All nodes in the cluster register the shard's per-shard
        // Raft handle with the multiplexed listener. Membership
        // initialization is a separate, explicit step (the
        // `initialize_shard` call below on the bootstrap node)
        // — see the ADR-033 §4 doc on `RaftShardStore::create_shard`.
        store.create_shard(
            bootstrap_shard,
            bootstrap_tenant,
            kiseki_common::ids::NodeId(cfg.node_id),
            kiseki_log::ShardConfig::default(),
            Some(&raft_addr_str),
        );
        // Bootstrap node initializes membership for the bootstrap
        // shard. Followers do not — they learn membership via
        // AppendEntries from the leader.
        if cfg.bootstrap {
            if let Err(e) = store.initialize_shard(bootstrap_shard) {
                tracing::warn!(
                    error = %e,
                    "bootstrap shard initialize_shard failed — \
                     cluster may need manual intervention",
                );
            }
        }
        tracing::info!(
            node_id = cfg.node_id,
            peers = cfg.raft_peers.len(),
            "log store: Raft",
        );

        let store_arc = Arc::new(store);

        // ADR-033 §4: control-plane Raft group. Built on the same
        // tokio runtime as the per-shard groups (mixing runtimes
        // deadlocks openraft). Apply hook bridges to RaftShardStore
        // so RecordSplit on every node creates the new shard's
        // per-shard Raft group locally — closes the cluster-wide
        // split gap surfaced by the @shard-mgmt BDD scenarios.
        let raft_rt = store_arc.raft_runtime_handle();
        let apply_hook: Arc<dyn crate::cluster_control::ApplyHook> = Arc::new(
            crate::cluster_control::ShardStoreApplyHook::new(Arc::clone(&store_arc), cfg.node_id),
        );
        let peers_for_ctrl = peers.clone();
        let bootstrap_flag = cfg.bootstrap;
        let data_dir_for_ctrl = cfg.data_dir.clone();
        // Construct on the dedicated Raft runtime (block_on inside a
        // spawned thread so we don't nest runtimes — the same pattern
        // RaftShardStore uses for create_shard).
        let registry_for_ctrl = registry.clone();
        let ctrl_metrics = Arc::clone(&metrics.cluster_control);
        let ctrl_store_res: Result<
            Arc<crate::cluster_control::OpenRaftControlStore>,
            std::io::Error,
        > = std::thread::spawn(move || {
            raft_rt.block_on(async move {
                crate::cluster_control::OpenRaftControlStore::new(
                    cfg.node_id,
                    &peers_for_ctrl,
                    data_dir_for_ctrl.as_deref(),
                    &registry_for_ctrl,
                    apply_hook,
                    bootstrap_flag,
                    Some(ctrl_metrics),
                )
                .await
                .map(Arc::new)
            })
        })
        .join()
        .map_err(|_| "control-plane raft thread panicked".to_owned())?;

        let ctrl_store = ctrl_store_res.map_err(|e| format!("control-plane raft init: {e}"))?;
        tracing::info!(
            node_id = cfg.node_id,
            peers = cfg.raft_peers.len(),
            "control-plane Raft group: up (ADR-033 §4)",
        );

        // Bootstrap node seeds the control-plane state machine with
        // the bootstrap namespace + bootstrap shard. Followers learn
        // it via Raft replication. The apply hook is idempotent on
        // already-existing shards, so the bootstrap shard's
        // per-shard Raft group (created above) is not re-built.
        //
        // Runs on the dedicated Raft runtime so the client_write
        // awaits don't try to drive the host runtime's reactor.
        // Background-only: don't block boot on the seed completing
        // (the leader may need an election first); the next admin
        // RPC will encounter a usable, seeded namespace map by the
        // time consensus closes.
        if cfg.bootstrap {
            let ctrl_for_seed = Arc::clone(&ctrl_store);
            let raft_rt_for_seed = store_arc.raft_runtime_handle();
            let bootstrap_ns = bootstrap_tenant.0.to_string();
            std::thread::spawn(move || {
                raft_rt_for_seed.block_on(async move {
                    let cmd = crate::cluster_control::ControlCommand::CreateNamespace {
                        namespace_id: bootstrap_ns,
                        tenant_id: bootstrap_tenant,
                        shards: vec![crate::cluster_control::commands::ShardRecord {
                            shard_id: bootstrap_shard,
                            range_start: [0u8; 32],
                            range_end: [0xFFu8; 32],
                            leader_node: kiseki_common::ids::NodeId(1),
                        }],
                    };
                    // Retry for up to 60s while leader election
                    // converges. The control-plane group's
                    // initialize() returns immediately but voters
                    // don't agree on a leader until the other
                    // nodes' control-plane groups come up — and
                    // the BDD harness brings nodes up serially,
                    // so node-1's first ~5s sees no peer.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                    let mut attempt = 0u32;
                    loop {
                        match ctrl_for_seed.submit(cmd.clone()).await {
                            Ok(_) => {
                                tracing::info!(
                                    attempts = attempt + 1,
                                    "control-plane: bootstrap namespace seeded",
                                );
                                return;
                            }
                            Err(e) if std::time::Instant::now() < deadline => {
                                attempt += 1;
                                if attempt % 10 == 0 {
                                    tracing::debug!(
                                        attempt,
                                        error = %e,
                                        "control-plane bootstrap seed: still retrying",
                                    );
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    attempts = attempt,
                                    "control-plane bootstrap CreateNamespace failed \
                                     after 60s — cluster will operate without \
                                     control-plane state",
                                );
                                return;
                            }
                        }
                    }
                });
            });
        }

        cluster_control_store = Some(ctrl_store);
        raft_shard_store_for_admin = Some(Arc::clone(&store_arc));

        store_arc
    } else if let Some(ref dir) = cfg.data_dir {
        std::fs::create_dir_all(dir.join("raft")).ok();
        // Match the composition + chunk store durability knobs:
        // when KISEKI_RAFT_FLUSH_INTERVAL_MS is set, the Raft log
        // commits with PersistMode::Buffer and a periodic task
        // drives the fsync barrier. Multi-node deployments recover
        // the loss window via Raft replication on restart; single-
        // node deployments accept the documented per-knob loss
        // window in exchange for the throughput lift (PUT path was
        // fsync-bound at ~31 k op/s with sync-per-write).
        let raft_flush_interval_ms = std::env::var("KISEKI_RAFT_FLUSH_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        let log_path = dir.join("raft").join("log");
        let store = if raft_flush_interval_ms.is_some() {
            kiseki_log::persistent_store::PersistentShardStore::open_eventual(&log_path).await
        } else {
            kiseki_log::persistent_store::PersistentShardStore::open(&log_path).await
        }
        .map_err(|e| format!("persistent store: {e}"))?;
        if cfg.bootstrap {
            store.create_shard(
                bootstrap_shard,
                bootstrap_tenant,
                kiseki_common::ids::NodeId(1),
                kiseki_log::ShardConfig::default(),
            );
        }
        if let Some(interval_ms) = raft_flush_interval_ms {
            // Periodic Raft-log fsync. Same shape as the
            // composition store flusher: cheap memtable + WAL append
            // inline, durability barrier at a bounded cadence.
            // Borrow a clone of the underlying FjallLogStore for
            // the flush task.
            let fjall = store.fjall().clone();
            tracing::info!(
                interval_ms,
                "log store: persistent fjall (eventual durability)"
            );
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    if let Err(e) = fjall.flush() {
                        tracing::warn!(error = %e, "raft log periodic flush failed");
                    }
                }
            });
        } else {
            tracing::info!(path = %dir.display(), "log store: persistent (fjall)");
        }
        Arc::new(store)
    } else {
        let store = kiseki_log::MemShardStore::new();
        if cfg.bootstrap {
            store.create_shard(
                bootstrap_shard,
                bootstrap_tenant,
                kiseki_common::ids::NodeId(1),
                kiseki_log::ShardConfig::default(),
            );
        }
        tracing::info!("log store: in-memory (no persistence)");
        Arc::new(store)
    };

    // Wrap in `InstrumentedLogOps` so every LogOps call records to
    // `LogMetrics` and emits a tracing span. The wrapper is opaque
    // to callers (still `Arc<dyn LogOps>`); inner store paths that
    // need concrete types (`RaftShardStore::initialize_shard` for
    // SplitShard's leader-side init) hold their own typed `Arc`.
    //
    // Skipped when `KISEKI_OBSERVABILITY=off` so perf runs measure
    // bare LogOps throughput. See the explanatory comment near the
    // key-store wrap above.
    let log_store: Arc<dyn kiseki_log::LogOps + Send + Sync> = if observability_enabled {
        Arc::new(kiseki_log::InstrumentedLogOps::new(
            log_store,
            Arc::clone(&metrics.log),
        ))
    } else {
        log_store
    };

    if cfg.bootstrap {
        tracing::info!(
            shard = %bootstrap_shard.0,
            tenant = %bootstrap_tenant.0,
            "bootstrap: shard created for tenant",
        );
    }

    // Audit: in-memory store.
    let audit_store = kiseki_audit::AuditLog::new();
    tracing::info!(events = audit_store.total_events(), "audit log: in-memory",);

    // Metrics — built early so the cluster-fabric Arc<FabricMetrics>
    // can be threaded into the per-peer client wrappers below.
    // (KisekiMetrics is also constructed before the log-store
    // creation above so `raft_transport` metrics can be wired into
    // the multiplexed listener BEFORE its first `create_shard` call.
    // See `metrics.raft_transport` plumbing in the multi-node path.)
    let _ = &metrics;

    // Captures the chunk-store device handle + composition fjall
    // flusher when their respective group-commit modes are on, so
    // the gateway's `fsync_pending` hook can drive explicit fsyncs
    // from FUSE / NFS `fsync(2)` callers (POSIX-compliance for
    // the eventual-durability optimization). Declared up here so
    // both the chunk-store section below and the composition-store
    // section further down can write into the same scope.
    let mut comp_flusher_for_fsync: Option<kiseki_composition::persistent::FjallFlusher> = None;
    let mut chunk_device_for_fsync: Option<std::sync::Arc<dyn kiseki_block::DeviceBackend>> = None;

    // Local chunk store: persistent (raw block device) if KISEKI_DATA_DIR
    // set, otherwise in-memory. Wrapped via SyncBridge so it satisfies
    // AsyncChunkOps — the cluster fabric and the gateway both consume the
    // async surface (Phase 16a, D-7).
    let local_chunk_store: Arc<dyn kiseki_chunk::AsyncChunkOps> = if let Some(ref dir) =
        cfg.data_dir
    {
        std::fs::create_dir_all(dir.join("chunks")).ok();
        let dev_path = dir.join("chunks").join("data.dev");
        // ADR-022 rev-4: chunk meta moved off JSON to fjall. Path
        // is now a keyspace directory (no extension).
        let meta_path = dir.join("chunks").join("meta");
        let store = if dev_path.exists() {
            kiseki_chunk::PersistentChunkStore::open(&dev_path, &meta_path)
                .map_err(|e| format!("persistent chunk store open: {e}"))?
        } else {
            kiseki_chunk::PersistentChunkStore::init(&dev_path, &meta_path, 4 * 1024 * 1024 * 1024)
                .map_err(|e| format!("persistent chunk store init: {e}"))?
        };
        // Receiver-side write_chunk phase histogram so /metrics shows
        // dedup_check / extent_io / save_meta / device_sync per write.
        store.set_write_phase_metric(Arc::new(
            metrics.chunk_persistent_write_phase_duration.clone(),
        ));
        // Group commit: per-write fsync was serializing concurrent
        // fabric receivers through the kernel sync. Disable inline
        // sync; the periodic flush task below keeps disk state fresh.
        // Crash safety: Raft replication ensures cross-node durability;
        // a single-node loss of ≤ flush_interval-ms of writes is
        // recovered by the under-replication scrub when the node
        // returns. See `PersistentChunkStore::sync_per_write` doc.
        store.set_sync_per_write(false);
        let device_for_flush = store.device_handle();
        // Capture for the gateway `fsync_pending` hook so explicit
        // `fsync(2)` from FUSE / NFS clients forces a real device
        // sync rather than waiting for the periodic task.
        chunk_device_for_fsync = Some(std::sync::Arc::clone(&device_for_flush));
        let flush_interval_ms = std::env::var("KISEKI_CHUNK_FLUSH_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(100);
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_millis(flush_interval_ms));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let device = std::sync::Arc::clone(&device_for_flush);
                let res = tokio::task::spawn_blocking(move || device.sync())
                    .await
                    .ok()
                    .and_then(Result::ok);
                if res.is_none() {
                    tracing::warn!("chunk store group-commit flush failed; will retry next tick",);
                }
            }
        });
        tracing::info!(
            path = %dir.display(),
            flush_interval_ms,
            "chunk store: persistent (raw block, group commit)",
        );
        Arc::new(kiseki_chunk::SyncBridge::new(store))
    } else {
        tracing::info!("chunk store: in-memory (no persistence)");
        Arc::new(kiseki_chunk::SyncBridge::new(
            kiseki_chunk::ChunkStore::new(),
        ))
    };

    // Cluster chunk fabric (Phase 16a step 12). For each *other* peer
    // in raft_peers we open a lazy mTLS gRPC Channel to its data-path
    // port and wrap it in GrpcFabricPeer. For a 1-node cluster peers
    // is empty and the store degenerates to local-only (D-6); the
    // existing single-node tests stay green by construction.
    //
    // The data-path port carries both the data services AND the
    // ClusterChunkService — peers reuse the same port. The SAN-role
    // interceptor (step 5) gates fabric methods to certs that carry
    // a `spiffe://cluster/fabric/<node-id>` SAN URI.
    let bootstrap_tenant_for_cluster = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));
    let mut fabric_peers: Vec<Arc<dyn kiseki_chunk_cluster::FabricPeer>> = Vec::new();
    let data_port = cfg.data_addr.port();
    // Build a peer-id → fabric address map. `KISEKI_FABRIC_PEERS`
    // (cfg.fabric_peers) overrides the per-port derivation below,
    // which is the only path that works when every node binds a
    // distinct data-path port (e.g. localhost multi-node, BDD).
    let fabric_override: std::collections::HashMap<u64, &str> = cfg
        .fabric_peers
        .iter()
        .map(|(id, addr)| (*id, addr.as_str()))
        .collect();
    for (peer_id, raft_peer_addr) in &cfg.raft_peers {
        if *peer_id == cfg.node_id {
            continue; // skip ourselves
        }
        let fabric_addr = fabric_override.get(peer_id).map_or_else(
            || fabric_addr_from_raft_peer(raft_peer_addr, data_port),
            |s| (*s).to_owned(),
        );
        match build_fabric_channel(&fabric_addr, cfg.tls.as_ref()) {
            Ok(channel) => {
                let name = format!("node-{peer_id}");
                fabric_peers.push(Arc::new(
                    kiseki_chunk_cluster::GrpcFabricPeer::new(name, channel)
                        .with_metrics(Arc::clone(&metrics.fabric)),
                ));
                tracing::info!(
                    peer_id,
                    fabric_addr,
                    "fabric peer registered for cross-node chunks",
                );
            }
            Err(e) => {
                tracing::warn!(
                    peer_id, fabric_addr, error = %e,
                    "fabric peer channel build failed — peer skipped (cluster may run degraded)",
                );
            }
        }
    }
    if !fabric_peers.is_empty() {
        tracing::info!(
            peers = fabric_peers.len(),
            "cross-node chunk replication enabled (Phase 16a)",
        );
    } else if cfg.raft_peers.len() > 1 {
        tracing::warn!(
            "cross-node fabric is empty despite raft_peers > 1 — cluster running in local-only mode",
        );
    }
    // Phase 16b step 3: pick durability defaults (copies + min_acks)
    // from the cluster size. 1-node → local-only (min_acks=1, won't
    // deadlock single-node tests). 2-node → Replication-2, both peers
    // ack. 3+ nodes → Replication-3 with 2-of-3 quorum.
    let cluster_size = cfg.raft_peers.len().max(1);
    let durability = kiseki_chunk_cluster::defaults_for(cluster_size);
    tracing::info!(
        cluster_size,
        copies = durability.copies,
        min_acks = durability.min_acks,
        strategy = ?durability.strategy,
        "cluster durability defaults",
    );
    // Phase 16e step 1: thread the per-cluster-size strategy into
    // ClusterCfg.ec_strategy so write_chunk / read_chunk dispatch
    // (16d steps 1+5) routes a 6+ node cluster through the EC
    // path, honoring I-C4 ("EC is the default") + I-D1 ("repaired
    // from EC parity").
    let cluster_nodes_for_cfg: Vec<u64> = cfg.raft_peers.iter().map(|(id, _)| *id).collect();
    let cluster_cfg =
        kiseki_chunk_cluster::ClusterCfg::new(bootstrap_tenant_for_cluster, "default")
            .with_min_acks(durability.min_acks)
            .with_ec_strategy(durability.strategy)
            .with_cluster_nodes(cluster_nodes_for_cfg)
            .with_self_node_id(cfg.node_id);
    // Phase 16d step 4: clone the peer list before it's moved into
    // ClusteredChunkStore so the scrub-scheduler adapters can build
    // a parallel by-id index for HasFragment + repair calls.
    let fabric_peers_for_scrub: Vec<Arc<dyn kiseki_chunk_cluster::FabricPeer>> =
        fabric_peers.iter().map(Arc::clone).collect();
    // GCP 2026-05-02 fix: share one envelope registry between the
    // local-node ClusterChunkServer (populated by incoming
    // PutFragment RPCs) and the local-node ClusteredChunkStore
    // (populated by the leader's own local-fragment writes). Without
    // this, peers fetching the leader's local fragment receive an
    // envelope with zero auth_tag/nonce — AES-GCM verify fails.
    let envelope_registry = kiseki_chunk_cluster::ChunkEnvelopeRegistry::default();
    let chunk_store: Arc<dyn kiseki_chunk::AsyncChunkOps> = Arc::new(
        kiseki_chunk_cluster::ClusteredChunkStore::new(
            Arc::clone(&local_chunk_store),
            fabric_peers,
            cluster_cfg,
        )
        .with_metrics(Arc::clone(&metrics.fabric))
        .with_envelope_registry(envelope_registry.clone()),
    );

    // Phase 16d step 4: spawn the periodic scrub scheduler when
    // running on a real cluster (>=1 peer; in single-node mode
    // there are no fragments to scrub against and no peers to
    // probe / repair from). Cadence is currently a fixed 10
    // minutes per shard — operators can revisit once the
    // scheduler ships per-shard metrics.
    let scrub_scheduler_handle: Option<Arc<kiseki_chunk_cluster::ScrubScheduler>> =
        if fabric_peers_for_scrub.is_empty() {
            None
        } else {
            let scrub_log = Arc::clone(&log_store) as Arc<dyn kiseki_log::traits::LogOps>;
            let scrub_local = Arc::clone(&local_chunk_store);
            let scrub_oracle: Arc<dyn kiseki_chunk_cluster::FragmentAvailabilityOracle> = Arc::new(
                kiseki_chunk_cluster::FabricAvailabilityOracle::new(&fabric_peers_for_scrub),
            );
            let scrub_deleter: Arc<dyn kiseki_chunk_cluster::OrphanDeleter> = Arc::new(
                kiseki_chunk_cluster::LocalChunkDeleter::new(Arc::clone(&local_chunk_store)),
            );
            let scrub_repairer: Arc<dyn kiseki_chunk_cluster::Repairer> =
                Arc::new(kiseki_chunk_cluster::FabricRepairer::new(
                    &fabric_peers_for_scrub,
                    bootstrap_tenant_for_cluster,
                    "default".into(),
                ));
            let scheduler = Arc::new(
                kiseki_chunk_cluster::ScrubScheduler::new(
                    scrub_log,
                    scrub_local,
                    scrub_oracle,
                    scrub_deleter,
                    scrub_repairer,
                    kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
                    bootstrap_tenant_for_cluster,
                    kiseki_chunk_cluster::OrphanScrubPolicy::default(),
                    kiseki_chunk_cluster::UnderReplicationPolicy {
                        target_copies: durability.copies,
                        min_acks: durability.min_acks,
                    },
                )
                // Phase 16e step 3: thread the EC strategy so the scrub
                // dispatches via repair_ec on EC clusters (≥6 nodes per
                // the defaults table). Replication-N stays on the legacy
                // repair() path via the trait default.
                .with_strategy(durability.strategy),
            );
            // Phase 16e step 4: build a shutdown channel + spawn the
            // scheduler with it. On Ctrl-C the data-path serve loop
            // exits via serve_with_shutdown; we send true on the
            // scrub channel so its loop drains cleanly + the
            // JoinHandle joins before the runtime shuts down. Today
            // the runtime doesn't have a shared shutdown signal hook,
            // so the scrub channel sender is leaked here — the
            // process exit terminates the loop. When the runtime
            // grows a unified shutdown registry this sender goes in
            // there.
            let (scrub_shutdown_tx, scrub_shutdown_rx) = tokio::sync::watch::channel(false);
            let scrub_handle = Arc::clone(&scheduler)
                .start_periodic(std::time::Duration::from_secs(600), scrub_shutdown_rx);
            // Detach: the channel sender + JoinHandle stay alive for
            // the process lifetime. Wiring a unified shutdown signal
            // is a runtime-wide concern tracked in
            // `specs/escalations/`.
            std::mem::drop((scrub_shutdown_tx, scrub_handle));
            tracing::info!(
                "scrub scheduler: spawned (orphan + under-replication, 10-min cadence, drain-on-shutdown)",
            );
            // ADR-025 W4: also hand the scheduler to
            // `StorageAdminGrpc::with_scrub` so the admin RPC can
            // call `trigger_now()` and `repair_one_chunk()`.
            Some(scheduler)
        };

    // Raw device discovery (KISEKI_RAW_DEVICES).
    // This is the discovery phase — actual device opening via DeviceBackend
    // is deferred until the RawBlockDevice implementation is wired.
    if !cfg.raw_devices.is_empty() {
        tracing::info!(
            devices = cfg.raw_devices.len(),
            "raw block devices configured"
        );
        for dev_path in &cfg.raw_devices {
            let path = std::path::Path::new(dev_path);
            if path.exists() {
                tracing::info!(device = dev_path, "raw device detected");
            } else {
                tracing::warn!(device = dev_path, "raw device not found — skipping");
            }
        }
    }

    // Composition: wired to log for delta emission. ADR-040 + ADR-022
    // successor: when KISEKI_DATA_DIR is set we back the comp_id →
    // Composition map with a fjall keyspace at
    // `<data_dir>/metadata/compositions/`, so hydrated state survives
    // restart and a node that joins late resumes from durable
    // `last_applied_seq`. Single-node / no-data-dir deployments keep
    // the in-memory backend (MemoryStorage) — same behavior as
    // pre-ADR-040.
    // Captures the keyspace directory so the periodic gauge refresher
    // can stat the on-disk footprint; None when the composition store
    // is in-memory.
    let mut comp_store_path: Option<std::path::PathBuf> = None;
    // `comp_flusher_for_fsync` declared at the runtime scope above
    // (before chunk-store construction) so both the chunk and
    // composition sections can populate it for the gateway
    // `fsync_pending` hook registration further down.
    let comp_storage: Box<dyn kiseki_composition::persistent::CompositionStorage> =
        if let Some(ref dir) = cfg.data_dir {
            let meta_dir = dir.join("metadata");
            std::fs::create_dir_all(&meta_dir).map_err(|e| {
                format!(
                    "create persistent composition dir {}: {e}",
                    meta_dir.display()
                )
            })?;
            let path = meta_dir.join("compositions");
            // Group commit (FUSE p99 fix): every put / bind_name
            // would otherwise trigger an inline fsync. fjall's
            // `PersistMode::Buffer` + a periodic flusher that calls
            // `PersistMode::SyncAll` gives the same crash-safety
            // contract as the previous redb write-behind queue at a
            // bounded loss-window equal to the flush interval.
            // `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS` enables eventual
            // durability; multi-node deployments are safe because
            // Raft re-replicates lost compositions via the under-
            // replication scrub on restart.
            let flush_interval_ms = std::env::var("KISEKI_COMPOSITION_FLUSH_INTERVAL_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok());
            let store = kiseki_composition::persistent::FjallStorage::open(&path)
                .map_err(|e| format!("open persistent composition store: {e}"))?;
            let store = if flush_interval_ms.is_some() {
                let store = store.with_eventual_durability(true);
                let flusher = store.flusher();
                // Hand the same flusher to the gateway via
                // `register_fsync_hook` further below, so explicit
                // `fsync(2)` from FUSE / NFS clients forces a real
                // fsync rather than waiting up to `interval_ms` for
                // the periodic task.
                comp_flusher_for_fsync = Some(flusher.clone());
                let interval_ms = flush_interval_ms.unwrap_or(100);
                // Periodic WAL fsync. The LSM memtable + WAL append
                // happens inline on every write (cheap, in-memory);
                // this task drives the durability barrier at a bounded
                // cadence. Same contract as the previous redb
                // write-behind drainer's flush loop.
                tokio::spawn(async move {
                    let mut tick =
                        tokio::time::interval(std::time::Duration::from_millis(interval_ms));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        if let Err(e) = flusher.flush() {
                            tracing::warn!(error=%e, "composition store periodic fsync failed");
                        }
                    }
                });
                tracing::info!(
                    path = %path.display(),
                    interval_ms,
                    "composition store: persistent (fjall, eventual durability)",
                );
                store
            } else {
                tracing::info!(
                    path = %path.display(),
                    "composition store: persistent (fjall, immediate fsync)",
                );
                store
            };
            comp_store_path = Some(path);
            Box::new(store)
        } else {
            tracing::info!("composition store: in-memory (no KISEKI_DATA_DIR)");
            Box::new(kiseki_composition::persistent::MemoryStorage::new())
        };
    let comp_store = kiseki_composition::composition::CompositionStore::with_storage(comp_storage)
        .with_log(Arc::clone(&log_store) as Arc<dyn kiseki_log::LogOps + Send + Sync>);

    // View: shared between gateway (staleness check) and stream
    // processor. With KISEKI_DATA_DIR set, persist views via the
    // ADR-040 sibling so descriptors + watermarks survive restart;
    // otherwise fall back to in-memory.
    let view_storage: Box<dyn kiseki_view::persistent::ViewStorage> =
        if let Some(ref dir) = cfg.data_dir {
            let meta_dir = dir.join("metadata");
            std::fs::create_dir_all(&meta_dir)
                .map_err(|e| format!("create persistent view dir {}: {e}", meta_dir.display()))?;
            let path = meta_dir.join("views.redb");
            let store = kiseki_view::persistent::PersistentRedbStorage::open(&path)
                .map_err(|e| format!("open persistent view store: {e}"))?;
            tracing::info!(path = %path.display(), "view store: persistent (redb-backed, ADR-040)");
            Box::new(store)
        } else {
            tracing::info!("view store: in-memory (no KISEKI_DATA_DIR)");
            Box::new(kiseki_view::persistent::MemoryStorage::new())
        };
    // parking_lot::RwLock — read-mostly. The mem_gateway read path
    // takes a read lock to check view staleness on every gateway
    // read; std::sync::Mutex was 9.78% of CPU at 64 KiB GET on the
    // perf flame because all c=16 readers serialized through it.
    // Stream processor (advances watermarks every 100 ms) takes a
    // write lock briefly.
    let view_store = Arc::new(parking_lot::RwLock::new(
        kiseki_view::view::ViewStore::with_storage(view_storage),
    ));

    // Bootstrap namespace + view for protocol gateways. The IDs are
    // deterministic (UUID-from-u128(1) for shard/view, UUIDv5 of
    // "default" for the namespace), and the records are pure
    // convention — a multi-node cluster's followers need them
    // installed locally so the Phase 16f composition hydrator can
    // resolve their `namespace_id` field. Creating them on every node
    // is idempotent. The Raft-specific seeding (initialize the group
    // vs. join as a follower) is the only thing gated on
    // `cfg.bootstrap`.
    let bootstrap_tenant = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));
    let bootstrap_ns =
        kiseki_common::ids::NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default"));
    let bootstrap_view = kiseki_common::ids::ViewId(uuid::Uuid::from_u128(1));
    comp_store.add_namespace(kiseki_composition::namespace::Namespace {
        id: bootstrap_ns,
        tenant_id: bootstrap_tenant,
        shard_id: bootstrap_shard,
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
    });
    let _ = view_store.write().create_view(kiseki_view::ViewDescriptor {
        view_id: bootstrap_view,
        tenant_id: bootstrap_tenant,
        source_shards: vec![bootstrap_shard],
        protocol: kiseki_view::ProtocolSemantics::Posix,
        consistency: kiseki_view::ConsistencyModel::ReadYourWrites,
        discardable: true,
        version: 1,
    });
    if cfg.bootstrap {
        tracing::info!("bootstrap: namespace 'default' + view installed (Raft seed node)");
    } else {
        tracing::info!(
            "bootstrap: namespace 'default' + view installed (Raft follower; will hydrate compositions from log)",
        );
    }

    // Shared gateway: wires composition + chunk + crypto. Used by S3 and NFS.
    let master_key =
        kiseki_crypto::keys::SystemMasterKey::new([0x42; 32], kiseki_common::tenancy::KeyEpoch(1));
    // ADR-042 §9.1: derive the native-service signing keys (handle
    // tokens, DEK fetch tickets, multipart upload IDs) from the master
    // key BEFORE moving it into the gateway. The grace window is
    // tunable via KISEKI_MASTER_KEY_ROTATION_GRACE_MS (default 5 min).
    let native_grace_ms: u64 = std::env::var("KISEKI_MASTER_KEY_ROTATION_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300_000);
    let native_signing_keys = std::sync::Arc::new(
        kiseki_gateway::native::signing_keys::SigningKeys::new(&master_key, native_grace_ms),
    );
    // Phase 16b step 2: pass the cluster's node-id list as the
    // placement for every fresh chunk. In a 1-node cluster this is
    // empty (the gateway carries vec![] in NewChunkMeta), matching
    // the single-node-degenerate path.
    let cluster_placement: Vec<u64> = cfg.raft_peers.iter().map(|(id, _)| *id).collect();
    let mut gw_builder = kiseki_gateway::InMemoryGateway::new(comp_store, chunk_store, master_key)
        .with_view_store(Arc::clone(&view_store))
        .with_cluster_placement(cluster_placement)
        // Phase 16c step 2: cap per-chunk placement at the
        // size-derived `copies` so a 6-node Replication-3 cluster
        // doesn't list all 6 nodes in every cluster_chunk_state row.
        .with_target_copies(usize::from(durability.copies))
        // ADR-040 §D7 + §D10 / F-4 closure: thread the read-path
        // retry counters (`kiseki_gateway_read_retry_total` and
        // `kiseki_gateway_read_retry_exhausted_total`) into the
        // gateway so operators can see whether they're hitting
        // the configurable budget.
        .with_retry_metrics(Arc::clone(&metrics.gateway_retry));
    // The inline path (mem_gateway.rs PUT path: writes ≤ inline_threshold
    // go to local small_store keyed by chunk_id) is single-node-only. In a
    // multi-node cluster the inline write lands on one node's redb and the
    // Raft-replicated composition metadata leads other nodes to look up
    // chunk_ids that aren't in their small_store → cross-node GET returns
    // 404. ADR-026 sketches a "small writes inline in delta → Raft only"
    // optimization keyed by hashed_key XOR seq, but mem_gateway and the
    // Raft state-machine apply path use incompatible key spaces, so until
    // that path is unified we route ALL writes through the chunk/fabric
    // path when fabric peers are present. Single-node clusters keep the
    // inline optimization.
    let multi_node = !fabric_peers_for_scrub.is_empty();
    if let Some(ref ss) = small_store {
        if multi_node {
            tracing::info!(
                "inline write path disabled in multi-node cluster — small writes go through fabric (Phase 16a)",
            );
        } else {
            gw_builder = gw_builder.with_inline_threshold(
                kiseki_log::ShardConfig::default().inline_threshold_bytes,
                std::sync::Arc::clone(ss)
                    as std::sync::Arc<dyn kiseki_common::inline_store::InlineStore>,
            );
        }
    }
    let gw = Arc::new(gw_builder);
    // Wire the shared workflow table + Prometheus counter so the
    // gateway's `x-kiseki-workflow-ref` header validation
    // (mem_gateway::write) is fully observable end-to-end. Without
    // this the gateway's atomic counters tick but `/metrics` shows
    // zero — and the BDD harness has nothing to assert on.
    gw.set_workflow_table(workflow_table.clone());

    // FUSE / NFS `fsync(2)` correctness under the eventual-durability
    // optimization. Each registered hook drives a real fsync on its
    // backing store. Without these, callers that explicitly issue
    // `fsync(2)` would silently get the periodic-task SLA (≤100 ms
    // window) instead of POSIX's "data is on disk now" guarantee.
    if let Some(flusher) = comp_flusher_for_fsync {
        // fjall's LSM memtable + WAL append happens inline on every
        // write; the hook drives a `PersistMode::SyncAll` so the
        // POSIX `fsync(2)` contract is honored (no extra "drain
        // overlay first" stage — fjall has no separate overlay).
        gw.register_fsync_hook(std::sync::Arc::new(move || {
            flusher.flush().map_err(|e| {
                kiseki_gateway::error::GatewayError::Upstream(format!(
                    "composition store fsync: {e}"
                ))
            })
        }));
        tracing::info!("fsync hook: composition store (fjall) registered");
    }
    if let Some(device) = chunk_device_for_fsync {
        gw.register_fsync_hook(std::sync::Arc::new(move || {
            device.sync().map_err(|e| {
                kiseki_gateway::error::GatewayError::Upstream(format!("chunk fsync: {e}"))
            })
        }));
        tracing::info!("fsync hook: chunk-store device registered");
    }

    gw.set_workflow_ref_writes_metric(Arc::new(metrics.gateway_workflow_ref_writes_total.clone()));
    // Mirror the gateway's atomic byte counters into the registered
    // Prometheus counters so `/metrics` scrapes show live throughput.
    // The GCP 2026-05-02 perf cluster reported these as 0 after
    // 1 GB of writes — i.e. the wiring was missing. Asserted by the
    // protocol-gateway BDD "operational metrics smoke" scenario.
    gw.set_chunk_byte_metrics(
        Arc::new(metrics.chunk_write_bytes.clone()),
        Arc::new(metrics.chunk_read_bytes.clone()),
    );
    gw.set_phase_duration_metrics(
        Arc::new(metrics.gateway_get_phase_duration.clone()),
        Arc::new(metrics.gateway_put_phase_duration.clone()),
    );

    // S3 gateway.
    let s3_gw = kiseki_gateway::s3::S3Gateway::new(Arc::clone(&gw));
    let s3_router = kiseki_gateway::s3_server::s3_router_full(
        s3_gw,
        bootstrap_tenant,
        kiseki_gateway::s3_auth::AccessKeyStore::new(),
        Some(Arc::new(metrics.gateway_requests_total.clone())),
        Some(Arc::new(metrics.gateway_request_duration.clone())),
    );
    let s3_addr = cfg.s3_addr;
    let s3_tls = cfg.tls.as_ref().and_then(|files| {
        let ca = std::fs::read(&files.ca_path).ok()?;
        let cert = std::fs::read(&files.cert_path).ok()?;
        let key = std::fs::read(&files.key_path).ok()?;
        kiseki_transport::TlsConfig::server_config(&ca, &cert, &key)
            .map(Arc::new)
            .ok()
    });
    tokio::spawn(async move {
        kiseki_gateway::s3_server::run_s3_server(s3_addr, s3_router, s3_tls).await;
    });

    // Prometheus metrics + admin UI server. The KisekiMetrics
    // registry was built earlier so the cluster fabric could plug
    // its FabricMetrics in. Reuse it here.
    let metrics_addr = cfg.metrics_addr;
    // Collect peer metrics addresses for the admin UI aggregator.
    let peer_metrics_addrs: Vec<String> = cfg
        .raft_peers
        .iter()
        .map(|(_, addr)| {
            // Raft peer addr is host:raft_port. Metrics is on the metrics port.
            // For now, assume peers use the same metrics port as this node.
            let host = addr.split(':').next().unwrap_or("127.0.0.1");
            format!("{host}:{}", metrics_addr.port())
        })
        .collect();
    let node_info = crate::web::api::NodeInfo {
        node_id: cfg.node_id,
        s3_addr: cfg.s3_addr.to_string(),
        nfs_addr: cfg.nfs_addr.to_string(),
        metrics_addr: cfg.metrics_addr.to_string(),
        raft_peers: cfg.raft_peers.clone(),
    };
    let metrics_log_store = Arc::clone(&log_store) as Arc<dyn kiseki_log::LogOps + Send + Sync>;
    let metrics_compositions = Some(gw.compositions_handle());
    let metrics_local_chunk_store = Some(Arc::clone(&local_chunk_store));
    // Pre-clone the §D10 composition metrics handle: the hydrator + the
    // periodic redb-size refresher (spawned later) both need it after
    // `metrics` is moved into the metrics-server task.
    let composition_metrics_for_hydrator = Arc::clone(&metrics.composition);
    let composition_metrics_for_size_refresh = Arc::clone(&metrics.composition);
    // Pre-clone the ADR-025 admin RPC counter; storage_admin_handler
    // is constructed below after `metrics` moves into the spawn.
    let storage_admin_calls_counter = Arc::new(metrics.storage_admin_calls_total.clone());
    // Pre-clone the fabric metrics for the ClusterChunkServer wired
    // further down; same reason — `metrics` moves into the spawn.
    let cluster_chunk_server_fabric = Arc::clone(&metrics.fabric);
    // Pre-clone the cluster-control metrics for `StorageAdminGrpc`'s
    // forwarding paths and `OpenRaftControlStore::with_metrics()`.
    let cluster_control_metrics_for_admin = Arc::clone(&metrics.cluster_control);
    tokio::spawn(async move {
        if let Err(e) = crate::metrics::run_metrics_server(
            metrics_addr,
            metrics,
            peer_metrics_addrs,
            Some(metrics_log_store),
            node_info,
            metrics_compositions,
            metrics_local_chunk_store,
        )
        .await
        {
            tracing::error!(error = %e, "metrics server error");
        }
    });

    // NFS gateway (NFSv3 + NFSv4.2 + pNFS on port 2049).
    //
    // ADR-038 §D4 transport gate: TLS by default, audited plaintext
    // fallback only with both flags set. Gate runs before any listener
    // binds so the server refuses to start cleanly on misconfiguration.
    let env_insecure_nfs =
        std::env::var("KISEKI_INSECURE_NFS").is_ok_and(|v| v == "true" || v == "1");
    let security = kiseki_gateway::nfs_security::evaluate(
        cfg.allow_plaintext_nfs,
        env_insecure_nfs,
        cfg.tls.is_some(),
        cfg.pnfs.layout_ttl_seconds,
        1, // bootstrap_tenant on this listener — single-tenant default
    )
    .map_err(|e| format!("NFS security gate refused start: {e}"))?;

    if security.emit_warn_banner {
        tracing::warn!(target: "kiseki::nfs::security", "{}", kiseki_gateway::nfs_security::PLAINTEXT_WARN_BANNER);
    }
    if let Some(audit_type) = security.audit_event {
        use kiseki_audit::event::AuditEvent;
        use kiseki_audit::store::AuditOps;
        use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
        audit_store.append(AuditEvent {
            sequence: kiseki_common::ids::SequenceNumber(0),
            timestamp: DeltaTimestamp {
                hlc: HybridLogicalClock {
                    physical_ms: now_ms,
                    logical: 0,
                    node_id: kiseki_common::ids::NodeId(cfg.node_id),
                },
                wall: WallTime {
                    millis_since_epoch: now_ms,
                    timezone: "UTC".into(),
                },
                quality: ClockQuality::Ntp,
            },
            event_type: audit_type,
            tenant_id: None,
            actor: "kiseki-server".to_string(),
            description: "plaintext NFS fallback active per ADR-038 §D4.2 — \
                operator opted in via [security].allow_plaintext_nfs=true \
                AND KISEKI_INSECURE_NFS=true"
                .to_string(),
        });
    }
    tracing::info!(
        mode = ?security.mode,
        layout_ttl_seconds = security.effective_layout_ttl_seconds,
        "NFS transport posture",
    );

    let nfs_tls = match security.mode {
        kiseki_gateway::nfs_security::NfsTransport::Tls => cfg.tls.as_ref().and_then(|files| {
            let ca = std::fs::read(&files.ca_path).ok()?;
            let cert = std::fs::read(&files.cert_path).ok()?;
            let key = std::fs::read(&files.key_path).ok()?;
            kiseki_transport::TlsConfig::server_config(&ca, &cert, &key)
                .map(Arc::new)
                .ok()
        }),
        kiseki_gateway::nfs_security::NfsTransport::Plaintext => None,
    };

    let nfs_gw = kiseki_gateway::nfs::NfsGateway::new(Arc::clone(&gw));
    let nfs_addr = cfg.nfs_addr;

    // Phase 15c.4 — construct the shared MdsLayoutManager BEFORE
    // either listener so NFS (MDS) and DS see the same instance and
    // the same fh4 MAC key. The manager governs:
    //   * Layout TTL + LRU eviction (§D9)
    //   * fh4 MAC validation between MDS-issued and DS-presented
    //     layouts (ADR-038 §D4.1)
    //   * The recall log that DS subprotocol consults
    //
    // DS endpoints are derived from raft peers (host portion) +
    // ds_addr's port — e.g. raft peer "kiseki-node1:9300" + ds_addr
    // ":2052" → "kiseki-node1:2052". This is what the kernel pNFS
    // client connects to after GETDEVICEINFO.
    let pnfs_layout_mgr: Option<Arc<kiseki_gateway::pnfs::MdsLayoutManager>> = if cfg.pnfs_enabled {
        let cluster_id_bytes: [u8; 16] = bootstrap_tenant.0.into_bytes();
        let mac_key = kiseki_gateway::pnfs::derive_pnfs_fh_mac_key(
            &[0x42; 32], // TODO Phase 15b: pull from kiseki_keymanager
            &cluster_id_bytes,
        );
        let storage_ds_addrs =
            compute_storage_ds_addrs(&cfg.ds_peers, &cfg.raft_peers, cfg.ds_addr);
        let mgr_cfg = kiseki_gateway::pnfs::MdsLayoutConfig {
            stripe_size_bytes: cfg.pnfs.stripe_size_bytes,
            layout_ttl_ms: cfg.pnfs.layout_ttl_seconds.saturating_mul(1000),
            max_entries: cfg.pnfs.layout_cache_max_entries,
            storage_ds_addrs,
            max_stripes_per_layout: cfg.pnfs.max_stripes_per_layout,
        };
        Some(Arc::new(kiseki_gateway::pnfs::MdsLayoutManager::new(
            mac_key, mgr_cfg,
        )))
    } else {
        None
    };

    // Storage nodes for the legacy LayoutManager fallback path. With
    // pnfs_layout_mgr wired (Phase 15c.4), op_layoutget routes via the
    // production manager; this list is unused but kept for back-compat
    // with the test harness that doesn't set the manager.
    let nfs_storage_nodes: Vec<String> = cfg
        .raft_peers
        .iter()
        .map(|(_, addr)| {
            let host = addr.split(':').next().unwrap_or(addr);
            format!("{host}:2052")
        })
        .collect();
    let nfs_listener =
        std::net::TcpListener::bind(nfs_addr).map_err(|e| format!("NFS bind {nfs_addr}: {e}"))?;
    let nfs_tls_for_thread = nfs_tls.clone();
    let pnfs_layout_mgr_for_nfs = pnfs_layout_mgr.clone();
    // Spawn the NFS listener as a tokio task on the main runtime —
    // post-async-native conversion the listener uses tokio::net and
    // each connection becomes a tokio task. No more std::thread::spawn,
    // no more block_on into a dedicated NFS runtime.
    tokio::spawn(async move {
        kiseki_gateway::nfs_server::serve_nfs_listener_with_mgr(
            nfs_listener,
            nfs_gw,
            bootstrap_tenant,
            bootstrap_ns,
            nfs_storage_nodes,
            pnfs_layout_mgr_for_nfs,
            None,
            nfs_tls_for_thread,
        )
        .await;
    });

    // Bug 10 fix: minimal portmapper / RPCBIND listener (RFC 1057).
    // Without this, unmodified Linux `mount -t nfs -o vers=3` clients
    // fail with "Connection refused" before any NFS RPC because the
    // kernel client first hits portmapper on TCP/111 to discover the
    // NFS / MOUNT port. We always advertise `nfs_addr.port()` for both
    // NFS3 and MOUNT3 since both are dispatched off the same NFS
    // listener by program number.
    if let Some(portmap_addr) = cfg.portmap_addr {
        match std::net::TcpListener::bind(portmap_addr) {
            Ok(listener) => {
                let listener = std::sync::Arc::new(listener);
                let advertised_port = nfs_addr.port();
                tracing::info!(
                    addr = %portmap_addr,
                    advertised_nfs_port = advertised_port,
                    "portmapper listening (NFS3 + MOUNT3 → NFS port)",
                );
                std::thread::spawn(move || {
                    kiseki_gateway::portmap::serve_portmap_listener(&listener, advertised_port);
                });
            }
            Err(e) => {
                // Don't fail server startup if 111 is taken by a host
                // rpcbind or unavailable; log and continue. NFSv4 still
                // works without portmapper.
                tracing::warn!(
                    addr = %portmap_addr,
                    error = %e,
                    "portmapper bind failed — NFSv3 mounts will need explicit mountport=",
                );
            }
        }
    }

    // pNFS Data Server listener (ADR-038 §D2). Only spawned when pNFS
    // is enabled AND `ds_addr` is configured. Shares the same
    // MdsLayoutManager instance as the NFS dispatcher above so DS
    // reads can validate fh4 stamps + honor recalls.
    if cfg.pnfs_enabled {
        if let Some(ds_addr) = cfg.ds_addr {
            let mac_key = pnfs_layout_mgr.as_ref().map_or_else(
                || {
                    let cluster_id_bytes: [u8; 16] = bootstrap_tenant.0.into_bytes();
                    kiseki_gateway::pnfs::derive_pnfs_fh_mac_key(&[0x42; 32], &cluster_id_bytes)
                },
                |m| m.current_mac_key(),
            );
            let ds_ctx = Arc::new(kiseki_gateway::pnfs_ds_server::DsContext {
                gateway: Arc::clone(&gw),
                mac_key,
                stripe_size_bytes: cfg.pnfs.stripe_size_bytes,
                rt: tokio::runtime::Handle::current(),
                now_ms: Arc::new(kiseki_gateway::pnfs_ds_server::default_now_ms),
                mds_layout_manager: pnfs_layout_mgr.clone(),
                write_buffers: Arc::new(kiseki_gateway::pnfs_write_buffer::DsWriteBuffers::new()),
            });
            let ds_tls_for_thread = nfs_tls.clone();
            tokio::spawn(async move {
                kiseki_gateway::pnfs_ds_server::run_ds_server(
                    ds_addr,
                    ds_ctx,
                    None,
                    ds_tls_for_thread,
                )
                .await;
            });
            tracing::info!(addr = %ds_addr, "pNFS DS listener spawned");
        }
    }

    // Stream processor: polls deltas from log → advances view watermarks.
    // Uses block_in_place to hold the std::sync::MutexGuard (not Send)
    // while awaiting the async poll(). This is safe because the spawned
    // task runs on a multi-thread runtime with block_in_place support.
    let sp_log = Arc::clone(&log_store);
    let sp_views = Arc::clone(&view_store);
    let sp_view_id = kiseki_common::ids::ViewId(uuid::Uuid::from_u128(1));
    let sp_rt = tokio::runtime::Handle::current();
    tokio::spawn(async move {
        loop {
            tokio::task::block_in_place(|| {
                let mut vs = sp_views.write();
                let mut sp = kiseki_view::stream_processor::TrackedStreamProcessor::new(
                    sp_log.as_ref(),
                    &mut *vs,
                );
                sp.track(sp_view_id);
                sp_rt.block_on(
                    sp.poll(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                    ),
                );
            });
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

    // Phase 16f: composition hydrator — followers reconstruct their
    // CompositionStore from the Raft-replicated delta log so cross-node
    // GETs resolve. Sibling of the view stream processor above; both
    // consume the same delta stream with non-overlapping responsibilities
    // (views: watermarks, compositions: id→metadata).
    if multi_node {
        let hyd_log = Arc::clone(&log_store);
        let hyd_compositions = gw.compositions_handle();
        let hyd_shard = bootstrap_shard;
        let hyd_metrics = composition_metrics_for_hydrator;
        tokio::spawn(async move {
            let mut hydrator = kiseki_composition::CompositionHydrator::new(hyd_compositions)
                .with_metrics(hyd_metrics);
            loop {
                let _applied = hydrator.poll(hyd_log.as_ref(), hyd_shard).await;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
        tracing::info!(
            "composition hydrator spawned (Phase 16f — followers consume create-deltas)",
        );
    }

    // §D10 — periodic stat of the composition store directory so the
    // `kiseki_composition_store_size_bytes` gauge tracks on-disk
    // growth. fjall is a keyspace = directory of LSM segments + WAL,
    // so we recurse one level (cheap — a few dozen files).
    // Only spawned when the persistent store is active. Also refreshes
    // `kiseki_composition_count` from the live store (cheap — backend
    // `count()` does one len call against the comps partition, no
    // full scan).
    if let Some(path) = comp_store_path {
        let size_metrics = composition_metrics_for_size_refresh;
        let count_compositions = gw.compositions_handle();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let total = dir_size_recursive(&path);
                size_metrics
                    .store_size_bytes
                    .set(i64::try_from(total).unwrap_or(i64::MAX));
                if let Ok(c) = count_compositions.with_storage_locked(|s| s.count()) {
                    size_metrics.count.set(i64::try_from(c).unwrap_or(i64::MAX));
                }
            }
        });
    }

    // TODO: Wire rotation_monitor::run_rotation_monitor() here.
    // The components exist (kiseki_keymanager::rotation_monitor + rewrap_worker)
    // but need a real KeyManagerOps reference from the Raft key store.
    // Current key manager is bootstrapped with a fixed key; production
    // requires the Raft-backed OpenRaftKeyStore for distributed rotation.

    // Periodic device scrub (P4c): bitmap vs redb consistency check.
    // Runs every 60 seconds when persistent chunk store is active.
    if cfg.data_dir.is_some() {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                // Scrub runs on the block device layer; report logged if issues found.
                // The actual scrub is performed by DeviceBackend::scrub() which
                // checks bitmap integrity and detects orphan extents.
                tracing::info!("scrub: periodic check completed");
            }
        });
    }

    // Backup manager (ADR-016). Stays None when KISEKI_BACKUP_BACKEND is
    // unset — the admin gRPC service will surface that as "disabled".
    if let Some(ref bcfg) = cfg.backup {
        match crate::backup::init_runtime_backup_manager(bcfg) {
            Ok(_) => tracing::info!(
                retention_days = bcfg.retention_days,
                include_data = bcfg.include_data,
                cleanup_interval_secs = bcfg.cleanup_interval_secs,
                "backup: enabled",
            ),
            Err(e) => tracing::warn!(error = %e, "backup: init failed — backups disabled"),
        }
    } else {
        tracing::info!("backup: disabled (set KISEKI_BACKUP_BACKEND=fs|s3 to enable)");
    }

    // --- gRPC services ---

    // Control plane (ADR-027: Rust-only).
    let control_tenants = Arc::new(TenantStore::new());
    let control_svc = ControlServiceServer::new(ControlGrpc::new(control_tenants));
    tracing::info!("control plane: in-process (ControlService on data-path gRPC)");

    let key_svc = KeyManagerServiceServer::new(KeyManagerGrpc::new(key_store));
    // ADR-025 W5: pre-clone the log store handle for the admin
    // service (`SplitShard` / `MergeShards`) before LogGrpc takes
    // ownership.
    let log_for_admin: Arc<dyn kiseki_log::traits::LogOps + Send + Sync> = Arc::clone(&log_store);
    let log_svc = LogServiceServer::new(LogGrpc::new(log_store));
    let admin_svc = kiseki_proto::v1::admin_service_server::AdminServiceServer::new(
        crate::admin_grpc::AdminGrpc::from_runtime(),
    );
    // ADR-025 W2: read-only RPCs (ListPools / GetPool / ListDevices /
    // GetDevice / ClusterStatus / PoolStatus / ListShards / GetShard /
    // ListRepairs) are wired through to the live runtime state.
    // Mutating RPCs still return UNIMPLEMENTED until W3-W7.
    let cluster_member_ids: Vec<u64> = cfg.raft_peers.iter().map(|(id, _)| *id).collect();
    // RepairTracker is constructed empty; the scrub-scheduler /
    // RepairChunk paths (W4) will start writing to it. Keeping the
    // shared handle here means the admin endpoint always returns a
    // stable empty list rather than UNIMPLEMENTED while we wire those.
    let repair_tracker = Arc::new(kiseki_chunk_cluster::repair_tracker::RepairTracker::new());
    // ADR-025 W3: cluster-wide tuning store. Persistent (redb)
    // when KISEKI_DATA_DIR is set, in-memory otherwise. Loaded
    // values rehydrate on boot — server defaults apply only on
    // first install or when persistence is unavailable.
    let tuning_store = if let Some(ref dir) = cfg.data_dir {
        let p = Arc::new(
            crate::tuning::RedbTuningPersistence::open(&dir.join("tuning"))
                .map_err(|e| format!("tuning store open: {e}"))?,
        );
        tracing::info!(path = %dir.join("tuning").display(), "tuning store: persistent (redb)");
        crate::tuning::TuningStore::with_persistence(p)
    } else {
        tracing::info!("tuning store: in-memory (no persistence)");
        crate::tuning::TuningStore::in_memory()
    };
    // ADR-025 W3 live-hook: tuning-change observer. W4/W5 will add
    // per-subsystem subscribers (compaction throttle, scrub
    // scheduler, raft snapshot interval). For W3 we land the wiring
    // skeleton — a single `tracing` subscriber so operators see
    // every SetTuningParams in the audit log even before subsystem
    // hooks are connected. This also keeps `subscribe()` exercised
    // in production so future hooks have a known-working channel.
    {
        let mut rx = tuning_store.subscribe();
        tokio::spawn(async move {
            // Skip the initial value; only log changes.
            let _ = rx.borrow_and_update();
            while rx.changed().await.is_ok() {
                let p = *rx.borrow();
                tracing::info!(
                    compaction_rate_mb_s = p.compaction_rate_mb_s,
                    gc_interval_s = p.gc_interval_s,
                    rebalance_rate_mb_s = p.rebalance_rate_mb_s,
                    scrub_interval_h = p.scrub_interval_h,
                    max_concurrent_repairs = p.max_concurrent_repairs,
                    stream_proc_poll_ms = p.stream_proc_poll_ms,
                    inline_threshold_bytes = p.inline_threshold_bytes,
                    raft_snapshot_interval = p.raft_snapshot_interval,
                    "tuning: SetTuningParams applied"
                );
            }
        });
    }
    // ADR-025 W4 deps. Same `Arc<MaintenanceMode>` is wired into
    // both the storage admin handler (flips the flag) and the
    // ClusterChunkServer (consults it on every PutFragment).
    // EvacuationRegistry is admin-only today; W5's EvacuateDevice
    // will be the producer.
    let maintenance_mode = Arc::new(kiseki_chunk_cluster::maintenance::MaintenanceMode::new());
    let evacuation_registry = Arc::new(kiseki_chunk::evacuation::EvacuationRegistry::new());
    // ADR-025 W5 deps: pool overrides (thresholds + rebalance
    // tracker) and the log-store handle for SplitShard/MergeShards.
    let pool_mutations = crate::pool_overrides::PoolMutationDeps::new();
    // ADR-025 W7: event broker channels for the streaming RPCs.
    // Same handles will be wired into chunk subsystem (DeviceHealth
    // producer) and chunk-cluster (IOStats sampler) as those
    // producers land. Today the channels exist + the admin RPCs
    // can subscribe; events arrive once producers wire up.
    let event_streams = crate::event_streams::EventStreams::new();
    let mut storage_admin_handler = crate::storage_admin::StorageAdminGrpc::from_runtime()
        .with_chunk_store(Arc::clone(&local_chunk_store))
        .with_cluster(cluster_member_ids, cfg.node_id)
        .with_bootstrap_shard(bootstrap_shard)
        .with_repair_tracker(Arc::clone(&repair_tracker))
        .with_tuning_store(tuning_store)
        .with_maintenance(Arc::clone(&maintenance_mode))
        .with_evacuations(Arc::clone(&evacuation_registry))
        .with_pool_mutations(pool_mutations)
        .with_log_store(log_for_admin)
        .with_event_streams(event_streams)
        .with_metrics(Arc::clone(&storage_admin_calls_counter));
    if let (Some(ctrl), Some(raft)) = (
        cluster_control_store.as_ref(),
        raft_shard_store_for_admin.as_ref(),
    ) {
        // ADR-033 §4: route SplitShard / MergeShards through the
        // control-plane Raft group so every node creates the new
        // per-shard Raft group locally on apply. Pass the typed
        // RaftShardStore handle so `initialize_shard` is callable
        // after the control-plane RecordSplit commits.
        // `cfg.fabric_peers` doubles as the admin-RPC peer list
        // because `StorageAdminService` is mounted on the data
        // port — forwarding non-leader admin calls to the leader
        // uses the same address.
        storage_admin_handler = storage_admin_handler.with_cluster_control(
            Arc::clone(ctrl),
            Arc::clone(raft),
            bootstrap_tenant.0.to_string(),
            cfg.fabric_peers.clone(),
            Arc::clone(&cluster_control_metrics_for_admin),
        );
    }
    if let Some(ref s) = scrub_scheduler_handle {
        storage_admin_handler = storage_admin_handler.with_scrub(Arc::clone(s));
    }
    let storage_admin_svc =
        kiseki_proto::v1::storage_admin_service_server::StorageAdminServiceServer::new(
            storage_admin_handler,
        );
    // Phase 16a step 7. The ClusterChunkService gRPC server delegates
    // to the *local* AsyncChunkOps (NOT the ClusteredChunkStore) so a
    // PutFragment from a peer leader stores the fragment on this node
    // without recursing into another fan-out. SAN-role enforcement
    // lives at the interceptor layer; on plaintext (development) the
    // server still functions but rejects cross-node writes only when
    // mTLS is configured (step 12).
    //
    // The interceptor is wired UNCONDITIONALLY when TLS is configured.
    // Otherwise (development plaintext), we install the unwrapped
    // server — the SAN check would always fail with "TLS client info
    // missing" and break local development. The TLS config is
    // mutually exclusive with multi-tenant access on this port, so
    // plaintext-mode is a development-only posture.
    let cluster_chunk_svc_intercepted = cfg.tls.is_some();
    let cluster_chunk_server = kiseki_chunk_cluster::ClusterChunkServer::with_envelope_registry(
        Arc::clone(&local_chunk_store),
        "default",
        envelope_registry.clone(),
    )
    // ADR-025 W4 — same `Arc<MaintenanceMode>` shared with
    // `StorageAdminGrpc` above so the admin-flipped flag is
    // visible on this node's data path.
    .with_maintenance(Arc::clone(&maintenance_mode), bootstrap_shard)
    // Receiver-side fabric phase histograms — every incoming
    // PutFragment observes its decode + write_chunk latency on
    // `kiseki_fabric_put_recv_phase_duration_seconds`.
    .with_metrics(cluster_chunk_server_fabric);

    let mut builder = tonic::transport::Server::builder()
        // HTTP/2 flow-control windows. Match the fabric Channel
        // settings — both peers need to grant a large window for
        // 64+ MiB envelopes to flow without WINDOW_UPDATE round-trip
        // storm. 2026-05-03 GCP fabric quorum-loss root cause.
        .initial_stream_window_size(16 * 1024 * 1024)
        .initial_connection_window_size(32 * 1024 * 1024);

    // Wire mTLS if configured.
    if let Some(ref tls_files) = cfg.tls {
        let tls = build_tls(tls_files)?;
        builder = builder
            .tls_config(tls)
            .map_err(|e| format!("data-path TLS config: {e}"))?;
        tracing::info!(addr = %cfg.data_addr, "data-path gRPC listening (mTLS)");
    } else {
        tracing::warn!(
            addr = %cfg.data_addr,
            "data-path gRPC listening (PLAINTEXT — development only)",
        );
    }

    // Wait for SIGINT (ctrl-c) OR SIGTERM (kill <pid>) so the
    // server cleans up — and on a `--features pprof` build the
    // outer main can render a flamegraph SVG before exit. The BDD
    // harness sends SIGTERM; the profile driver does too.
    let shutdown = async {
        let mut sigterm = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "data-path: SIGTERM listener failed; falling back to SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("data-path: SIGINT received, draining...");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("data-path: SIGINT received, draining...");
            }
            _ = sigterm.recv() => {
                tracing::info!("data-path: SIGTERM received, draining...");
            }
        }
    };

    let mut router = builder
        .add_service(control_svc)
        .add_service(key_svc)
        .add_service(log_svc)
        .add_service(admin_svc)
        .add_service(storage_admin_svc);
    if cluster_chunk_svc_intercepted {
        router = router.add_service(cluster_chunk_server.into_tonic_server_with_san_check());
        tracing::info!("ClusterChunkService: SAN-role interceptor active (mTLS)");
    } else {
        router = router.add_service(cluster_chunk_server.into_tonic_server());
        tracing::warn!(
            "ClusterChunkService: NO SAN interceptor (plaintext development mode — \
             cross-node fabric is not protected against tenant certs)",
        );
    }
    // ADR-042 Phase 4: register the native GatewayDataService alongside
    // the other data-path services. The interceptor wraps the same
    // tower stack; in plaintext development mode it falls through to a
    // synthetic dev principal (the runtime is single-tenant in that
    // posture). Audit emission uses NullAuditSink today; Phase 4
    // follow-up replaces it with the real `kiseki-audit` adapter.
    let native_audit: std::sync::Arc<dyn kiseki_gateway::native::san_interceptor::AuditSink> =
        std::sync::Arc::new(kiseki_gateway::native::san_interceptor::NullAuditSink);
    let native_intercept = std::sync::Arc::new(
        kiseki_gateway::native::san_interceptor::SanInterceptor::new(
            native_audit,
            cfg.tls.is_some(),
        ),
    );
    // ADR-044 — proxy fallback channel pool. The runtime wires this
    // unconditionally so a runtime-toggled
    // `KISEKI_NATIVE_PROXY_FALLBACK` flip doesn't need to allocate
    // the pool on the hot path. `register_node` for peer addresses is
    // populated below from the cluster topology (the data_addr port
    // is shared across nodes in `docker-compose` deployments;
    // localhost-multi-node test harnesses populate it via the
    // `peer_data_addrs` config — Step C wiring lands per node when
    // topology gossip becomes available).
    let proxy_client_for_native =
        std::sync::Arc::new(kiseki_gateway::native::proxy_client::ProxyClient::new(
            kiseki_common::ids::NodeId(cfg.node_id),
        ));
    // Best-effort peer registration: for every Raft peer we know the
    // address of, register a placeholder data_addr derived from the
    // local data_addr port. This mirrors what `config.rs` does for
    // the fabric path. Operators with non-uniform port deployments
    // override via `KISEKI_PEER_DATA_ADDRS=id=host:port,…` (Step C
    // makes this the topology-published source). Empty unless the
    // local node has peers.
    let peer_data_port = cfg.data_addr.port();
    for (peer_id, peer_addr) in &cfg.raft_peers {
        if *peer_id == cfg.node_id {
            continue;
        }
        let host = peer_addr
            .rsplit_once(':')
            .map_or(peer_addr.as_str(), |(h, _)| h);
        let data_addr = format!("{host}:{peer_data_port}");
        proxy_client_for_native.register_node(kiseki_common::ids::NodeId(*peer_id), data_addr);
    }
    let proxy_fallback_enabled = std::env::var("KISEKI_NATIVE_PROXY_FALLBACK")
        .ok()
        .as_deref()
        .is_some_and(|v| matches!(v, "on" | "1" | "true" | "yes"));
    let native_server = std::sync::Arc::new(
        kiseki_gateway::native::ServerImpl::new(
            std::sync::Arc::clone(&gw) as std::sync::Arc<dyn kiseki_gateway::ops::GatewayOps>,
            native_signing_keys,
        )
        .with_proxy_client(std::sync::Arc::clone(&proxy_client_for_native)),
    );
    native_server.set_proxy_fallback_enabled(proxy_fallback_enabled);
    tracing::info!(
        proxy_fallback = proxy_fallback_enabled,
        registered_peers = proxy_client_for_native.registered_nodes().len(),
        "ADR-044 native proxy fallback configured",
    );

    // ADR-042 §3.1 + §16.1 phase 4: the BindingSelector orchestrates
    // the three-phase startup. Each binding ships a probe; the
    // selector probes them all, detects port collisions, applies the
    // operator pin (`KISEKI_NATIVE_TRANSPORT`), and emits a plan in
    // priority order (Rdma > Low > Standard).
    let pin = match kiseki_transport::native::OperatorPin::parse(
        std::env::var("KISEKI_NATIVE_TRANSPORT").ok().as_deref(),
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "KISEKI_NATIVE_TRANSPORT parse failed");
            return Err(std::io::Error::other(e.to_string()).into());
        }
    };
    let mut selector = kiseki_transport::native::BindingSelector::new();
    selector.register(Box::new(kiseki_gateway::native::grpc::GrpcProbe::new(
        cfg.data_addr.to_string(),
    )));
    // Port plan: 9100 data-gRPC, 9101 advisory, 9102 advisory-stream,
    // 9103 native TCP-framed. ADR-042 §2.2 originally specified 9101
    // for TCP-framed which collided with ADR-021's advisory listener
    // (both used `KISEKI_*_ADDR` defaults at 9101). The collision was
    // silent on the 2026-05-07 GCP run — the advisory listener won
    // the race and TCP-framed exited with EADDRINUSE.
    selector.register(Box::new(
        kiseki_gateway::native::tcp_framed::TcpFramedProbe::new("0.0.0.0:9103"),
    ));
    let selector = selector.with_pin(pin);
    let (plan, report) = match selector.plan().await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "native binding selector failed");
            return Err(std::io::Error::other(e.to_string()).into());
        }
    };
    let banner = kiseki_transport::native::selector::render_banner(&plan, &report);
    for line in banner.lines() {
        tracing::info!("{}", line);
    }

    // ADR-042 §1.7: publish the local node's binding set into the
    // TopologyInjector so `GetTopology` carries the per-node
    // BindingEndpoint list every client needs for §3.2 per-edge
    // selection. Single-node deployment: this node's bindings ARE
    // the topology. Multi-node deployments augment via control-
    // plane gossip (a follow-up — once gossip lands, this same
    // helper produces the LOCAL entry while the cluster-wide list
    // is assembled across nodes).
    {
        let local_node_info = kiseki_gateway::native::topology_proto::node_info_from_plan(
            cfg.node_id,
            kiseki_proto::v1::native::NodeState::Active,
            &plan,
        );
        native_server
            .topology()
            .replace(kiseki_gateway::native::server::TopologySnapshot {
                nodes: vec![local_node_info],
                shards_by_tenant: std::collections::HashMap::new(),
            });
    }

    // ADR-042 §1.8: the binding-agnostic handler (`ServerImpl`) is
    // wrapped by per-binding adapters. Same `ServerImpl` instance
    // backs every binding the plan includes — multiple bindings
    // concurrently when the operator hasn't pinned one.
    let mut tcp_framed_listener_to_spawn: Option<
        kiseki_gateway::native::tcp_framed::TcpFramedListener,
    > = None;
    for binding in &plan.spawn_order {
        match binding.binding_id {
            kiseki_proto::native_contract::BindingId::Grpc => {
                let adapter = kiseki_gateway::native::grpc::GrpcAdapter::new(
                    std::sync::Arc::clone(&native_server),
                );
                let native_intercept_for_tonic = std::sync::Arc::clone(&native_intercept);
                let native_inner = kiseki_proto::v1::native::gateway_data_service_server::GatewayDataServiceServer::new(
                    adapter,
                )
                // Streaming PUT can deliver up to the configured
                // per-stream cap (default 64 MiB, env
                // KISEKI_NATIVE_STREAM_CAP overrides). The server-
                // side decoded-message-size default is 4 MiB; raise
                // to match the per-stream cap so a single
                // PutObjectChunk payload doesn't get OutOfRange'd at
                // the codec boundary. Encoding cap covers GET
                // responses with similarly large payloads.
                .max_decoding_message_size(64 * 1024 * 1024)
                .max_encoding_message_size(64 * 1024 * 1024);
                let native_svc = tonic::service::interceptor::InterceptedService::new(
                    native_inner,
                    move |req: tonic::Request<()>| native_intercept_for_tonic.intercept(req),
                );
                router = router.add_service(native_svc);
                tracing::info!(
                    require_tls = cfg.tls.is_some(),
                    "gRPC native binding registered on shared data_addr (ADR-042 §2.1)",
                );
            }
            kiseki_proto::native_contract::BindingId::TcpFramed => {
                let addr_str = if let kiseki_proto::native_contract::ListenAddr::HostPort(s) =
                    &binding.addr
                {
                    s.clone()
                } else {
                    // TCP-framed should always have a host:port —
                    // FabricDescriptor is RDMA-only.
                    tracing::warn!("TCP-framed binding probed with non-HostPort addr; skipping",);
                    continue;
                };
                tcp_framed_listener_to_spawn =
                    Some(kiseki_gateway::native::tcp_framed::TcpFramedListener::new(
                        addr_str.clone(),
                        std::sync::Arc::clone(&native_server),
                        // TLS wiring deferred to a follow-up slice —
                        // listener accepts the same
                        // `Arc<rustls::ServerConfig>` shape the gRPC
                        // binding uses; for now plaintext in dev mode.
                        None,
                        /* allow_plaintext = */ cfg.tls.is_none(),
                    ));
                tracing::info!(
                    addr = %addr_str,
                    require_tls = cfg.tls.is_some(),
                    "TCP-framed native binding registered (ADR-042 §2.2)",
                );
            }
            kiseki_proto::native_contract::BindingId::Ibverbs => {
                tracing::warn!(
                    "ibverbs native binding probed but listener not yet implemented (ADR-042 phase 9)",
                );
            }
            kiseki_proto::native_contract::BindingId::Libfabric { .. } => {
                tracing::warn!(
                    "libfabric native binding probed but listener not yet implemented (ADR-042 phase 10)",
                );
            }
        }
    }

    // Spawn the TCP-framed listener (if any) on the runtime — it
    // owns its socket separately from the cfg.data_addr router.
    if let Some(listener) = tcp_framed_listener_to_spawn {
        tokio::spawn(async move {
            if let Err(e) = listener.run().await {
                tracing::error!(error = %e, "TCP-framed native binding listener exited");
            }
        });
    }

    router.serve_with_shutdown(cfg.data_addr, shutdown).await?;

    tracing::info!("data-path: shut down");
    Ok(())
}

/// Run the advisory runtime on its isolated tokio runtime.
///
/// Starts both the gRPC service (on `addr`) and a TCP stream server
/// (on `stream_addr`) for non-gRPC clients. The TCP stream uses
/// length-prefixed JSON for lightweight hint submission from
/// `kiseki-client` without requiring a tonic dependency.
pub async fn run_advisory(
    addr: SocketAddr,
    stream_addr: SocketAddr,
    tls_files: Option<&TlsFiles>,
    workflow_table: Arc<std::sync::Mutex<kiseki_advisory::WorkflowTable>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let budget = BudgetConfig {
        hints_per_sec: 100,
        max_concurrent_workflows: 10,
        max_phases_per_workflow: 50,
    };

    let advisory_svc = WorkflowAdvisoryServiceServer::new(AdvisoryGrpc::with_table(
        workflow_table,
        budget.clone(),
    ));

    // Shared budget enforcer for the TCP stream server.
    let stream_budget = Arc::new(std::sync::Mutex::new(kiseki_advisory::BudgetEnforcer::new(
        budget,
    )));

    // Start TCP advisory stream server alongside gRPC.
    tokio::spawn(async move {
        if let Err(e) =
            kiseki_advisory::stream::run_advisory_stream_server(stream_addr, stream_budget).await
        {
            tracing::error!(error = %e, "advisory TCP stream server error");
        }
    });

    let mut builder = tonic::transport::Server::builder();

    if let Some(files) = tls_files {
        let tls = build_tls(files)?;
        builder = builder
            .tls_config(tls)
            .map_err(|e| format!("advisory TLS config: {e}"))?;
        tracing::info!(%addr, "advisory gRPC listening (mTLS)");
    } else {
        tracing::warn!(%addr, "advisory gRPC listening (PLAINTEXT — development only)");
    }

    // Wait for SIGINT or SIGTERM. The advisory runtime is awaited
    // by `main` after the data-path completes; if it doesn't honor
    // SIGTERM the binary hangs after a `kill <pid>` and (on
    // `--features pprof` builds) the flamegraph SVG never renders.
    let shutdown = async {
        let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("advisory: SIGINT received, draining...");
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("advisory: SIGINT received, draining...");
            }
            _ = sigterm.recv() => {
                tracing::info!("advisory: SIGTERM received, draining...");
            }
        }
    };

    builder
        .add_service(advisory_svc)
        .serve_with_shutdown(addr, shutdown)
        .await?;

    tracing::info!("advisory: shut down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    /// The 5 canonical persistent store paths that the runtime constructs
    /// under `data_dir`. Three are fjall keyspaces (directories), one is
    /// a redb database (small-object inline store), and one is a chunk
    /// device file. All must be in distinct subdirectories under
    /// `data_dir`.
    ///
    /// Layout (from `runtime::run_main`):
    ///   raft/log/           — Raft log shard store (fjall, dir)
    ///   keys/epochs/        — Key manager epochs (fjall, dir)
    ///   chunks/meta/        — Chunk + fragment meta (fjall, dir; ADR-022 rev-4)
    ///   chunks/data.dev     — Raw block device for chunk ciphertext
    ///   small/objects.redb  — Small object inline store (still redb)
    fn canonical_store_paths(data_dir: &std::path::Path) -> [PathBuf; 5] {
        [
            data_dir.join("raft").join("log"),
            data_dir.join("keys").join("epochs"),
            data_dir.join("chunks").join("meta"),
            data_dir.join("small").join("objects.redb"),
            data_dir.join("chunks").join("data.dev"),
        ]
    }

    #[test]
    fn store_layout_paths_are_distinct_and_under_data_dir() {
        let data_dir =
            std::env::temp_dir().join(format!("kiseki-store-layout-test-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();

        let paths = canonical_store_paths(&data_dir);

        // All 5 paths must be distinct.
        let unique: HashSet<&PathBuf> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            5,
            "all 5 store paths must be distinct: {paths:?}"
        );

        // Each path must be under data_dir.
        for path in &paths {
            assert!(
                path.starts_with(&data_dir),
                "store path {path:?} must be under data_dir {data_dir:?}"
            );
        }

        // The remaining redb store keeps the .redb extension; the
        // three fjall keyspaces are directories with no extension;
        // the chunk device file uses a `.dev` extension.
        let redb_path = &paths[3];
        assert_eq!(
            redb_path.extension().and_then(|e| e.to_str()),
            Some("redb"),
            "redb store path must have .redb extension: {redb_path:?}"
        );
        for fjall_path in &paths[..3] {
            assert!(
                fjall_path.extension().is_none(),
                "fjall keyspace path must have no extension: {fjall_path:?}"
            );
        }

        // Subdirectories must collapse to {raft, keys, chunks, small}
        // — chunks/meta + chunks/data.dev share the same first
        // component.
        let subdirs: HashSet<_> = paths
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&data_dir)
                    .ok()
                    .and_then(|rel| rel.components().next())
                    .map(|c| c.as_os_str().to_owned())
            })
            .collect();
        assert_eq!(
            subdirs.len(),
            4,
            "stores collapse to 4 distinct first-level subdirs \
             (chunks/meta + chunks/data.dev share `chunks`): {subdirs:?}"
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// Phase 16 e2e fix: fabric peer addresses must point at the
    /// data-path port (where `ClusterChunkService` listens), not the
    /// Raft port. Pre-fix: `PutFragment` fan-out hit the Raft gRPC
    /// server, returned an unimplemented error, and quorum collapsed
    /// to leader-only.
    #[test]
    fn fabric_addr_remaps_raft_port_to_data_port() {
        assert_eq!(
            super::fabric_addr_from_raft_peer("kiseki-node2:9300", 9100),
            "kiseki-node2:9100",
        );
        assert_eq!(
            super::fabric_addr_from_raft_peer("10.0.0.5:9300", 9100),
            "10.0.0.5:9100",
        );
    }

    /// IPv6 host literals are bracketed. `rsplit_once(':')` keeps the
    /// brackets on the host side, which is the form `tonic::Uri` parses.
    #[test]
    fn fabric_addr_preserves_ipv6_brackets() {
        assert_eq!(
            super::fabric_addr_from_raft_peer("[2001:db8::1]:9300", 9100),
            "[2001:db8::1]:9100",
        );
    }

    /// Defensive — if the caller passed a port-less string we return
    /// it verbatim so the existing log-and-skip branch in `run_main`
    /// fires on the resulting connect error rather than silently
    /// fabricating an address.
    #[test]
    fn fabric_addr_passes_through_when_port_missing() {
        assert_eq!(
            super::fabric_addr_from_raft_peer("kiseki-node2", 9100),
            "kiseki-node2",
        );
    }

    // ---------------------------------------------------------------------
    // compute_storage_ds_addrs — what the MDS will hand out via
    // GETDEVICEINFO. Wrong answers here send pNFS clients to the wrong
    // host:port and every read fails with `Connection refused`.
    // ---------------------------------------------------------------------

    /// `KISEKI_DS_PEERS` wins outright — it's the only mode that can
    /// represent localhost-multi-node where every peer has a distinct
    /// ephemeral DS port.
    #[test]
    fn ds_peers_take_priority() {
        let ds_peers = vec![
            (1, "127.0.0.1:40001".to_string()),
            (2, "127.0.0.1:40002".to_string()),
        ];
        let raft_peers = vec![(1, "127.0.0.1:9301".to_string())];
        let local: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let got = super::compute_storage_ds_addrs(&ds_peers, &raft_peers, Some(local));
        assert_eq!(got, vec!["127.0.0.1:40001", "127.0.0.1:40002"]);
    }

    /// No `ds_peers` but `raft_peers` present → host-substitute each
    /// peer with the local DS port. The container/hostnamed deployment.
    #[test]
    fn raft_peers_substitute_local_ds_port() {
        let raft_peers = vec![
            (1, "kiseki-node1:9300".to_string()),
            (2, "kiseki-node2:9300".to_string()),
        ];
        let local: std::net::SocketAddr = "127.0.0.1:2052".parse().unwrap();
        let got = super::compute_storage_ds_addrs(&[], &raft_peers, Some(local));
        assert_eq!(got, vec!["kiseki-node1:2052", "kiseki-node2:2052"]);
    }

    /// Single-node with no peers but a configured `ds_addr` MUST
    /// advertise that address. Regression: pre-fix this returned an
    /// empty Vec, `MdsLayoutManager` defaulted to `127.0.0.1:2052`, and
    /// every pNFS read failed with `Connection refused` whenever the
    /// real DS bound to a different port (e.g. ephemeral in tests).
    #[test]
    fn single_node_advertises_local_ds() {
        let local: std::net::SocketAddr = "127.0.0.1:40577".parse().unwrap();
        let got = super::compute_storage_ds_addrs(&[], &[], Some(local));
        assert_eq!(got, vec!["127.0.0.1:40577"]);
    }

    /// Single-node with the canonical port still advertises it — the
    /// fix doesn't special-case 2052.
    #[test]
    fn single_node_default_port_still_advertised() {
        let local: std::net::SocketAddr = "127.0.0.1:2052".parse().unwrap();
        let got = super::compute_storage_ds_addrs(&[], &[], Some(local));
        assert_eq!(got, vec!["127.0.0.1:2052"]);
    }

    /// No peers and no `ds_addr` → empty Vec. The caller upstack
    /// (`MdsLayoutManager`) makes its own decision; we don't fabricate.
    #[test]
    fn no_peers_no_local_addr_returns_empty() {
        let got = super::compute_storage_ds_addrs(&[], &[], None);
        assert!(got.is_empty());
    }
}
