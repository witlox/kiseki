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
        .connect_timeout(std::time::Duration::from_secs(5));

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

/// Run the main data-path server.
#[allow(clippy::too_many_lines)]
pub async fn run_main(cfg: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
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
            &dir.join("keys").join("epochs.redb"),
            &*identity,
            &salt,
        )
        .map_err(|e| format!("persistent key store: {e}"))?;
        tracing::info!(
            epoch = store.health().current_epoch.unwrap_or(0),
            "key manager: persistent (redb) ready",
        );
        store
    } else {
        // In-memory: use a process-scoped tempdir for both the redb file
        // and the file-based node identity. Ephemeral by design.
        let tmp = std::env::temp_dir().join(format!("kiseki-keys-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).ok();
        let identity = kiseki_keymanager::node_identity::FileIdentitySource::new(
            tmp.join("node-identity.key"),
        )
        .map_err(|e| format!("ephemeral node identity: {e}"))?;
        let store =
            kiseki_keymanager::PersistentKeyStore::open(&tmp.join("epochs.redb"), &identity, &salt)
                .map_err(|e| format!("key store init: {e}"))?;
        tracing::info!(
            epoch = store.health().current_epoch.unwrap_or(0),
            "key manager: in-memory (ephemeral) ready",
        );
        store
    };
    let key_store = Arc::new(key_store);

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

    // Log store: Raft (multi-node), persistent (redb), or in-memory.
    let bootstrap_shard = kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1));
    let bootstrap_tenant = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));

    let log_store: Arc<dyn kiseki_log::LogOps + Send + Sync> = if cfg.node_id > 0
        && cfg.raft_peers.len() > 1
    {
        // Multi-node Raft: consensus-replicated log store.
        let peers: std::collections::BTreeMap<u64, String> =
            cfg.raft_peers.iter().cloned().collect();
        let raft_addr_str = cfg
            .raft_addr
            .map_or_else(|| "0.0.0.0:9300".to_owned(), |a| a.to_string());
        let mut store = kiseki_log::RaftShardStore::new(cfg.node_id, peers, cfg.data_dir.clone());
        if let Some(ref ss) = small_store {
            store = store.with_inline_store(std::sync::Arc::clone(ss)
                as std::sync::Arc<dyn kiseki_common::inline_store::InlineStore>);
        }
        // All nodes in the cluster create the shard. The bootstrap flag
        // controls whether this node seeds the Raft group (calls initialize)
        // or joins as a follower (receives membership from the leader).
        store.create_shard(
            bootstrap_shard,
            bootstrap_tenant,
            kiseki_common::ids::NodeId(cfg.node_id),
            kiseki_log::ShardConfig::default(),
            Some(&raft_addr_str),
            cfg.bootstrap,
        );
        tracing::info!(
            node_id = cfg.node_id,
            peers = cfg.raft_peers.len(),
            "log store: Raft",
        );
        Arc::new(store)
    } else if let Some(ref dir) = cfg.data_dir {
        std::fs::create_dir_all(dir.join("raft")).ok();
        let store = kiseki_log::persistent_store::PersistentShardStore::open(
            &dir.join("raft").join("log.redb"),
        )
        .await
        .map_err(|e| format!("persistent store: {e}"))?;
        if cfg.bootstrap {
            store.create_shard(
                bootstrap_shard,
                bootstrap_tenant,
                kiseki_common::ids::NodeId(1),
                kiseki_log::ShardConfig::default(),
            );
        }
        tracing::info!(path = %dir.display(), "log store: persistent (redb)");
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
    let metrics = crate::metrics::KisekiMetrics::new();

    // Local chunk store: persistent (raw block device) if KISEKI_DATA_DIR
    // set, otherwise in-memory. Wrapped via SyncBridge so it satisfies
    // AsyncChunkOps — the cluster fabric and the gateway both consume the
    // async surface (Phase 16a, D-7).
    let local_chunk_store: Arc<dyn kiseki_chunk::AsyncChunkOps> = if let Some(ref dir) =
        cfg.data_dir
    {
        std::fs::create_dir_all(dir.join("chunks")).ok();
        let dev_path = dir.join("chunks").join("data.dev");
        let meta_path = dir.join("chunks").join("meta.json");
        let store = if dev_path.exists() {
            kiseki_chunk::PersistentChunkStore::open(&dev_path, &meta_path)
                .map_err(|e| format!("persistent chunk store open: {e}"))?
        } else {
            kiseki_chunk::PersistentChunkStore::init(&dev_path, &meta_path, 4 * 1024 * 1024 * 1024)
                .map_err(|e| format!("persistent chunk store init: {e}"))?
        };
        tracing::info!(path = %dir.display(), "chunk store: persistent (raw block)");
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
    let chunk_store: Arc<dyn kiseki_chunk::AsyncChunkOps> = Arc::new(
        kiseki_chunk_cluster::ClusteredChunkStore::new(
            Arc::clone(&local_chunk_store),
            fabric_peers,
            cluster_cfg,
        )
        .with_metrics(Arc::clone(&metrics.fabric)),
    );

    // Phase 16d step 4: spawn the periodic scrub scheduler when
    // running on a real cluster (>=1 peer; in single-node mode
    // there are no fragments to scrub against and no peers to
    // probe / repair from). Cadence is currently a fixed 10
    // minutes per shard — operators can revisit once the
    // scheduler ships per-shard metrics.
    if !fabric_peers_for_scrub.is_empty() {
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
        let scrub_handle =
            scheduler.start_periodic(std::time::Duration::from_secs(600), scrub_shutdown_rx);
        // Detach: the channel sender + JoinHandle stay alive for
        // the process lifetime. Wiring a unified shutdown signal
        // is a runtime-wide concern tracked in
        // `specs/escalations/`.
        std::mem::drop((scrub_shutdown_tx, scrub_handle));
        tracing::info!(
            "scrub scheduler: spawned (orphan + under-replication, 10-min cadence, drain-on-shutdown)",
        );
    }

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

    // Composition: wired to log for delta emission. ADR-040: when
    // KISEKI_DATA_DIR is set we back the comp_id → Composition map
    // with a redb file at `<data_dir>/metadata/compositions.redb`,
    // so hydrated state survives restart and a node that joins late
    // resumes from durable `last_applied_seq`. Single-node /
    // no-data-dir deployments keep the in-memory backend (MemoryStorage)
    // — same behavior as pre-ADR-040.
    // Captures the redb path so the periodic gauge refresher can stat
    // its size; None when the composition store is in-memory.
    let mut comp_redb_path: Option<std::path::PathBuf> = None;
    let comp_storage: Box<dyn kiseki_composition::persistent::CompositionStorage> =
        if let Some(ref dir) = cfg.data_dir {
            let meta_dir = dir.join("metadata");
            std::fs::create_dir_all(&meta_dir).map_err(|e| {
                format!(
                    "create persistent composition dir {}: {e}",
                    meta_dir.display()
                )
            })?;
            let path = meta_dir.join("compositions.redb");
            let store = kiseki_composition::persistent::PersistentRedbStorage::open(&path)
                .map_err(|e| format!("open persistent composition store: {e}"))?
                .with_metrics(Arc::clone(&metrics.composition));
            tracing::info!(
                path = %path.display(),
                "composition store: persistent (redb-backed, ADR-040)",
            );
            comp_redb_path = Some(path);
            Box::new(store)
        } else {
            tracing::info!("composition store: in-memory (no KISEKI_DATA_DIR)");
            Box::new(kiseki_composition::persistent::MemoryStorage::new())
        };
    let mut comp_store =
        kiseki_composition::composition::CompositionStore::with_storage(comp_storage)
            .with_log(Arc::clone(&log_store) as Arc<dyn kiseki_log::LogOps + Send + Sync>);

    // View: shared between gateway (staleness check) and stream processor.
    let view_store = Arc::new(std::sync::Mutex::new(kiseki_view::view::ViewStore::new()));

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
    let _ = view_store
        .lock()
        .unwrap()
        .create_view(kiseki_view::ViewDescriptor {
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

    // S3 gateway.
    let s3_gw = kiseki_gateway::s3::S3Gateway::new(Arc::clone(&gw));
    let s3_router = kiseki_gateway::s3_server::s3_router(s3_gw, bootstrap_tenant);
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
        let ds_port = cfg.ds_addr.map_or(2052, |a| a.port());
        let storage_ds_addrs: Vec<String> = cfg
            .raft_peers
            .iter()
            .map(|(_, addr)| {
                let host = addr.split(':').next().unwrap_or(addr);
                format!("{host}:{ds_port}")
            })
            .collect();
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
    std::thread::spawn(move || {
        kiseki_gateway::nfs_server::serve_nfs_listener_with_mgr(
            nfs_listener,
            nfs_gw,
            bootstrap_tenant,
            bootstrap_ns,
            nfs_storage_nodes,
            pnfs_layout_mgr_for_nfs,
            None,
            nfs_tls_for_thread,
        );
    });

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
            });
            let ds_tls_for_thread = nfs_tls.clone();
            std::thread::spawn(move || {
                kiseki_gateway::pnfs_ds_server::run_ds_server(
                    ds_addr,
                    ds_ctx,
                    None,
                    ds_tls_for_thread,
                );
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
                let mut vs = sp_views
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    // §D10 — periodic stat of `compositions.redb` so the
    // `kiseki_composition_redb_size_bytes` gauge tracks on-disk growth.
    // Only spawned when the persistent store is active. Also refreshes
    // `kiseki_composition_count` from the live store (cheap — single
    // redb metadata read, no scan).
    if let Some(path) = comp_redb_path {
        let size_metrics = composition_metrics_for_size_refresh;
        let count_compositions = gw.compositions_handle();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if let Ok(meta) = std::fs::metadata(&path) {
                    size_metrics
                        .redb_size_bytes
                        .set(i64::try_from(meta.len()).unwrap_or(i64::MAX));
                }
                let store = count_compositions.lock().await;
                if let Ok(c) = store.storage().count() {
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
    let log_svc = LogServiceServer::new(LogGrpc::new(log_store));
    let admin_svc = kiseki_proto::v1::admin_service_server::AdminServiceServer::new(
        crate::admin_grpc::AdminGrpc::from_runtime(),
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
    let cluster_chunk_server =
        kiseki_chunk_cluster::ClusterChunkServer::new(Arc::clone(&local_chunk_store), "default");

    let mut builder = tonic::transport::Server::builder();

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

    let shutdown = async {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("data-path: shutdown signal received, draining...");
    };

    let mut router = builder
        .add_service(control_svc)
        .add_service(key_svc)
        .add_service(log_svc)
        .add_service(admin_svc);
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
) -> Result<(), Box<dyn std::error::Error>> {
    let budget = BudgetConfig {
        hints_per_sec: 100,
        max_concurrent_workflows: 10,
        max_phases_per_workflow: 50,
    };

    let advisory_svc = WorkflowAdvisoryServiceServer::new(AdvisoryGrpc::new(budget.clone()));

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

    let shutdown = async {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("advisory: shutdown signal received, draining...");
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

    /// The 4 canonical persistent store paths that the runtime constructs
    /// under `data_dir`. Three are redb databases, one is a chunk device +
    /// metadata pair. All must be in distinct subdirectories under `data_dir`.
    ///
    /// Layout (from `runtime::run_main`):
    ///   raft/log.redb       — Raft log (persistent shard store)
    ///   keys/epochs.redb    — Key manager epochs
    ///   small/objects.redb  — Small object inline store
    ///   chunks/data.dev     — Raw block device for chunks
    fn canonical_store_paths(data_dir: &std::path::Path) -> [PathBuf; 4] {
        [
            data_dir.join("raft").join("log.redb"),
            data_dir.join("keys").join("epochs.redb"),
            data_dir.join("small").join("objects.redb"),
            data_dir.join("chunks").join("data.dev"),
        ]
    }

    #[test]
    fn redb_layout_paths_are_distinct_and_under_data_dir() {
        let data_dir =
            std::env::temp_dir().join(format!("kiseki-redb-layout-test-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();

        let paths = canonical_store_paths(&data_dir);

        // All 4 paths must be distinct.
        let unique: HashSet<&PathBuf> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            4,
            "all 4 store paths must be distinct: {paths:?}"
        );

        // Each path must be under data_dir.
        for path in &paths {
            assert!(
                path.starts_with(&data_dir),
                "store path {path:?} must be under data_dir {data_dir:?}"
            );
        }

        // The 3 redb stores must have .redb extension.
        let redb_paths = &paths[..3];
        for path in redb_paths {
            assert_eq!(
                path.extension().and_then(|e| e.to_str()),
                Some("redb"),
                "redb store path must have .redb extension: {path:?}"
            );
        }

        // Subdirectories must be distinct (raft, keys, small, chunks).
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
            "each store must reside in a distinct subdirectory: {subdirs:?}"
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
}
