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

/// NFS3 procedure numbers (RFC 1813 section 3.3).
mod proc {
    pub const NULL: u32 = 0;
    pub const GETATTR: u32 = 1;
    pub const SETATTR: u32 = 2;
    pub const LOOKUP: u32 = 3;
    pub const ACCESS: u32 = 4;
    pub const READLINK: u32 = 5;
    pub const READ: u32 = 6;
    pub const WRITE: u32 = 7;
    pub const CREATE: u32 = 8;
    pub const MKDIR: u32 = 9;
    pub const SYMLINK: u32 = 10;
    pub const MKNOD: u32 = 11;
    pub const REMOVE: u32 = 12;
    pub const RMDIR: u32 = 13;
    pub const RENAME: u32 = 14;
    pub const LINK: u32 = 15;
    pub const READDIR: u32 = 16;
    pub const READDIRPLUS: u32 = 17;
    pub const PATHCONF: u32 = 19;
    pub const FSINFO: u32 = 20;
    pub const FSSTAT: u32 = 21;
    pub const COMMIT: u32 = 22;
}

/// NFS3 status codes.
#[allow(dead_code)]
mod status {
    pub const NFS3_OK: u32 = 0;
    pub const NFS3ERR_NOENT: u32 = 2;
    pub const NFS3ERR_IO: u32 = 5;
    pub const NFS3ERR_EXIST: u32 = 17;
    pub const NFS3ERR_NOTDIR: u32 = 20;
    pub const NFS3ERR_NOTEMPTY: u32 = 66;
    pub const NFS3ERR_NOTSUPP: u32 = 10004;
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
        proc::SETATTR => reply_setattr(header.xid, reader, ctx),
        proc::LOOKUP => reply_lookup(header.xid, reader, ctx),
        proc::ACCESS => reply_access(header.xid, reader, ctx),
        proc::READLINK => reply_readlink(header.xid, reader, ctx),
        proc::READ => reply_read(header.xid, reader, ctx),
        proc::WRITE => reply_write(header.xid, reader, ctx),
        proc::CREATE => reply_create(header.xid, reader, ctx),
        proc::MKDIR => reply_mkdir(header.xid, reader, ctx),
        proc::SYMLINK => reply_symlink(header.xid, reader, ctx),
        proc::MKNOD => reply_mknod(header.xid),
        proc::REMOVE => reply_remove(header.xid, reader, ctx),
        proc::RMDIR => reply_rmdir(header.xid, reader, ctx),
        proc::RENAME => reply_rename(header.xid, reader, ctx),
        proc::LINK => reply_link(header.xid, reader, ctx),
        proc::READDIR => reply_readdir(header.xid, reader, ctx),
        proc::READDIRPLUS => reply_readdirplus(header.xid, reader, ctx),
        proc::PATHCONF => reply_pathconf(header.xid),
        proc::FSSTAT => reply_fsstat(header.xid, ctx),
        proc::FSINFO => reply_fsinfo(header.xid, ctx),
        proc::COMMIT => reply_commit(header.xid),
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
    let name = reader.read_string().unwrap_or_default();

    match ctx.write_named(&name, Vec::new()) {
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

fn reply_remove<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let _dir_fh = reader.read_opaque().unwrap_or_default();
    let name = reader.read_string().unwrap_or_default();

    match ctx.remove_file(&name) {
        Ok(()) => {
            w.write_u32(status::NFS3_OK);
            w.write_bool(false); // pre wcc
            w.write_bool(false); // post wcc
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_NOENT);
            w.write_bool(false);
            w.write_bool(false);
        }
    }

    w.into_bytes()
}

fn reply_rename<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let _from_dir = reader.read_opaque().unwrap_or_default();
    let from_name = reader.read_string().unwrap_or_default();
    let _to_dir = reader.read_opaque().unwrap_or_default();
    let to_name = reader.read_string().unwrap_or_default();

    match ctx.rename_file(&from_name, &to_name) {
        Ok(()) => {
            w.write_u32(status::NFS3_OK);
            // from dir wcc + to dir wcc (both absent)
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_NOENT);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
        }
    }

    w.into_bytes()
}

fn reply_fsstat<G: GatewayOps>(xid: u32, _ctx: &NfsContext<G>) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    w.write_u32(status::NFS3_OK);
    w.write_bool(false); // post-op attrs
                         // tbytes, fbytes, abytes (total, free, available)
    w.write_u64(1_000_000_000_000); // 1TB total
    w.write_u64(500_000_000_000); // 500GB free
    w.write_u64(500_000_000_000); // 500GB available
                                  // tfiles, ffiles, afiles
    w.write_u64(1_000_000);
    w.write_u64(500_000);
    w.write_u64(500_000);
    // invarsec
    w.write_u32(0);

    w.into_bytes()
}

fn reply_fsinfo<G: GatewayOps>(xid: u32, _ctx: &NfsContext<G>) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    w.write_u32(status::NFS3_OK);
    w.write_bool(false); // post-op attrs
                         // rtmax, rtpref, rtmult (read transfer)
    w.write_u32(1_048_576); // 1MB max read
    w.write_u32(65536); // 64KB preferred
    w.write_u32(4096); // 4KB multiple
                       // wtmax, wtpref, wtmult (write transfer)
    w.write_u32(1_048_576);
    w.write_u32(65536);
    w.write_u32(4096);
    // dtpref (readdir)
    w.write_u32(65536);
    // maxfilesize
    w.write_u64(u64::MAX);
    // time_delta (seconds, nseconds)
    w.write_u32(0);
    w.write_u32(1);
    // properties
    w.write_u32(0x001b); // FSF_LINK | FSF_SYMLINK | FSF_HOMOGENEOUS | FSF_CANSETTIME

    w.into_bytes()
}

// --- F1: New NFS3 handlers below ---

fn reply_setattr<G: GatewayOps>(
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
            w.write_bool(false); // pre wcc
            w.write_bool(false); // post wcc
            return w.into_bytes();
        }
    };

    // sattr3: each field is optional (bool + value).
    // Read mode if present.
    let set_mode = reader.read_bool().unwrap_or(false);
    let mode = if set_mode {
        Some(reader.read_u32().unwrap_or(0o644))
    } else {
        None
    };
    // Skip uid, gid, size, atime, mtime (all optional).
    for _ in 0..5 {
        if reader.read_bool().unwrap_or(false) {
            let _ = reader.read_u32(); // consume value (or time)
        }
    }
    // guard check (sattrguard3): bool + optional pre-op ctime
    if reader.read_bool().unwrap_or(false) {
        let _ = reader.read_u32(); // seconds
        let _ = reader.read_u32(); // nseconds
    }

    match ctx.setattr(&fh, mode) {
        Ok(_attrs) => {
            w.write_u32(status::NFS3_OK);
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

fn reply_access<G: GatewayOps>(
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
            w.write_bool(false); // post-op attrs
            return w.into_bytes();
        }
    };

    let requested = reader.read_u32().unwrap_or(0x3F);

    match ctx.access(&fh) {
        Ok(granted) => {
            w.write_u32(status::NFS3_OK);
            w.write_bool(false); // post-op attrs
            w.write_u32(granted & requested); // only grant what was requested
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_BADHANDLE);
            w.write_bool(false);
        }
    }

    w.into_bytes()
}

fn reply_readlink<G: GatewayOps>(
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
            w.write_bool(false); // post-op attrs
            return w.into_bytes();
        }
    };

    match ctx.readlink(&fh) {
        Ok(target) => {
            w.write_u32(status::NFS3_OK);
            w.write_bool(false); // post-op attrs
            w.write_string(&target); // nfspath3
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_IO);
            w.write_bool(false);
        }
    }

    w.into_bytes()
}

fn reply_mkdir<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let _dir_fh = reader.read_opaque().unwrap_or_default();
    let name = reader.read_string().unwrap_or_default();
    // Skip sattr3 (mode, uid, gid, size, atime, mtime — all optional).
    for _ in 0..6 {
        if reader.read_bool().unwrap_or(false) {
            let _ = reader.read_u32();
        }
    }

    match ctx.mkdir(&name) {
        Ok((new_fh, _attrs)) => {
            w.write_u32(status::NFS3_OK);
            // post_op_fh3: handle follows = true + handle
            w.write_bool(true);
            w.write_opaque(&new_fh);
            // post-op attrs
            w.write_bool(false);
            // dir wcc
            w.write_bool(false); // pre
            w.write_bool(false); // post
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_IO);
            w.write_bool(false); // pre wcc
            w.write_bool(false); // post wcc
        }
    }

    w.into_bytes()
}

fn reply_symlink<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let _dir_fh = reader.read_opaque().unwrap_or_default();
    let name = reader.read_string().unwrap_or_default();
    // Skip sattr3 (6 optional fields).
    for _ in 0..6 {
        if reader.read_bool().unwrap_or(false) {
            let _ = reader.read_u32();
        }
    }
    let target = reader.read_string().unwrap_or_default();

    match ctx.symlink(&name, &target) {
        Ok((new_fh, _attrs)) => {
            w.write_u32(status::NFS3_OK);
            w.write_bool(true);
            w.write_opaque(&new_fh);
            w.write_bool(false); // post-op attrs
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

/// MKNOD creates special device files. Kiseki does not support device files,
/// so we always return `NFS3ERR_NOTSUPP`.
fn reply_mknod(xid: u32) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);
    w.write_u32(status::NFS3ERR_NOTSUPP);
    w.write_bool(false); // pre wcc
    w.write_bool(false); // post wcc
    w.into_bytes()
}

fn reply_rmdir<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let _dir_fh = reader.read_opaque().unwrap_or_default();
    let name = reader.read_string().unwrap_or_default();

    match ctx.rmdir(&name) {
        Ok(()) => {
            w.write_u32(status::NFS3_OK);
            w.write_bool(false); // pre wcc
            w.write_bool(false); // post wcc
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_NOENT);
            w.write_bool(false);
            w.write_bool(false);
        }
    }

    w.into_bytes()
}

fn reply_link<G: GatewayOps>(xid: u32, reader: &mut XdrReader<'_>, ctx: &NfsContext<G>) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let target_fh = match reader.read_opaque() {
        Ok(fh) if fh.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&fh);
            arr
        }
        _ => {
            w.write_u32(status::NFS3ERR_BADHANDLE);
            w.write_bool(false); // post-op attrs
            w.write_bool(false); // pre wcc
            w.write_bool(false); // post wcc
            return w.into_bytes();
        }
    };

    let _dir_fh = reader.read_opaque().unwrap_or_default();
    let name = reader.read_string().unwrap_or_default();

    match ctx.link(&target_fh, &name) {
        Ok(()) => {
            w.write_u32(status::NFS3_OK);
            w.write_bool(false); // post-op file attrs
            w.write_bool(false); // pre wcc
            w.write_bool(false); // post wcc
        }
        Err(_) => {
            w.write_u32(status::NFS3ERR_IO);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
        }
    }

    w.into_bytes()
}

fn reply_readdirplus<G: GatewayOps>(
    xid: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    let _dir_fh = reader.read_opaque().unwrap_or_default();
    let _cookie = reader.read_u64().unwrap_or(0);
    let _cookieverf = reader.read_opaque_fixed(8).unwrap_or_default();
    let _dircount = reader.read_u32().unwrap_or(0);
    let _maxcount = reader.read_u32().unwrap_or(0);

    w.write_u32(status::NFS3_OK);
    w.write_bool(false); // dir attrs omitted
    w.write_opaque_fixed(&[0u8; 8]); // cookieverf

    let entries = ctx.readdir();
    for (i, entry) in entries.iter().enumerate() {
        w.write_bool(true); // entry follows
        w.write_u64(entry.fileid);
        w.write_string(&entry.name);
        w.write_u64((i + 1) as u64); // cookie
                                     // name_attributes (post_op_attr): false = not present
        w.write_bool(false);
        // name_handle (post_op_fh3): false = not present
        w.write_bool(false);
    }
    w.write_bool(false); // no more entries
    w.write_bool(true); // eof

    w.into_bytes()
}

/// PATHCONF returns static filesystem configuration.
fn reply_pathconf(xid: u32) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    w.write_u32(status::NFS3_OK);
    w.write_bool(false); // post-op attrs
    w.write_u32(1024); // linkmax
    w.write_u32(255); // name_max
    w.write_bool(true); // no_trunc
    w.write_bool(false); // chown_restricted
    w.write_bool(true); // case_insensitive = false (we say true = case preserving)
    w.write_bool(true); // case_preserving

    w.into_bytes()
}

/// COMMIT flushes pending writes to stable storage.
fn reply_commit(xid: u32) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0);

    w.write_u32(status::NFS3_OK);
    w.write_bool(false); // pre wcc
    w.write_bool(false); // post wcc
    w.write_opaque_fixed(&[0u8; 8]); // write verifier

    w.into_bytes()
}
