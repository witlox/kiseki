//! NFS TCP server — listens on port 2049, routes to NFSv3 or NFSv4.2.
//!
//! Both versions share the same port. The ONC RPC version field in
//! the first call determines which dispatcher handles the connection.
//! NFSv3 = version 3, NFSv4.x = version 4.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use kiseki_common::ids::{NamespaceId, OrgId};

use crate::nfs::NfsGateway;
use crate::nfs3_server::handle_nfs3_connection;
use crate::nfs4_server::{handle_nfs4_connection, SessionManager};
use crate::nfs_ops::NfsContext;
use crate::nfs_xdr::{read_rm_message, write_rm_message, RpcCallHeader, XdrReader};
use crate::ops::GatewayOps;

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
    let ctx = Arc::new(NfsContext::with_storage_nodes(
        gateway,
        tenant_id,
        namespace_id,
        storage_nodes,
    ));
    let sessions = Arc::new(SessionManager::new());

    let listener = TcpListener::bind(addr).unwrap_or_else(|e| {
        tracing::error!(addr = %addr, error = %e, "NFS bind failed");
        std::process::exit(1);
    });

    tracing::info!(addr = %addr, "NFS server listening (NFSv3 + NFSv4.2)");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "NFS accept error");
                continue;
            }
        };

        let ctx = Arc::clone(&ctx);
        let sessions = Arc::clone(&sessions);
        thread::spawn(move || {
            if let Err(e) = handle_connection(stream, ctx, sessions) {
                tracing::error!(error = %e, "NFS connection error");
            }
        });
    }
}

/// Handle a connection — peek at the first RPC to determine version,
/// then delegate to v3 or v4 handler for the rest.
fn handle_connection<G: GatewayOps>(
    mut stream: TcpStream,
    ctx: Arc<NfsContext<G>>,
    sessions: Arc<SessionManager>,
) -> io::Result<()> {
    // Read first message to determine version.
    let first_msg = read_rm_message(&mut stream)?;
    let mut reader = XdrReader::new(&first_msg);
    let header = RpcCallHeader::decode(&mut reader)?;

    if header.version == 4 {
        // NFSv4 — process first COMPOUND, then continue with v4 handler.
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
