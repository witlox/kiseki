//! Runtime composition — wires all contexts and starts gRPC servers.

use std::net::SocketAddr;
use std::sync::Arc;

use kiseki_advisory::budget::BudgetConfig;
use kiseki_advisory::grpc::AdvisoryGrpc;
use kiseki_keymanager::grpc::KeyManagerGrpc;
use kiseki_keymanager::raft_store::RaftKeyStore;
use kiseki_proto::v1::key_manager_service_server::KeyManagerServiceServer;
use kiseki_proto::v1::workflow_advisory_service_server::WorkflowAdvisoryServiceServer;

use crate::config::ServerConfig;

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

    // Log: in-memory store (Raft integration will replace).
    let _log_store = kiseki_log::MemShardStore::new();

    // Audit: in-memory store.
    let _audit_store = kiseki_audit::AuditLog::new();

    // Chunk: in-memory store.
    let _chunk_store = kiseki_chunk::ChunkStore::new();

    // Composition: in-memory store.
    let _comp_store = kiseki_composition::composition::CompositionStore::new();

    // View: in-memory store.
    let _view_store = kiseki_view::view::ViewStore::new();

    // --- gRPC services ---

    let key_svc = KeyManagerServiceServer::new(KeyManagerGrpc::new(key_store));

    eprintln!("  data-path gRPC listening on {}", cfg.data_addr);

    tonic::transport::Server::builder()
        .add_service(key_svc)
        .serve(cfg.data_addr)
        .await?;

    Ok(())
}

/// Run the advisory runtime on its isolated tokio runtime.
pub async fn run_advisory(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let budget = BudgetConfig {
        hints_per_sec: 100,
        max_concurrent_workflows: 10,
        max_phases_per_workflow: 50,
    };

    let advisory_svc = WorkflowAdvisoryServiceServer::new(AdvisoryGrpc::new(budget));

    eprintln!("  advisory gRPC listening on {addr}");

    tonic::transport::Server::builder()
        .add_service(advisory_svc)
        .serve(addr)
        .await?;

    Ok(())
}
