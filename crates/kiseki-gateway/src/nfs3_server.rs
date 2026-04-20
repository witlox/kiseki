//! NFSv3 ONC RPC server (RFC 1813).
//!
//! Listens on TCP, decodes Record Marking framed ONC RPC calls,
//! dispatches to shared `NfsContext` operations, encodes replies.
//!
//! Program: 100003, Version: 3.

use std::io;
use std::net::TcpStream;
use std::sync::Arc;

use crate::nfs_ops::NfsContext;
use crate::nfs_xdr::{
    encode_reply_accepted, read_rm_message, write_rm_message, RpcCallHeader, XdrReader, XdrWriter,
};
use crate::ops::GatewayOps;

/// NFS3 program number.
const NFS3_PROGRAM: u32 = 100003;
/// NFS3 version.
const NFS3_VERSION: u32 = 3;

/// NFS3 procedure numbers.
mod proc {
    pub const NULL: u32 = 0;
    pub const GETATTR: u32 = 1;
    pub const LOOKUP: u32 = 3;
    pub const READ: u32 = 6;
    pub const WRITE: u32 = 7;
    pub const CREATE: u32 = 8;
    pub const READDIR: u32 = 16;
}

/// NFS3 status codes.
mod status {
    pub const NFS3_OK: u32 = 0;
    pub const NFS3ERR_NOENT: u32 = 2;
    pub const NFS3ERR_IO: u32 = 5;
    pub const NFS3ERR_BADHANDLE: u32 = 10001;
}

/// Process a single already-decoded NFS3 message and return the reply bytes.
pub fn handle_nfs3_first_message<G: GatewayOps>(
    header: &RpcCallHeader,
    raw_msg: &[u8],
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut reader = XdrReader::new(raw_msg);
    // Skip past the RPC header (already decoded by caller).
    let _ = RpcCallHeader::decode(&mut reader);
    dispatch_nfs3(header, &mut reader, ctx)
}

/// Handle one NFS3 TCP connection (after the first message).
pub fn handle_nfs3_connection<G: GatewayOps>(
    mut stream: TcpStream,
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

        if header.program != NFS3_PROGRAM || header.version != NFS3_VERSION {
            // Program/version mismatch — reply with PROG_MISMATCH.
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 2); // PROG_MISMATCH
            w.write_u32(NFS3_VERSION); // low
            w.write_u32(NFS3_VERSION); // high
            write_rm_message(&mut stream, &w.into_bytes())?;
            continue;
        }

        let reply = dispatch_nfs3(&header, &mut reader, &ctx);
        write_rm_message(&mut stream, &reply)?;
    }
}

fn dispatch_nfs3<G: GatewayOps>(
    header: &RpcCallHeader,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    match header.procedure {
        proc::NULL => reply_null(header.xid),
        proc::GETATTR => reply_getattr(header.xid, reader, ctx),
        proc::LOOKUP => reply_lookup(header.xid, reader, ctx),
        proc::READ => reply_read(header.xid, reader, ctx),
        proc::WRITE => reply_write(header.xid, reader, ctx),
        proc::CREATE => reply_create(header.xid, reader, ctx),
        proc::READDIR => reply_readdir(header.xid, reader, ctx),
        _ => {
            // Unsupported procedure — reply PROC_UNAVAIL.
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 3); // PROC_UNAVAIL
            w.into_bytes()
        }
    }
}

fn reply_null(xid: u32) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0); // SUCCESS
    w.into_bytes()
}

fn reply_getattr<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let fh = match reader.read_opaque() {
        Ok(fh) if fh.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&fh);
            arr
        }
        _ => {
            w.write_u32(status::NFS3ERR_BADHANDLE);
            return w.into_bytes();
        }
    };

    match ctx.getattr(&fh) {
        Ok(attrs) => {
            w.write_u32(status::NFS3_OK);
            // fattr3: type, mode, nlink, uid, gid, size, used, rdev, fsid, fileid
            let ftype = match attrs.file_type {
                crate::nfs_ops::FileType::Regular => 1u32,
                crate::nfs_ops::FileType::Directory => 2u32,
            };
            w.write_u32(ftype);
            w.write_u32(attrs.mode);
            w.write_u32(attrs.nlink);
            w.write_u32(attrs.uid);
            w.write_u32(attrs.gid);
            w.write_u64(attrs.size); // size
            w.write_u64(attrs.size); // used
            w.write_u64(0); // rdev
            w.write_u64(1); // fsid
            w.write_u64(attrs.fileid);
            // atime, mtime, ctime (3 x nfstime3 = 3 x 2 x u32)
            for _ in 0..6 {
                w.write_u32(0);
            }
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_NOENT);
        }
    }

    w.into_bytes()
}

fn reply_read<G: GatewayOps>(xid: u32, reader: &mut XdrReader<'_>, ctx: &NfsContext<G>) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let fh = match reader.read_opaque() {
        Ok(fh) if fh.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&fh);
            arr
        }
        _ => {
            w.write_u32(status::NFS3ERR_BADHANDLE);
            return w.into_bytes();
        }
    };

    let offset = reader.read_u64().unwrap_or(0);
    let count = reader.read_u32().unwrap_or(0);

    match ctx.read(&fh, offset, count) {
        Ok(resp) => {
            w.write_u32(status::NFS3_OK);
            // post-op attributes (false = not present)
            w.write_bool(false);
            w.write_u32(resp.data.len() as u32); // count
            w.write_bool(resp.eof);
            w.write_opaque(&resp.data);
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_IO);
        }
    }

    w.into_bytes()
}

fn reply_write<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    // Skip file handle (we create a new composition for each write).
    let _fh = reader.read_opaque().unwrap_or_default();
    let _offset = reader.read_u64().unwrap_or(0);
    let _count = reader.read_u32().unwrap_or(0);
    let _stable = reader.read_u32().unwrap_or(0); // FILE_SYNC=2
    let data = reader.read_opaque().unwrap_or_default();

    match ctx.write(data) {
        Ok((_new_fh, resp)) => {
            w.write_u32(status::NFS3_OK);
            // wcc_data (before + after attributes, both absent)
            w.write_bool(false); // pre-op
            w.write_bool(false); // post-op
            w.write_u32(resp.count); // count
            w.write_u32(2); // committed = FILE_SYNC
            w.write_opaque_fixed(&[0u8; 8]); // write verifier
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_IO);
        }
    }

    w.into_bytes()
}

fn reply_lookup<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let _dir_fh = reader.read_opaque().unwrap_or_default();
    let name = reader.read_string().unwrap_or_default();

    match ctx.lookup_by_name(&name) {
        Some((fh, _attrs)) => {
            w.write_u32(status::NFS3_OK);
            w.write_opaque(&fh);
            w.write_bool(false); // post-op attrs omitted
            w.write_bool(false); // dir attrs omitted
        }
        None => {
            w.write_u32(status::NFS3ERR_NOENT);
            w.write_bool(false);
        }
    }

    w.into_bytes()
}

fn reply_create<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let _dir_fh = reader.read_opaque().unwrap_or_default();
    let _name = reader.read_string().unwrap_or_default();

    match ctx.write(Vec::new()) {
        Ok((new_fh, _resp)) => {
            w.write_u32(status::NFS3_OK);
            w.write_bool(true);
            w.write_opaque(&new_fh);
            w.write_bool(false); // post-op
            w.write_bool(false); // pre wcc
            w.write_bool(false); // post wcc
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_IO);
            w.write_bool(false);
            w.write_bool(false);
        }
    }

    w.into_bytes()
}

fn reply_readdir<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let _dir_fh = reader.read_opaque().unwrap_or_default();

    w.write_u32(status::NFS3_OK);
    w.write_bool(false); // dir attrs omitted
    w.write_opaque_fixed(&[0u8; 8]); // cookieverf

    let entries = ctx.readdir();
    for (i, entry) in entries.iter().enumerate() {
        w.write_bool(true);
        w.write_u64(entry.fileid);
        w.write_string(&entry.name);
        w.write_u64((i + 1) as u64);
    }
    w.write_bool(false); // no more
    w.write_bool(true); // eof

    w.into_bytes()
}
