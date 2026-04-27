//! MOUNT v3 protocol — RFC 1813 Appendix I.
//!
//! Linux NFSv3 client cannot mount without the MOUNT protocol (program
//! 100005, version 3): MNT3 returns the export's root file handle that
//! every subsequent NFSv3 op references. Without MOUNT, `mount -t nfs
//! -o vers=3` fails before issuing a single NFSv3 RPC.
//!
//! Kiseki's NFSv3 wire-level handler (`nfs3_server`) implements the
//! NFSv3 protocol on port 2049 but historically lacked MOUNT entirely
//! — clients hit `Protocol not supported`. This module fills that gap
//! with a minimal MOUNT3 stub: NULL + MNT (the only two procs Linux
//! actually invokes for an `-o nolock` mount).
//!
//! Program: 100005, Version: 3.
//!
//! Per-procedure shapes (RFC 1813 Appendix I):
//!
//!   MOUNTPROC3_NULL    = 0  args: void               reply: void
//!   MOUNTPROC3_MNT     = 1  args: dirpath            reply: mountres3
//!   MOUNTPROC3_DUMP    = 2  args: void               reply: mountlist
//!   MOUNTPROC3_UMNT    = 3  args: dirpath            reply: void
//!   MOUNTPROC3_UMNTALL = 4  args: void               reply: void
//!   MOUNTPROC3_EXPORT  = 5  args: void               reply: exportlist
//!
//! Linux invokes MOUNT3 on the same TCP port as NFS3 when the client
//! is told `mountport=2049,mountproto=tcp` (our test uses this; the
//! standard portmapper-driven mount needs RPCBIND which kiseki also
//! doesn't run).

use std::io;
use std::sync::Arc;

use crate::nfs_ops::NfsContext;
use crate::nfs_xdr::{
    encode_reply_accepted, read_rm_message, write_rm_message, RpcCallHeader, XdrReader, XdrWriter,
};
use crate::ops::GatewayOps;

/// MOUNT3 program number per IANA RPC program registry.
pub const MOUNT3_PROGRAM: u32 = 100_005;
/// MOUNT3 version per RFC 1813 Appendix I.
pub const MOUNT3_VERSION: u32 = 3;

/// MOUNT3 procedure numbers (RFC 1813 Appendix I).
pub mod proc {
    pub const NULL: u32 = 0;
    pub const MNT: u32 = 1;
    pub const DUMP: u32 = 2;
    pub const UMNT: u32 = 3;
    pub const UMNTALL: u32 = 4;
    pub const EXPORT: u32 = 5;
}

/// `mountstat3` per RFC 1813 Appendix I.
pub mod mountstat3 {
    /// Mount succeeded.
    pub const MNT3_OK: u32 = 0;
    /// Permission denied.
    pub const MNT3ERR_PERM: u32 = 1;
    /// No such file or directory (export does not exist).
    pub const MNT3ERR_NOENT: u32 = 2;
    /// I/O error.
    pub const MNT3ERR_IO: u32 = 5;
    /// Access denied.
    pub const MNT3ERR_ACCES: u32 = 13;
    /// Not a directory.
    pub const MNT3ERR_NOTDIR: u32 = 20;
    /// Invalid argument (export name malformed).
    pub const MNT3ERR_INVAL: u32 = 22;
    /// MOUNT protocol minor version not supported.
    pub const MNT3ERR_NAMETOOLONG: u32 = 63;
    /// Server is not exporting the path.
    pub const MNT3ERR_NOTSUPP: u32 = 10004;
    /// Server fault (catch-all).
    pub const MNT3ERR_SERVERFAULT: u32 = 10006;
}

/// AUTH_NONE = 0, AUTH_SYS = 1 per RFC 5531 §8. Kiseki accepts
/// AUTH_SYS for compatibility with the standard Linux NFSv3 client;
/// stronger auth (AUTH_NONE rejected by Linux for MNT) is wired
/// elsewhere via TLS.
const AUTH_SYS: u32 = 1;

/// Process a single MOUNT3 message. Returns the reply bytes.
pub fn handle_mount3_message<G: GatewayOps>(
    header: &RpcCallHeader,
    raw_msg: &[u8],
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut reader = XdrReader::new(raw_msg);
    let _ = RpcCallHeader::decode(&mut reader);
    dispatch_mount3(header, &mut reader, ctx)
}

/// Long-lived MOUNT3 connection handler. Some Linux clients keep the
/// MOUNT TCP socket open after MNT (for UMNT later), so we loop the
/// way the NFSv3/v4 dispatchers do.
pub fn handle_mount3_connection<G: GatewayOps, S: io::Read + io::Write>(
    mut stream: S,
    ctx: Arc<NfsContext<G>>,
) -> io::Result<()> {
    loop {
        let msg = match read_rm_message(&mut stream) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };

        let mut reader = XdrReader::new(&msg);
        let header = RpcCallHeader::decode(&mut reader)?;

        if header.program != MOUNT3_PROGRAM || header.version != MOUNT3_VERSION {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 2); // PROG_MISMATCH
            w.write_u32(MOUNT3_VERSION); // low
            w.write_u32(MOUNT3_VERSION); // high
            write_rm_message(&mut stream, &w.into_bytes())?;
            continue;
        }

        let reply = dispatch_mount3(&header, &mut reader, &ctx);
        write_rm_message(&mut stream, &reply)?;
    }
}

fn dispatch_mount3<G: GatewayOps>(
    header: &RpcCallHeader,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    tracing::debug!(
        xid = header.xid,
        procedure = header.procedure,
        "MOUNT3 dispatch"
    );
    match header.procedure {
        proc::NULL => reply_null(header.xid),
        proc::MNT => reply_mnt(header.xid, reader, ctx),
        proc::UMNT => reply_umnt(header.xid),
        proc::UMNTALL => reply_umntall(header.xid),
        proc::DUMP | proc::EXPORT => reply_empty_list(header.xid),
        _ => {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 3); // PROC_UNAVAIL
            w.into_bytes()
        }
    }
}

fn reply_null(xid: u32) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0); // SUCCESS, empty body
    w.into_bytes()
}

/// MOUNTPROC3_MNT — args = `dirpath` (utf8 string), reply = `mountres3`.
///
/// `mountres3`:
///
///   ```ignore
///   union mountres3 switch (mountstat3 fhs_status) {
///   case MNT3_OK:
///       mountres3_ok mountinfo;
///   default:
///       void;
///   };
///   struct mountres3_ok {
///       fhandle3   fhandle;        /* nfs_fh3 — opaque<NFS3_FHSIZE=64> */
///       int        auth_flavors<>; /* sec flavors the export accepts */
///   };
///   ```
fn reply_mnt<G: GatewayOps>(xid: u32, reader: &mut XdrReader<'_>, ctx: &NfsContext<G>) -> Vec<u8> {
    let dirpath = reader.read_string().unwrap_or_default();
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0); // SUCCESS

    // Kiseki exports a single namespace named "default" (matches the
    // NFSv4 PUTROOTFH+LOOKUP("default") path). Accept both "/default"
    // and "default" (Linux NFS client sends "/default").
    let trimmed = dirpath.trim_start_matches('/');
    if trimmed != "default" {
        w.write_u32(mountstat3::MNT3ERR_NOENT);
        return w.into_bytes();
    }

    // The namespace root file handle (32 bytes in kiseki's NFSv4
    // handle layout — fits within the NFS3_FHSIZE = 64 limit).
    let root_fh = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);

    w.write_u32(mountstat3::MNT3_OK);
    // fhandle3: opaque<NFS3_FHSIZE> — variable length up to 64 bytes.
    w.write_opaque(&root_fh);
    // auth_flavors<> — list. Linux accepts the mount when AUTH_SYS
    // is offered. We don't enforce any auth in MOUNT; the namespace
    // ACL gate happens at NFSv3 op time.
    w.write_u32(1); // count
    w.write_u32(AUTH_SYS);
    w.into_bytes()
}

fn reply_umnt(xid: u32) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0); // SUCCESS — UMNT is a notification, void reply
    w.into_bytes()
}

fn reply_umntall(xid: u32) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);
    w.into_bytes()
}

fn reply_empty_list(xid: u32) -> Vec<u8> {
    // DUMP and EXPORT both return linked lists; we return an empty
    // list (single bool=false). Linux clients tolerate this for
    // -o nolock,bg mounts.
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);
    w.write_bool(false); // no entries
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_gateway::InMemoryGateway;
    use crate::nfs::NfsGateway;
    use crate::nfs_ops::NfsContext;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_composition::namespace::Namespace;
    use kiseki_crypto::keys::SystemMasterKey;

    fn test_ctx() -> NfsContext<InMemoryGateway> {
        let tenant = OrgId(uuid::Uuid::nil());
        let ns = NamespaceId(uuid::Uuid::from_u128(1));
        let mut store = CompositionStore::new();
        store.add_namespace(Namespace {
            id: ns,
            tenant_id: tenant,
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        });
        let chunks = ChunkStore::new();
        let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let gw = InMemoryGateway::new(store, Box::new(chunks), master_key);
        let nfs_gw = NfsGateway::new(gw);
        NfsContext::new(nfs_gw, tenant, ns)
    }

    fn build_call(xid: u32, prog: u32, ver: u32, procedure: u32, body: &[u8]) -> Vec<u8> {
        let mut w = XdrWriter::new();
        w.write_u32(xid);
        w.write_u32(0); // CALL
        w.write_u32(2); // RPC v2
        w.write_u32(prog);
        w.write_u32(ver);
        w.write_u32(procedure);
        // AUTH_NONE creds + verifier.
        w.write_u32(0);
        w.write_opaque(&[]);
        w.write_u32(0);
        w.write_opaque(&[]);
        let mut buf = w.into_bytes();
        buf.extend_from_slice(body);
        buf
    }

    fn header(xid: u32, procedure: u32) -> RpcCallHeader {
        RpcCallHeader {
            xid,
            program: MOUNT3_PROGRAM,
            version: MOUNT3_VERSION,
            procedure,
        }
    }

    /// RFC 1813 Appendix I — MOUNTPROC3_NULL is the empty ping. Reply
    /// MUST be RPC accept_stat=SUCCESS (0) with an empty body. Linux
    /// `mount.nfs -o vers=3` issues this before MNT to verify the
    /// MOUNT service is alive.
    #[test]
    fn rfc1813_appendix_i_mount3_null_returns_empty_accept_ok() {
        let ctx = test_ctx();
        let raw = build_call(0xCAFE, MOUNT3_PROGRAM, MOUNT3_VERSION, proc::NULL, &[]);
        let reply = handle_mount3_message(&header(0xCAFE, proc::NULL), &raw, &ctx);

        let mut r = XdrReader::new(&reply);
        let xid = r.read_u32().unwrap();
        assert_eq!(xid, 0xCAFE, "xid echoed");
        let msg_type = r.read_u32().unwrap();
        assert_eq!(msg_type, 1, "REPLY = 1");
        let reply_stat = r.read_u32().unwrap();
        assert_eq!(reply_stat, 0, "MSG_ACCEPTED = 0");
        let _vf = r.read_u32().unwrap();
        let _vlen = r.read_u32().unwrap();
        let accept_stat = r.read_u32().unwrap();
        assert_eq!(accept_stat, 0, "SUCCESS");
        assert_eq!(
            r.remaining(),
            0,
            "RFC 1813 Appendix I: NULL reply body MUST be empty; got {} bytes",
            r.remaining()
        );
    }

    /// RFC 1813 Appendix I — MOUNTPROC3_MNT("default") returns the
    /// namespace root file handle so subsequent NFSv3 ops can reference
    /// the mount point. AUTH_SYS (1) advertised in auth_flavors so
    /// Linux's `mount.nfs` accepts the export.
    #[test]
    fn rfc1813_appendix_i_mount3_mnt_default_returns_root_fh() {
        let ctx = test_ctx();
        // args: dirpath = "/default" (Linux client passes the full path)
        let mut body = XdrWriter::new();
        body.write_string("/default");
        let body_bytes = body.into_bytes();
        let raw = build_call(
            0x1001,
            MOUNT3_PROGRAM,
            MOUNT3_VERSION,
            proc::MNT,
            &body_bytes,
        );

        let reply = handle_mount3_message(&header(0x1001, proc::MNT), &raw, &ctx);

        let mut r = XdrReader::new(&reply);
        let _xid = r.read_u32().unwrap();
        let _msg_type = r.read_u32().unwrap();
        let _reply_stat = r.read_u32().unwrap();
        let _vf = r.read_u32().unwrap();
        let _vlen = r.read_u32().unwrap();
        let accept_stat = r.read_u32().unwrap();
        assert_eq!(accept_stat, 0, "RFC 5531: SUCCESS");
        let fhs_status = r.read_u32().unwrap();
        assert_eq!(
            fhs_status,
            mountstat3::MNT3_OK,
            "RFC 1813 Appendix I: MNT MUST succeed for the canonical \
             'default' export name"
        );
        let fh = r.read_opaque().unwrap();
        assert_eq!(
            fh.len(),
            32,
            "kiseki file handle is 32 bytes (NFSv4-style, fits within NFS3_FHSIZE=64)"
        );
        let auth_count = r.read_u32().unwrap();
        assert_eq!(auth_count, 1, "exactly one auth flavor advertised");
        let auth = r.read_u32().unwrap();
        assert_eq!(auth, AUTH_SYS, "AUTH_SYS (1) advertised for Linux compat");
    }

    /// RFC 1813 Appendix I — MNT for an unknown export name MUST yield
    /// `MNT3ERR_NOENT` (2). Linux surfaces this as `mount.nfs: access
    /// denied by server while mounting`.
    #[test]
    fn rfc1813_appendix_i_mount3_mnt_unknown_export_returns_noent() {
        let ctx = test_ctx();
        let mut body = XdrWriter::new();
        body.write_string("/no-such-export");
        let body_bytes = body.into_bytes();
        let raw = build_call(
            0x1002,
            MOUNT3_PROGRAM,
            MOUNT3_VERSION,
            proc::MNT,
            &body_bytes,
        );

        let reply = handle_mount3_message(&header(0x1002, proc::MNT), &raw, &ctx);
        let mut r = XdrReader::new(&reply);
        for _ in 0..6 {
            let _ = r.read_u32();
        }
        let fhs_status = r.read_u32().unwrap();
        assert_eq!(
            fhs_status,
            mountstat3::MNT3ERR_NOENT,
            "RFC 1813 Appendix I: unknown export MUST yield MNT3ERR_NOENT"
        );
    }
}
