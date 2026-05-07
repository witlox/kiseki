//! NFS TCP server — listens on port 2049, routes to NFSv3 or NFSv4.2.
//!
//! Both versions share the same port. The ONC RPC version field in
//! the first call determines which dispatcher handles the connection.
//! NFSv3 = version 3, NFSv4.x = version 4.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use kiseki_common::ids::{NamespaceId, OrgId};
use rustls::ServerConfig;

use crate::nfs::NfsGateway;
use crate::nfs3_server::handle_nfs3_connection;
use crate::nfs4_server::handle_nfs4_connection;
use crate::nfs_ops::NfsContext;
use crate::nfs_xdr::{RpcCallHeader, XdrReader};
use crate::ops::GatewayOps;

/// RFC 9289 §4.2 — recommended keep-alive cadence on a long-lived
/// NFS-over-TLS session. The 60-second interval is the upper bound
/// before NAT/firewall idle-timeouts can sever the TLS session in
/// typical deployments.
///
/// Currently informational; the post-`bd56236` async-native server
/// stack relies on tokio's read/write timeouts rather than
/// kernel-level `SO_KEEPALIVE`. Kept as a documented constant so a
/// future re-introduction matches the RFC cadence.
pub const RFC9289_KEEPALIVE_INTERVAL_SECS: u64 = 60;

/// Start the NFS TCP server supporting both NFSv3 and NFSv4.2.
///
/// Spawns a thread per connection. The first RPC call determines the
/// version for that connection.
pub async fn run_nfs_server<G: GatewayOps + Send + Sync + 'static>(
    addr: SocketAddr,
    gateway: NfsGateway<G>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
) {
    run_nfs_server_with_peers(addr, gateway, tenant_id, namespace_id, Vec::new()).await;
}

/// Start the NFS server with pNFS storage node addresses for layout delegation.
pub async fn run_nfs_server_with_peers<G: GatewayOps + Send + Sync + 'static>(
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
    )
    .await;
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
pub async fn serve_nfs_listener<G: GatewayOps + Send + Sync + 'static>(
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
    )
    .await;
}

/// Phase 15c.4 — same as `serve_nfs_listener` plus an optional
/// production `MdsLayoutManager`. When threaded through, the kernel's
/// LAYOUTGET path returns Flex Files layouts pointing at real DS
/// endpoints instead of the legacy FILES-layout stub.
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub async fn serve_nfs_listener_with_mgr<G: GatewayOps + Send + Sync + 'static>(
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
    // Convert to tokio listener for async accept. The std listener
    // was set_nonblocking(true) + busy-looped before; tokio's
    // listener gives us proper readiness notification + cooperative
    // shutdown via the cancellation token check between accepts.
    let _ = listener.set_nonblocking(true);
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "NFS listener: std → tokio conversion failed");
            return;
        }
    };

    loop {
        if let Some(ref s) = shutdown {
            if s.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("NFS server shutting down");
                return;
            }
        }
        // Accept with a 50ms cancellation-aware timeout so the
        // shutdown flag is polled promptly without busy-looping.
        let accepted =
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept()).await;
        let (stream, peer) = match accepted {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "NFS accept error");
                continue;
            }
            Err(_) => continue, // timeout — re-check shutdown.
        };

        let ctx = Arc::clone(&ctx);
        let tls = tls.clone();
        // Spawn a tokio task per connection — no OS thread per
        // connection, no block_on per request. Native-equivalent
        // scheduling for NFS handlers.
        tokio::spawn(async move {
            // Disable Nagle's algorithm — same rationale as the
            // pre-async server: NFS RPC is request/reply with many
            // sub-MSS replies, Nagle + Linux's 40 ms delayed-ACK
            // would cap throughput around 0.5-7 MB/s.
            let _ = stream.set_nodelay(true);
            if let Some(tls_cfg) = tls {
                let acceptor = tokio_rustls::TlsAcceptor::from(tls_cfg);
                match acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        if let Err(e) = handle_connection(tls_stream, ctx).await {
                            tracing::debug!(error = %e, peer = %peer, "NFS-over-TLS connection ended");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, peer = %peer, "TLS handshake failed");
                    }
                }
            } else if let Err(e) = handle_connection(stream, ctx).await {
                tracing::debug!(error = %e, peer = %peer, "NFS plaintext connection ended");
            }
        });
    }
}

/// Handle a connection — peek at the first RPC to determine version,
/// then delegate to v3 or v4 handler for the rest. Async-native.
async fn handle_connection<G: GatewayOps, S>(
    mut stream: S,
    ctx: Arc<NfsContext<G>>,
) -> io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Read first message to determine program + version.
    let first_msg = crate::nfs_xdr::read_rm_message_async(&mut stream).await?;
    let mut reader = XdrReader::new(&first_msg);
    let header = RpcCallHeader::decode(&mut reader)?;
    tracing::debug!(
        program = header.program,
        version = header.version,
        procedure = header.procedure,
        "NFS dispatch first message"
    );

    if header.program == crate::nfs3_mount::MOUNT3_PROGRAM {
        let reply = crate::nfs3_mount::handle_mount3_message(&header, &first_msg, &ctx);
        crate::nfs_xdr::write_rm_message_async(&mut stream, &reply).await?;
        return crate::nfs3_mount::handle_mount3_connection(stream, ctx).await;
    }

    if header.version == 4 {
        // NFSv4 — process first COMPOUND, then continue with v4 handler.
        let sessions = Arc::clone(&ctx.sessions);
        let reply =
            crate::nfs4_server::handle_nfs4_first_compound(&header, &first_msg, &ctx, &sessions)
                .await;
        crate::nfs_xdr::write_rm_message_async(&mut stream, &reply).await?;
        handle_nfs4_connection(stream, ctx, sessions).await
    } else {
        // NFSv3 (or unknown — v3 handler returns PROG_MISMATCH for wrong versions).
        let reply = crate::nfs3_server::handle_nfs3_first_message(&header, &first_msg, &ctx).await;
        crate::nfs_xdr::write_rm_message_async(&mut stream, &reply).await?;
        handle_nfs3_connection(stream, ctx).await
    }
}
