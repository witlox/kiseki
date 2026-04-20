//! Runtime composition — wires all contexts and starts gRPC servers.

use std::net::SocketAddr;
use std::sync::Arc;

use kiseki_advisory::budget::BudgetConfig;
use kiseki_advisory::grpc::AdvisoryGrpc;
use kiseki_keymanager::grpc::KeyManagerGrpc;
use kiseki_keymanager::raft_store::RaftKeyStore;
use kiseki_log::grpc::LogGrpc;
use kiseki_proto::v1::key_manager_service_server::KeyManagerServiceServer;
use kiseki_proto::v1::log_service_server::LogServiceServer;
use kiseki_proto::v1::workflow_advisory_service_server::WorkflowAdvisoryServiceServer;
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
pub async fn run_main(cfg: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    // --- Context construction ---

    // Key Manager: Raft-ready store with initial epoch.
    let key_store = RaftKeyStore::new().map_err(|e| format!("key store init: {e}"))?;
    let key_store = Arc::new(key_store);
    eprintln!(
        "  key manager: epoch {} ready",
        key_store.health().current_epoch.unwrap_or(0)
    );

    // Log: in-memory store, shared across composition and view.
    let log_store = Arc::new(kiseki_log::MemShardStore::new());

    // Audit: in-memory store.
    let _audit_store = kiseki_audit::AuditLog::new();

    // Chunk: in-memory store.
    let _chunk_store = kiseki_chunk::ChunkStore::new();

    // Composition: wired to log for delta emission.
    let _comp_store = kiseki_composition::composition::CompositionStore::new()
        .with_log(Arc::clone(&log_store) as Arc<dyn kiseki_log::LogOps + Send + Sync>);

    // View: in-memory store (stream processor polls from log_store).
    let _view_store = kiseki_view::view::ViewStore::new();

    // --- gRPC services ---

    let key_svc = KeyManagerServiceServer::new(KeyManagerGrpc::new(key_store));
    let log_svc = LogServiceServer::new(LogGrpc::new(log_store));

    let mut builder = tonic::transport::Server::builder();

    // Wire mTLS if configured.
    if let Some(ref tls_files) = cfg.tls {
        let tls = build_tls(tls_files)?;
        builder = builder
            .tls_config(tls)
            .map_err(|e| format!("data-path TLS config: {e}"))?;
        eprintln!("  data-path gRPC listening on {} (mTLS)", cfg.data_addr);
    } else {
        eprintln!(
            "  WARNING: data-path gRPC listening on {} (PLAINTEXT — development only)",
            cfg.data_addr
        );
    }

    let shutdown = async {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("  data-path: shutdown signal received, draining...");
    };

    builder
        .add_service(key_svc)
        .add_service(log_svc)
        .serve_with_shutdown(cfg.data_addr, shutdown)
        .await?;

    eprintln!("  data-path: shut down.");
    Ok(())
}

/// Run the advisory runtime on its isolated tokio runtime.
pub async fn run_advisory(
    addr: SocketAddr,
    tls_files: Option<&TlsFiles>,
) -> Result<(), Box<dyn std::error::Error>> {
    let budget = BudgetConfig {
        hints_per_sec: 100,
        max_concurrent_workflows: 10,
        max_phases_per_workflow: 50,
    };

    let advisory_svc = WorkflowAdvisoryServiceServer::new(AdvisoryGrpc::new(budget));

    let mut builder = tonic::transport::Server::builder();

    if let Some(files) = tls_files {
        let tls = build_tls(files)?;
        builder = builder
            .tls_config(tls)
            .map_err(|e| format!("advisory TLS config: {e}"))?;
        eprintln!("  advisory gRPC listening on {addr} (mTLS)");
    } else {
        eprintln!("  WARNING: advisory gRPC listening on {addr} (PLAINTEXT — development only)");
    }

    let shutdown = async {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("  advisory: shutdown signal received, draining...");
    };

    builder
        .add_service(advisory_svc)
        .serve_with_shutdown(addr, shutdown)
        .await?;

    eprintln!("  advisory: shut down.");
    Ok(())
}
