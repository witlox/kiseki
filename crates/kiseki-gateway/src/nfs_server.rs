//! NFS TCP server — listens on port 2049, routes to NFSv3 or NFSv4.2.
//!
//! Version is determined by the ONC RPC program version in the first
//! call. Currently only NFSv3 is implemented; NFSv4.2 COMPOUND will
//! be added in a follow-up.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread;

use kiseki_common::ids::{NamespaceId, OrgId};

use crate::nfs::NfsGateway;
use crate::nfs3_server::handle_nfs3_connection;
use crate::nfs_ops::NfsContext;
use crate::ops::GatewayOps;

/// Start the NFS TCP server on the given address.
///
/// Spawns a thread per connection (NFS is stateful per-connection).
/// Production would use async I/O; this is MVP.
pub fn run_nfs_server<G: GatewayOps + Send + Sync + 'static>(
    addr: SocketAddr,
    gateway: NfsGateway<G>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
) {
    let ctx = Arc::new(NfsContext::new(gateway, tenant_id, namespace_id));

    let listener = TcpListener::bind(addr).unwrap_or_else(|e| {
        eprintln!("NFS bind {addr}: {e}");
        std::process::exit(1);
    });

    eprintln!("  NFS server listening on {addr} (NFSv3)");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("NFS accept: {e}");
                continue;
            }
        };

        let ctx = Arc::clone(&ctx);
        thread::spawn(move || {
            if let Err(e) = handle_nfs3_connection(stream, ctx) {
                eprintln!("NFS connection error: {e}");
            }
        });
    }
}
