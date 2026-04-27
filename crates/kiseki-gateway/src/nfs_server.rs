//! NFS TCP server — listens on port 2049, routes to NFSv3 or NFSv4.2.
//!
//! Both versions share the same port. The ONC RPC version field in
//! the first call determines which dispatcher handles the connection.
//! NFSv3 = version 3, NFSv4.x = version 4.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kiseki_common::ids::{NamespaceId, OrgId};
use rustls::ServerConfig;
use socket2::{SockRef, TcpKeepalive};

use crate::nfs::NfsGateway;
use crate::nfs3_server::handle_nfs3_connection;
use crate::nfs4_server::handle_nfs4_connection;
use crate::nfs_ops::NfsContext;
use crate::nfs_xdr::{read_rm_message, write_rm_message, RpcCallHeader, XdrReader};
use crate::ops::GatewayOps;

/// RFC 9289 §4.2 — recommended keep-alive cadence on a long-lived
/// NFS-over-TLS session. The 60-second interval is the upper bound
/// before NAT/firewall idle-timeouts can sever the TLS session in
/// typical deployments.
pub const RFC9289_KEEPALIVE_INTERVAL_SECS: u64 = 60;

/// Configure TCP keep-alive on an accepted connection. Per RFC 9289
/// §4.2 a 60-sec cadence is the default; the kernel handles the
/// idle-reset semantic (it only fires after `time` seconds of
/// idleness).
fn enable_tcp_keepalive(stream: &TcpStream) -> io::Result<()> {
    let ka = TcpKeepalive::new()
        .with_time(Duration::from_secs(RFC9289_KEEPALIVE_INTERVAL_SECS))
        .with_interval(Duration::from_secs(RFC9289_KEEPALIVE_INTERVAL_SECS));
    SockRef::from(stream).set_tcp_keepalive(&ka)
}

/// Start the NFS TCP server supporting both NFSv3 and NFSv4.2.
///
/// Spawns a thread per connection. The first RPC call determines the
/// version for that connection.
pub fn run_nfs_server<G: GatewayOps + Send + Sync + 'static>(
    addr: SocketAddr,
    gateway: NfsGateway<G>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
) {
    run_nfs_server_with_peers(addr, gateway, tenant_id, namespace_id, Vec::new());
}

/// Start the NFS server with pNFS storage node addresses for layout delegation.
pub fn run_nfs_server_with_peers<G: GatewayOps + Send + Sync + 'static>(
    addr: SocketAddr,
    gateway: NfsGateway<G>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
    storage_nodes: Vec<String>,
) {
    let listener = TcpListener::bind(addr).unwrap_or_else(|e| {
        tracing::error!(addr = %addr, error = %e, "NFS bind failed");
        std::process::exit(1);
    });
    serve_nfs_listener(
        listener,
        gateway,
        tenant_id,
        namespace_id,
        storage_nodes,
        None,
        None,
    );
}

/// Run the NFS server on an already-bound listener with an optional
/// shutdown signal. Tests can pre-bind on `127.0.0.1:0` and pass the
/// listener directly (avoiding a bind→drop→rebind race). Production
/// callers should use [`run_nfs_server`] which binds for them.
///
/// When `shutdown` is `Some` and the flag flips to `true`, the accept
/// loop exits after the current iteration; in-flight per-connection
/// threads are detached and exit on their own.
///
/// The `tls` parameter wires NFS-over-TLS (RFC 9289 / ADR-038 §D4.1).
/// When `Some`, every accepted `TcpStream` is wrapped in
/// `rustls::StreamOwned` before being handed to the per-connection
/// handler. When `None`, plaintext TCP is used (only allowed under
/// the audited `[security].allow_plaintext_nfs` fallback per
/// ADR-038 §D4.2).
#[allow(clippy::needless_pass_by_value)]
pub fn serve_nfs_listener<G: GatewayOps + Send + Sync + 'static>(
    listener: TcpListener,
    gateway: NfsGateway<G>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
    storage_nodes: Vec<String>,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    tls: Option<Arc<ServerConfig>>,
) {
    serve_nfs_listener_with_mgr(
        listener,
        gateway,
        tenant_id,
        namespace_id,
        storage_nodes,
        None,
        shutdown,
        tls,
    );
}

/// Phase 15c.4 — same as `serve_nfs_listener` plus an optional
/// production `MdsLayoutManager`. When threaded through, the kernel's
/// LAYOUTGET path returns Flex Files layouts pointing at real DS
/// endpoints instead of the legacy FILES-layout stub.
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub fn serve_nfs_listener_with_mgr<G: GatewayOps + Send + Sync + 'static>(
    listener: TcpListener,
    gateway: NfsGateway<G>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
    storage_nodes: Vec<String>,
    mds_layout_manager: Option<Arc<crate::pnfs::MdsLayoutManager>>,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    tls: Option<Arc<ServerConfig>>,
) {
    let ctx = Arc::new(NfsContext::with_storage_nodes_and_mgr(
        gateway,
        tenant_id,
        namespace_id,
        storage_nodes,
        mds_layout_manager,
    ));
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(addr = %addr, "NFS server listening (NFSv3 + NFSv4.2)");
    }
    // Use a short accept timeout so the shutdown flag is checked
    // promptly. Without this `incoming()` blocks forever.
    let _ = listener.set_nonblocking(true);

    loop {
        if let Some(ref s) = shutdown {
            if s.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("NFS server shutting down");
                return;
            }
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                let ctx = Arc::clone(&ctx);
                let tls = tls.clone();
                thread::spawn(move || {
                    let _ = stream.set_nonblocking(false);
                    if let Err(e) = enable_tcp_keepalive(&stream) {
                        tracing::debug!(
                            error = %e,
                            peer = %peer,
                            "TCP keep-alive setup failed (RFC 9289 §4.2)"
                        );
                    }
                    if let Some(tls_cfg) = tls {
                        match rustls::ServerConnection::new(tls_cfg) {
                            Ok(conn) => {
                                let tls_stream = rustls::StreamOwned::new(conn, stream);
                                if let Err(e) = handle_connection(tls_stream, ctx) {
                                    tracing::debug!(error = %e, peer = %peer, "NFS-over-TLS connection ended");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, peer = %peer, "TLS server-conn init failed");
                            }
                        }
                    } else if let Err(e) = handle_connection(stream, ctx) {
                        tracing::debug!(error = %e, peer = %peer, "NFS plaintext connection ended");
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No pending connection; sleep briefly and re-check shutdown.
                thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                tracing::error!(error = %e, "NFS accept error");
            }
        }
    }
}

/// Handle a connection — peek at the first RPC to determine version,
/// then delegate to v3 or v4 handler for the rest.
///
/// Generic over the stream type so the same dispatcher serves both
/// raw `TcpStream` (plaintext fallback) and `rustls::StreamOwned`
/// (TLS default).
fn handle_connection<G: GatewayOps, S: io::Read + io::Write>(
    mut stream: S,
    ctx: Arc<NfsContext<G>>,
) -> io::Result<()> {
    // Read first message to determine program + version.
    let first_msg = read_rm_message(&mut stream)?;
    let mut reader = XdrReader::new(&first_msg);
    let header = RpcCallHeader::decode(&mut reader)?;
    tracing::debug!(
        program = header.program,
        version = header.version,
        procedure = header.procedure,
        "NFS dispatch first message"
    );

    // MOUNT3 (program 100005) shares port 2049 with NFS when the
    // client uses `mountport=2049,mountproto=tcp` — RFC 1813 Appendix
    // I doesn't reserve a port. The standard portmapper-driven mount
    // discovers MOUNT's port via RPCBIND, which kiseki doesn't run;
    // co-locating MOUNT on 2049 is the documented Phase 15c.5
    // dispatch path.
    if header.program == crate::nfs3_mount::MOUNT3_PROGRAM {
        let reply = crate::nfs3_mount::handle_mount3_message(&header, &first_msg, &ctx);
        write_rm_message(&mut stream, &reply)?;
        return crate::nfs3_mount::handle_mount3_connection(stream, ctx);
    }

    if header.version == 4 {
        // NFSv4 — process first COMPOUND, then continue with v4 handler.
        let sessions = Arc::clone(&ctx.sessions);
        let reply =
            crate::nfs4_server::handle_nfs4_first_compound(&header, &first_msg, &ctx, &sessions);
        write_rm_message(&mut stream, &reply)?;
        handle_nfs4_connection(stream, ctx, sessions)
    } else {
        // NFSv3 (or unknown — v3 handler returns PROG_MISMATCH for wrong versions).
        let reply = crate::nfs3_server::handle_nfs3_first_message(&header, &first_msg, &ctx);
        write_rm_message(&mut stream, &reply)?;
        handle_nfs3_connection(stream, ctx)
    }
}
