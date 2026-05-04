#![allow(clippy::unwrap_used, clippy::expect_used)]
//! pNFS Flexible Files state (ADR-038, Phase 15).

use std::sync::Arc;

pub struct PnfsState {
    pub mac_key: Option<kiseki_gateway::pnfs::PnfsFhMacKey>,
    pub fh: Option<kiseki_gateway::pnfs::PnfsFileHandle>,
    pub last_results: Vec<(u32, u32, Vec<u8>)>,
    pub gateway_reads: Arc<std::sync::atomic::AtomicU64>,
    pub composition_bytes: Option<Vec<u8>>,
    pub security_eval: Option<
        Result<
            kiseki_gateway::nfs_security::NfsSecurity,
            kiseki_gateway::nfs_security::NfsSecurityError,
        >,
    >,
    pub audit_log: Arc<kiseki_audit::store::AuditLog>,
    pub ds_ctx: Option<
        Arc<
            kiseki_gateway::pnfs_ds_server::DsContext<kiseki_gateway::mem_gateway::InMemoryGateway>,
        >,
    >,
    pub ds_addr: Option<std::net::SocketAddr>,
    pub ds_shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub mds_mgr: Option<Arc<kiseki_gateway::pnfs::MdsLayoutManager>>,
    pub last_layout: Option<kiseki_gateway::pnfs::ServerLayout>,
    pub clock_ms: u64,
}

impl PnfsState {
    pub fn new() -> Self {
        Self {
            mac_key: None,
            fh: None,
            last_results: Vec::new(),
            gateway_reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            composition_bytes: None,
            security_eval: None,
            audit_log: Arc::new(kiseki_audit::store::AuditLog::new()),
            ds_ctx: None,
            ds_addr: None,
            ds_shutdown: None,
            mds_mgr: None,
            last_layout: None,
            clock_ms: 1_000_000,
        }
    }
}
