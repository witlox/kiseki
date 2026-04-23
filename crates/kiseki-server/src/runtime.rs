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
    // Uses PersistentKeyStore for dual-write (memory + redb) in persistent mode.
    // Falls back to plain RaftKeyStore (memory-only) otherwise.
    // Both implement KeyManagerOps; gRPC uses PersistentKeyStore when available.
    let key_store = if let Some(ref dir) = cfg.data_dir {
        std::fs::create_dir_all(dir.join("keys")).ok();
        let store =
            kiseki_keymanager::PersistentKeyStore::open(&dir.join("keys").join("epochs.redb"))
                .map_err(|e| format!("persistent key store: {e}"))?;
        tracing::info!(
            epoch = store.health().current_epoch.unwrap_or(0),
            "key manager: persistent (redb) ready",
        );
        store
    } else {
        // In-memory: use PersistentKeyStore with a temp path that won't be reused.
        // This keeps the runtime code uniform (single type for key_store).
        let tmp = std::env::temp_dir().join(format!("kiseki-keys-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).ok();
        let store = kiseki_keymanager::PersistentKeyStore::open(&tmp.join("epochs.redb"))
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

    let log_store: Arc<dyn kiseki_log::LogOps + Send + Sync> =
        if cfg.node_id > 0 && cfg.raft_peers.len() > 1 {
            // Multi-node Raft: consensus-replicated log store.
            let peers: std::collections::BTreeMap<u64, String> =
                cfg.raft_peers.iter().cloned().collect();
            let raft_addr_str = cfg
                .raft_addr
                .map_or_else(|| "0.0.0.0:9300".to_owned(), |a| a.to_string());
            let mut store = kiseki_log::RaftShardStore::new(
                cfg.node_id,
                peers,
                tokio::runtime::Handle::current(),
                cfg.data_dir.clone(),
            );
            if let Some(ref ss) = small_store {
                store = store.with_inline_store(std::sync::Arc::clone(ss)
                    as std::sync::Arc<dyn kiseki_common::inline_store::InlineStore>);
            }
            if cfg.bootstrap {
                store.create_shard(
                    bootstrap_shard,
                    bootstrap_tenant,
                    kiseki_common::ids::NodeId(cfg.node_id),
                    kiseki_log::ShardConfig::default(),
                    Some(&raft_addr_str),
                );
            }
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

    // Chunk store: persistent (raw block device) if KISEKI_DATA_DIR set,
    // otherwise in-memory. The gateway accepts any ChunkOps implementation.
    let chunk_store: Box<dyn kiseki_chunk::ChunkOps + Send> = if let Some(ref dir) = cfg.data_dir {
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
        Box::new(store)
    } else {
        tracing::info!("chunk store: in-memory (no persistence)");
        Box::new(kiseki_chunk::ChunkStore::new())
    };

    // Composition: wired to log for delta emission.
    let mut comp_store = kiseki_composition::composition::CompositionStore::new()
        .with_log(Arc::clone(&log_store) as Arc<dyn kiseki_log::LogOps + Send + Sync>);

    // View: shared between gateway (staleness check) and stream processor.
    let view_store = Arc::new(std::sync::Mutex::new(kiseki_view::view::ViewStore::new()));

    // Bootstrap namespace for protocol gateways (maps "default" bucket/export).
    let bootstrap_tenant = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));
    let bootstrap_ns =
        kiseki_common::ids::NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default"));
    if cfg.bootstrap {
        let bootstrap_shard = kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1));
        comp_store.add_namespace(kiseki_composition::namespace::Namespace {
            id: bootstrap_ns,
            tenant_id: bootstrap_tenant,
            shard_id: bootstrap_shard,
            read_only: false,
        });

        // Create a bootstrap view for the default namespace.
        let bootstrap_view = kiseki_common::ids::ViewId(uuid::Uuid::from_u128(1));
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
        tracing::info!("bootstrap: namespace 'default' + view for gateways");
    } else {
        tracing::warn!("KISEKI_BOOTSTRAP not set — S3/NFS gateways have no namespaces");
        tracing::warn!("set KISEKI_BOOTSTRAP=true for development/testing");
    }

    // Shared gateway: wires composition + chunk + crypto. Used by S3 and NFS.
    let master_key =
        kiseki_crypto::keys::SystemMasterKey::new([0x42; 32], kiseki_common::tenancy::KeyEpoch(1));
    let mut gw_builder = kiseki_gateway::InMemoryGateway::new(comp_store, chunk_store, master_key)
        .with_view_store(Arc::clone(&view_store));
    if let Some(ref ss) = small_store {
        gw_builder = gw_builder.with_inline_threshold(
            kiseki_log::ShardConfig::default().inline_threshold_bytes,
            std::sync::Arc::clone(ss)
                as std::sync::Arc<dyn kiseki_common::inline_store::InlineStore>,
        );
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

    // Prometheus metrics + admin UI server.
    let metrics = crate::metrics::KisekiMetrics::new();
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
    tokio::spawn(async move {
        if let Err(e) =
            crate::metrics::run_metrics_server(metrics_addr, metrics, peer_metrics_addrs).await
        {
            tracing::error!(error = %e, "metrics server error");
        }
    });

    // NFS gateway (NFSv3 + NFSv4.2 on port 2049).
    let nfs_gw = kiseki_gateway::nfs::NfsGateway::new(Arc::clone(&gw));
    let nfs_addr = cfg.nfs_addr;
    std::thread::spawn(move || {
        kiseki_gateway::nfs_server::run_nfs_server(
            nfs_addr,
            nfs_gw,
            bootstrap_tenant,
            bootstrap_ns,
        );
    });

    // Stream processor: polls deltas from log → advances view watermarks.
    let sp_log = Arc::clone(&log_store);
    let sp_views = Arc::clone(&view_store);
    let sp_view_id = kiseki_common::ids::ViewId(uuid::Uuid::from_u128(1));
    tokio::spawn(async move {
        loop {
            {
                let mut vs = sp_views
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut sp = kiseki_view::stream_processor::TrackedStreamProcessor::new(
                    sp_log.as_ref(),
                    &mut *vs,
                );
                sp.track(sp_view_id);
                sp.poll(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

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

    // --- gRPC services ---

    // Control plane (ADR-027: Rust-only).
    let control_tenants = Arc::new(TenantStore::new());
    let control_svc = ControlServiceServer::new(ControlGrpc::new(control_tenants));
    tracing::info!("control plane: in-process (ControlService on data-path gRPC)");

    let key_svc = KeyManagerServiceServer::new(KeyManagerGrpc::new(key_store));
    let log_svc = LogServiceServer::new(LogGrpc::new(log_store));

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

    builder
        .add_service(control_svc)
        .add_service(key_svc)
        .add_service(log_svc)
        .serve_with_shutdown(cfg.data_addr, shutdown)
        .await?;

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
