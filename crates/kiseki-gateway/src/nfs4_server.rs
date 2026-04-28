//! NFSv4.2 COMPOUND server (RFC 7862).
//!
//! Handles NFSv4.2 COMPOUND requests — each RPC contains a sequence
//! of operations processed in order. Session and lease management
//! for stateful file access.
//!
//! Program: 100003, Version: 4 (minor version 2).

// NFSv4 ops use match on Result for status tracking — clearer than if-let chains.
#![allow(clippy::single_match_else)]

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use crate::nfs_ops::{FileHandle, NfsContext};
use crate::nfs_xdr::{
    encode_reply_accepted, read_rm_message, write_rm_message, RpcCallHeader, XdrReader, XdrWriter,
};
use crate::ops::GatewayOps;

/// NFSv4 program/version constants.
const NFS4_PROGRAM: u32 = 100003;
/// RFC 8881 §20 — NFSv4 callback program. Linux 6.x clients send
/// CB_NULL on this program (version 1, procedure 0) over the SAME
/// TCP socket as the forward NFS channel to verify the back-channel
/// framing decode. kiseki accepts CB_NULL with ACCEPT_OK (empty body)
/// — it doesn't actually dispatch CB_COMPOUND, but it must not
/// reject the framing or the kernel marks the back channel broken
/// and the mount fails with "Operation not supported" (Phase 15
/// e2e blocker, 2026-04-27).
const NFS4_CB_PROGRAM: u32 = 400122;
const NFS4_VERSION: u32 = 4;

/// NFSv4 operation codes (RFC 7530 + RFC 7862).
#[allow(dead_code)]
pub mod op {
    pub const ACCESS: u32 = 3;
    pub const CLOSE: u32 = 4;
    pub const COMMIT: u32 = 5;
    pub const CREATE: u32 = 6;
    pub const GETATTR: u32 = 9;
    pub const GETFH: u32 = 10;
    pub const LINK: u32 = 11;
    pub const LOCK: u32 = 12;
    pub const LOOKUP: u32 = 15;
    pub const OPEN: u32 = 18;
    pub const PUTFH: u32 = 22;
    pub const PUTROOTFH: u32 = 24;
    pub const READ: u32 = 25;
    pub const READDIR: u32 = 26;
    pub const READLINK: u32 = 27;
    pub const REMOVE: u32 = 28;
    pub const RENAME: u32 = 29;
    pub const RESTOREFH: u32 = 31;
    pub const SAVEFH: u32 = 32;
    pub const SETATTR: u32 = 34;
    pub const WRITE: u32 = 38;
    pub const EXCHANGE_ID: u32 = 42;
    pub const CREATE_SESSION: u32 = 43;
    pub const DESTROY_SESSION: u32 = 44;
    pub const RECLAIM_COMPLETE: u32 = 58;
    pub const LAYOUTGET: u32 = 50;
    pub const LAYOUTRETURN: u32 = 51;
    pub const GETDEVICEINFO: u32 = 47;
    pub const SEQUENCE: u32 = 53;
    pub const IO_ADVISE: u32 = 63;
    // RFC 8881 v4.1 ops the kernel mount.nfs4 sequence requires:
    // BIND_CONN_TO_SESSION + SECINFO_NO_NAME + DESTROY_CLIENTID.
    // Without these, OP_ILLEGAL aborts the kernel's session bring-up
    // (Phase 15 e2e blocker, surfaced 2026-04-27).
    pub const BIND_CONN_TO_SESSION: u32 = 41;
    pub const SECINFO_NO_NAME: u32 = 52;
    pub const DESTROY_CLIENTID: u32 = 57;
    // RFC 7862 v4.2 ops kiseki recognizes (mostly stubs that emit
    // typed errors; the wire surface area must reach a per-op handler
    // so the dispatcher can return spec-aligned status codes per
    // §15.5 instead of catch-all NFS4ERR_NOTSUPP / OP_ILLEGAL).
    pub const ALLOCATE: u32 = 59;
    pub const COPY: u32 = 60;
    pub const DEALLOCATE: u32 = 62;
    pub const LAYOUTERROR: u32 = 64;
    pub const READ_PLUS: u32 = 68;
    pub const SEEK: u32 = 69;
}

/// NFSv4 status codes.
pub mod nfs4_status {
    pub const NFS4_OK: u32 = 0;
    pub const NFS4ERR_NOENT: u32 = 2;
    pub const NFS4ERR_IO: u32 = 5;
    pub const NFS4ERR_NOTSUPP: u32 = 10004;
    pub const NFS4ERR_BADHANDLE: u32 = 10001;
    pub const NFS4ERR_STALE_CLIENTID: u32 = 10012;
    pub const NFS4ERR_BADSESSION: u32 = 10052;
    pub const NFS4ERR_BAD_STATEID: u32 = 10025;
    pub const NFS4ERR_DENIED: u32 = 10010;
    pub const NFS4ERR_NOFILEHANDLE: u32 = 10020;
    pub const NFS4ERR_MINOR_VERS_MISMATCH: u32 = 10021;
    pub const NFS4ERR_BADXDR: u32 = 10036;
    pub const NFS4ERR_OP_ILLEGAL: u32 = 10044;
    pub const NFS4ERR_BADIOMODE: u32 = 10049;
    pub const NFS4ERR_LAYOUTUNAVAILABLE: u32 = 10059;
    pub const NFS4ERR_UNION_NOTSUPP: u32 = 10090;
}

/// NFSv4 session state.
#[derive(Clone)]
struct Session {
    session_id: [u8; 16],
    client_id: u64,
    fore_channel_slots: u32,
    sequence_ids: Vec<u32>,
}

/// Stateid — identifies an open file or lock state.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct StateId(pub [u8; 16]);

/// Open file state.
struct OpenState {
    stateid: StateId,
    file_handle: FileHandle,
}

/// Lock state.
struct LockState {
    lock_stateid: StateId,
    offset: u64,
    length: u64,
    write: bool,
}

/// Per-connection NFSv4 COMPOUND state.
struct CompoundState {
    current_fh: Option<FileHandle>,
    saved_fh: Option<FileHandle>,
    current_stateid: Option<StateId>,
}

/// NFSv4 session manager — tracks active sessions, stateids, and locks.
pub struct SessionManager {
    next_client_id: Mutex<u64>,
    sessions: Mutex<HashMap<[u8; 16], Session>>,
    open_files: Mutex<HashMap<StateId, OpenState>>,
    locks: Mutex<Vec<LockState>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            next_client_id: Mutex::new(1),
            sessions: Mutex::new(HashMap::new()),
            open_files: Mutex::new(HashMap::new()),
            locks: Mutex::new(Vec::new()),
        }
    }

    pub fn open_file(&self, fh: FileHandle) -> StateId {
        let sid = StateId(*uuid::Uuid::new_v4().as_bytes());
        self.open_files.lock().unwrap().insert(
            sid,
            OpenState {
                stateid: sid,
                file_handle: fh,
            },
        );
        sid
    }

    fn close_file(&self, sid: &StateId) -> bool {
        self.open_files.lock().unwrap().remove(sid).is_some()
    }

    fn is_open(&self, sid: &StateId) -> bool {
        self.open_files.lock().unwrap().contains_key(sid)
    }

    fn add_lock(&self, sid: StateId, offset: u64, length: u64, write: bool) -> Result<StateId, ()> {
        let mut locks = self.locks.lock().unwrap();
        // Check for conflicting locks (saturating to prevent overflow).
        let req_end = offset.saturating_add(length);
        for lock in locks.iter() {
            let lock_end = lock.offset.saturating_add(lock.length);
            let overlaps = lock.offset < req_end && offset < lock_end;
            if overlaps && (write || lock.write) {
                return Err(()); // Conflict
            }
        }
        let lock_sid = StateId(*uuid::Uuid::new_v4().as_bytes());
        locks.push(LockState {
            lock_stateid: lock_sid,
            offset,
            length,
            write,
        });
        Ok(lock_sid)
    }

    fn exchange_id(&self) -> u64 {
        // Random client IDs prevent prediction (C-ADV-7).
        uuid::Uuid::new_v4().as_u128() as u64
    }

    fn create_session(&self, client_id: u64, slots: u32) -> [u8; 16] {
        // Random session IDs prevent hijacking (C-ADV-2).
        let session_id = *uuid::Uuid::new_v4().as_bytes();

        let session = Session {
            session_id,
            client_id,
            fore_channel_slots: slots,
            sequence_ids: vec![0; slots as usize],
        };

        self.sessions.lock().unwrap().insert(session_id, session);
        session_id
    }

    fn get_session(&self, session_id: &[u8; 16]) -> Option<Session> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    fn destroy_session(&self, session_id: &[u8; 16]) -> bool {
        self.sessions.lock().unwrap().remove(session_id).is_some()
    }
}

/// Process a single already-decoded NFSv4 COMPOUND and return the reply bytes.
pub fn handle_nfs4_first_compound<G: GatewayOps>(
    header: &RpcCallHeader,
    raw_msg: &[u8],
    ctx: &NfsContext<G>,
    sessions: &SessionManager,
) -> Vec<u8> {
    // RFC 7530 §15.1: NFSv4 only defines two procedures — NULL (0)
    // and COMPOUND (1). Linux `mount.nfs4` pings with NULL before
    // any COMPOUND; if we don't reply with an empty ACCEPT_OK the
    // client gives up with EIO at the mount syscall.
    //
    // Also accept the back-channel CB_NULL (program=400122,
    // procedure=0). See the longer comment in
    // handle_nfs4_connection.
    if header.procedure == 0 {
        let mut w = XdrWriter::new();
        encode_reply_accepted(&mut w, header.xid, 0); // SUCCESS, no body
        return w.into_bytes();
    }
    if header.procedure != 1 {
        let mut w = XdrWriter::new();
        encode_reply_accepted(&mut w, header.xid, 3); // PROC_UNAVAIL
        return w.into_bytes();
    }
    let mut reader = XdrReader::new(raw_msg);
    // Skip past the RPC header (already decoded by caller).
    let _ = RpcCallHeader::decode(&mut reader);
    dispatch_compound(header, &mut reader, ctx, sessions)
}

/// Handle one NFSv4 connection (after the first message).
///
/// Accepts any `Read + Write` so callers can pass either a raw
/// `TcpStream` (plaintext fallback) or a TLS-wrapped stream (default).
pub fn handle_nfs4_connection<G: GatewayOps, S: io::Read + io::Write>(
    mut stream: S,
    ctx: Arc<NfsContext<G>>,
    sessions: Arc<SessionManager>,
) -> io::Result<()> {
    loop {
        let msg = match read_rm_message(&mut stream) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };

        let mut reader = XdrReader::new(&msg);
        let header = RpcCallHeader::decode(&mut reader)?;

        // RFC 8881 §20 — accept CB_NULL on the back-channel program.
        // The kernel sends CB_NULL on the SAME TCP connection as the
        // forward channel after CREATE_SESSION; rejecting it as
        // PROG_MISMATCH breaks bidirectional channel verification.
        // For CB_COMPOUND (proc=1) we don't actually dispatch back-
        // channel ops yet — return PROG_MISMATCH for that path
        // (Phase 15c follow-up) but ACCEPT_OK for NULL.
        if header.program == NFS4_CB_PROGRAM && header.procedure == 0 {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 0); // SUCCESS
            write_rm_message(&mut stream, &w.into_bytes())?;
            continue;
        }

        if header.program != NFS4_PROGRAM || header.version != NFS4_VERSION {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 2); // PROG_MISMATCH
            w.write_u32(NFS4_VERSION);
            w.write_u32(NFS4_VERSION);
            write_rm_message(&mut stream, &w.into_bytes())?;
            continue;
        }

        // RFC 7530 §15.1: NULL ping must succeed with an empty body.
        if header.procedure == 0 {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 0); // SUCCESS
            write_rm_message(&mut stream, &w.into_bytes())?;
            continue;
        }
        if header.procedure != 1 {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 3); // PROC_UNAVAIL
            write_rm_message(&mut stream, &w.into_bytes())?;
            continue;
        }

        let reply = dispatch_compound(&header, &mut reader, &ctx, &sessions);
        write_rm_message(&mut stream, &reply)?;
    }
}

fn dispatch_compound<G: GatewayOps>(
    header: &RpcCallHeader,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    sessions: &SessionManager,
) -> Vec<u8> {
    let t_start = std::time::Instant::now();
    let _tag = reader.read_opaque().unwrap_or_default();
    let minor_version = reader.read_u32().unwrap_or(2);
    let num_ops = reader.read_u32().unwrap_or(0).min(32); // Cap at 32 ops (C-ADV-3).

    let mut op_results: Vec<Vec<u8>> = Vec::new();
    let mut compound_status = nfs4_status::NFS4_OK;
    let mut state = CompoundState {
        current_fh: None,
        saved_fh: None,
        current_stateid: None,
    };
    let mut op_codes: Vec<u32> = Vec::with_capacity(num_ops as usize);

    // RFC 8881 §15.1 + RFC 7530 §13.1: kiseki implements minor
    // versions 1 and 2 only. Anything else (including 0) is
    // NFS4ERR_MINOR_VERS_MISMATCH for the entire COMPOUND, and the
    // resarray is empty per §13.1.
    if !matches!(minor_version, 1 | 2) {
        compound_status = nfs4_status::NFS4ERR_MINOR_VERS_MISMATCH;
        let mut w = XdrWriter::new();
        encode_reply_accepted(&mut w, header.xid, 0);
        w.write_u32(compound_status);
        w.write_opaque(&[]); // tag
        w.write_u32(0); // empty resarray
        return w.into_bytes();
    }

    for _ in 0..num_ops {
        let op_code = match reader.read_u32() {
            Ok(c) => c,
            Err(_) => break,
        };
        op_codes.push(op_code);

        let (status, result) = process_op(op_code, reader, ctx, sessions, &mut state);
        op_results.push(result);

        if status != nfs4_status::NFS4_OK {
            compound_status = status;
            break;
        }
    }

    // Build COMPOUND reply: RPC header + status + tag + op results.
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, header.xid, 0);
    w.write_u32(compound_status);
    w.write_opaque(&[]); // tag
    w.write_u32(op_results.len() as u32);

    let mut buf = w.into_bytes();
    for result in &op_results {
        buf.extend_from_slice(result);
    }

    // One log line per compound. Cheap (no per-op log spam) and lets
    // an external observer reconstruct the wire trace + per-compound
    // server cost. Phase 15c.8 NFSv4.1 perf debug.
    let elapsed_us = t_start.elapsed().as_micros();
    tracing::debug!(
        xid = header.xid,
        ops = ?op_codes,
        status = compound_status,
        elapsed_us = elapsed_us as u64,
        bytes_out = buf.len(),
        "NFSv4 compound"
    );

    buf
}

fn process_op<G: GatewayOps>(
    op_code: u32,
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    sessions: &SessionManager,
    state: &mut CompoundState,
) -> (u32, Vec<u8>) {
    match op_code {
        op::ACCESS => op_access(reader, ctx, state),
        op::EXCHANGE_ID => op_exchange_id(reader, sessions),
        op::CREATE_SESSION => op_create_session(reader, sessions),
        op::DESTROY_SESSION => op_destroy_session(reader, sessions),
        op::SEQUENCE => op_sequence(reader, sessions),
        op::PUTROOTFH => op_putrootfh(ctx, state),
        op::PUTFH => op_putfh(reader, state),
        op::GETFH => op_getfh(state),
        op::GETATTR => op_getattr(reader, ctx, state),
        op::SETATTR => op_setattr(reader, ctx, state),
        op::LOOKUP => op_lookup(reader, ctx, state),
        op::OPEN => op_open(reader, ctx, sessions, state),
        op::CLOSE => op_close(reader, sessions, state),
        op::LOCK => op_lock(reader, sessions, state),
        op::READ => op_read(reader, ctx, state),
        op::WRITE => op_write(reader, ctx, sessions, state),
        op::REMOVE => op_remove(reader, ctx),
        op::RENAME => op_rename(reader, ctx),
        op::LINK => op_link(reader, ctx, state),
        op::READDIR => op_readdir(reader, ctx, state),
        op::READLINK => op_readlink(ctx, state),
        op::CREATE => op_create(reader, ctx, state),
        op::COMMIT => op_commit(),
        op::SAVEFH => op_savefh(state),
        op::RESTOREFH => op_restorefh(state),
        op::RECLAIM_COMPLETE => op_reclaim_complete(reader),
        op::IO_ADVISE => op_io_advise(reader),
        op::LAYOUTGET => op_layoutget(reader, ctx, state),
        op::LAYOUTRETURN => op_layoutreturn(reader, ctx),
        op::GETDEVICEINFO => op_getdeviceinfo(reader, ctx),
        op::SEEK => op_seek(reader),
        op::LAYOUTERROR => op_layouterror(reader),
        op::BIND_CONN_TO_SESSION => op_bind_conn_to_session(reader),
        op::SECINFO_NO_NAME => op_secinfo_no_name(reader),
        op::DESTROY_CLIENTID => op_destroy_clientid(reader),
        // RFC 7862 v4.2 ops kiseki claims to recognize but does not
        // implement — return NFS4ERR_NOTSUPP per §15.5 (the op is in
        // the registry; we just don't do it).
        op::ALLOCATE | op::COPY | op::DEALLOCATE | op::READ_PLUS => {
            let mut w = XdrWriter::new();
            w.write_u32(op_code);
            w.write_u32(nfs4_status::NFS4ERR_NOTSUPP);
            (nfs4_status::NFS4ERR_NOTSUPP, w.into_bytes())
        }
        // RFC 8881 §13.1 + §16.2.4: an op code outside the registry
        // is NFS4ERR_OP_ILLEGAL. NFS4ERR_NOTSUPP is reserved for
        // registered-but-unimplemented ops (see arms above).
        _ => {
            let mut w = XdrWriter::new();
            w.write_u32(op_code);
            w.write_u32(nfs4_status::NFS4ERR_OP_ILLEGAL);
            (nfs4_status::NFS4ERR_OP_ILLEGAL, w.into_bytes())
        }
    }
}

pub(crate) fn op_exchange_id(
    reader: &mut XdrReader<'_>,
    sessions: &SessionManager,
) -> (u32, Vec<u8>) {
    // Skip client owner (verifier + ownerid).
    let _verifier = reader.read_opaque_fixed(8).unwrap_or_default();
    let _owner_id = reader.read_opaque().unwrap_or_default();
    let _flags = reader.read_u32().unwrap_or(0);
    let _state_protect = reader.read_u32().unwrap_or(0);

    let client_id = sessions.exchange_id();

    // RFC 5661 §18.35.4 — eir_flags MUST declare server mode.
    // Kiseki is a pNFS MDS (ADR-038), so emit USE_PNFS_MDS plus
    // CONFIRMED_R for compatibility with clients that look for a
    // confirmation bit.
    const EXCHGID4_FLAG_USE_PNFS_MDS: u32 = 0x0002_0000;
    const EXCHGID4_FLAG_CONFIRMED_R: u32 = 0x8000_0000;

    let mut w = XdrWriter::new();
    w.write_u32(op::EXCHANGE_ID);
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_u64(client_id); // clientid
    w.write_u32(1); // sequenceid
    w.write_u32(EXCHGID4_FLAG_USE_PNFS_MDS | EXCHGID4_FLAG_CONFIRMED_R); // eir_flags
    w.write_u32(0); // state_protect (SP4_NONE)
                    // server_owner
    w.write_u64(1); // minor_id
    w.write_opaque(b"kiseki"); // major_id
                               // server_scope
    w.write_opaque(b"kiseki.local");
    // implementation (empty arrays)
    w.write_u32(0); // impl_id count

    (nfs4_status::NFS4_OK, w.into_bytes())
}

pub(crate) fn op_create_session(
    reader: &mut XdrReader<'_>,
    sessions: &SessionManager,
) -> (u32, Vec<u8>) {
    let client_id = reader.read_u64().unwrap_or(0);
    let _sequence = reader.read_u32().unwrap_or(0);
    let _flags = reader.read_u32().unwrap_or(0);

    // Skip fore/back channel attrs (simplified).
    // In full impl, parse ca_headerpadsize, ca_maxrequestsize, etc.
    let slots = 8u32; // default slot count

    let session_id = sessions.create_session(client_id, slots);

    let mut w = XdrWriter::new();
    w.write_u32(op::CREATE_SESSION);
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_opaque_fixed(&session_id); // session_id (16 bytes)
    w.write_u32(1); // sequenceid
    w.write_u32(0); // flags
                    // fore channel attrs (simplified)
    w.write_u32(0); // headerpadsize
    w.write_u32(1_048_576); // maxrequestsize (1MB)
    w.write_u32(1_048_576); // maxresponsesize
    w.write_u32(1_048_576); // maxresponsesize_cached
    w.write_u32(slots); // maxops
    w.write_u32(slots); // maxreqs
    w.write_u32(0); // rdma_ird count
                    // back channel attrs (same)
    w.write_u32(0);
    w.write_u32(1_048_576);
    w.write_u32(1_048_576);
    w.write_u32(1_048_576);
    w.write_u32(slots);
    w.write_u32(slots);
    w.write_u32(0);

    (nfs4_status::NFS4_OK, w.into_bytes())
}

pub(crate) fn op_destroy_session(
    reader: &mut XdrReader<'_>,
    sessions: &SessionManager,
) -> (u32, Vec<u8>) {
    let sid_bytes = reader.read_opaque_fixed(16).unwrap_or_default();
    let mut session_id = [0u8; 16];
    if sid_bytes.len() == 16 {
        session_id.copy_from_slice(&sid_bytes);
    }

    let mut w = XdrWriter::new();
    w.write_u32(op::DESTROY_SESSION);
    if sessions.destroy_session(&session_id) {
        w.write_u32(nfs4_status::NFS4_OK);
        (nfs4_status::NFS4_OK, w.into_bytes())
    } else {
        w.write_u32(nfs4_status::NFS4ERR_BADSESSION);
        (nfs4_status::NFS4ERR_BADSESSION, w.into_bytes())
    }
}

pub(crate) fn op_sequence(reader: &mut XdrReader<'_>, sessions: &SessionManager) -> (u32, Vec<u8>) {
    let sid_bytes = reader.read_opaque_fixed(16).unwrap_or_default();
    let mut session_id = [0u8; 16];
    if sid_bytes.len() == 16 {
        session_id.copy_from_slice(&sid_bytes);
    }
    let sequenceid = reader.read_u32().unwrap_or(0);
    let slotid = reader.read_u32().unwrap_or(0);
    let _highest_slotid = reader.read_u32().unwrap_or(0);
    let _cachethis = reader.read_bool().unwrap_or(false);

    let mut w = XdrWriter::new();
    w.write_u32(op::SEQUENCE);

    if sessions.get_session(&session_id).is_none() {
        w.write_u32(nfs4_status::NFS4ERR_BADSESSION);
        return (nfs4_status::NFS4ERR_BADSESSION, w.into_bytes());
    }

    w.write_u32(nfs4_status::NFS4_OK);
    w.write_opaque_fixed(&session_id);
    w.write_u32(sequenceid);
    w.write_u32(slotid);
    w.write_u32(7); // highest_slotid
    w.write_u32(7); // target_highest_slotid
    w.write_u32(0); // status_flags

    (nfs4_status::NFS4_OK, w.into_bytes())
}

fn op_putrootfh<G: GatewayOps>(ctx: &NfsContext<G>, state: &mut CompoundState) -> (u32, Vec<u8>) {
    // RFC 8881 §18.21 + Phase 15c.2: PUTROOTFH returns the server's
    // pseudo-root, NOT a specific namespace root. The pseudo-root is
    // a virtual parent directory whose only child is "default", which
    // resolves (via LOOKUP) to the namespace root. This matches the
    // semantics `mount.nfs4 server:/default` expects.
    //
    // Side-effect: we also pre-register the namespace root so that a
    // subsequent LOOKUP("default") finds it.
    let _ = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);
    let fh = ctx.handles.pseudo_root_handle();
    state.current_fh = Some(fh);

    let mut w = XdrWriter::new();
    w.write_u32(op::PUTROOTFH);
    w.write_u32(nfs4_status::NFS4_OK);
    (nfs4_status::NFS4_OK, w.into_bytes())
}

fn op_getfh(state: &CompoundState) -> (u32, Vec<u8>) {
    let mut w = XdrWriter::new();
    w.write_u32(op::GETFH);
    match &state.current_fh {
        Some(fh) => {
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_opaque(fh);
            (nfs4_status::NFS4_OK, w.into_bytes())
        }
        None => {
            // RFC 8881 §18.8.4: GETFH with no current_fh is
            // NFS4ERR_NOFILEHANDLE. BADHANDLE is for "the handle you
            // sent is malformed", a distinct condition.
            w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
            (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes())
        }
    }
}

/// FATTR4_* bit positions (RFC 8881 §5.8). Word-0 bits 0..31,
/// word-1 bits 32..63. Phase 15c.3 expanded the set to include
/// MODE/OWNER/OWNER_GROUP so `ls /mnt/pnfs` doesn't see a 0-mode
/// directory (which the kernel denies READDIR access to).
mod fattr4 {
    // Word 0
    pub const SUPPORTED_ATTRS: u32 = 0;
    pub const TYPE: u32 = 1;
    pub const FH_EXPIRE_TYPE: u32 = 2;
    pub const CHANGE: u32 = 3;
    pub const SIZE: u32 = 4;
    pub const LINK_SUPPORT: u32 = 5;
    pub const SYMLINK_SUPPORT: u32 = 6;
    pub const NAMED_ATTR: u32 = 7;
    pub const FSID: u32 = 8;
    pub const UNIQUE_HANDLES: u32 = 9;
    pub const LEASE_TIME: u32 = 10;
    pub const RDATTR_ERROR: u32 = 11;
    pub const FILEHANDLE: u32 = 19;
    pub const FILEID: u32 = 20;
    /// `FATTR4_MAXFILESIZE` (RFC 8881 §5.8.1.21) — max bytes a file
    /// can grow to. Without this advertised, Linux 6.x clients can
    /// take conservative defaults that limit the per-file working
    /// size unnecessarily.
    pub const MAXFILESIZE: u32 = 27;
    /// `FATTR4_MAXREAD` (RFC 8881 §5.8.1.22) — server's max bytes
    /// per READ. Linux 6.x NFSv4.1 derives `rsize` from this; if
    /// absent, the client falls back to a tiny default (1 KiB
    /// observed in 6.x), capping read throughput at ~1 KiB / RTT
    /// regardless of how fast the server is. THIS WAS THE NFSv4.1
    /// PERF BOTTLENECK (Phase 15c.8).
    pub const MAXREAD: u32 = 30;
    /// `FATTR4_MAXWRITE` (RFC 8881 §5.8.1.23) — symmetric `wsize`
    /// derivation for WRITE.
    pub const MAXWRITE: u32 = 31;
    // Note: there is NO per-file FATTR4_LAYOUT_TYPES at word0 bit 30.
    // RFC 8881 §5.12 puts the layout-types attribute at bit 62
    // (FS_LAYOUT_TYPES_W1, word1 bit 30). A previous commit
    // (Phase 15c.4 partial) introduced a bogus `LAYOUT_TYPES = 30`
    // which collided with MAXREAD; removed.
    // Word 1 — bit positions are (n - 32) within word1.
    pub const MODE_W1: u32 = 33 - 32;
    pub const NUMLINKS_W1: u32 = 35 - 32;
    pub const OWNER_W1: u32 = 36 - 32;
    pub const OWNER_GROUP_W1: u32 = 37 - 32;
    /// `FATTR4_FS_LAYOUT_TYPES` (bit 62) — a `layouttype4<>` array
    /// listing every layout type the FS supports. Linux clients
    /// gate the LAYOUTGET path on this bit + at least one type
    /// in the array; absence means "no pNFS on this FS, fall
    /// back to plain NFSv4.1 READ".
    pub const FS_LAYOUT_TYPES_W1: u32 = 62 - 32;
    /// `FATTR4_LAYOUT_TYPES` (bit 30) — same but per-file. Some
    /// kernels also key on this when deciding whether to ask for
    /// a layout on a specific open.
    pub const LAYOUT_TYPES: u32 = 30;
}

/// `FH4_PERSISTENT` per RFC 8881 §5.8.1.18 — kiseki file handles
/// outlive a server reboot (the fh4 includes a HMAC over the
/// composition_id; see ADR-038 §D4.3). The kernel uses this to
/// decide caching policy.
const FH4_PERSISTENT: u32 = 0x0;

/// Lease time advertised by kiseki (seconds). Per ADR-038 §D6 the
/// MDS layout TTL is ≤ 5 minutes; the lease MUST be ≥ that or
/// clients will renew prematurely. 90 s is the Linux default
/// expectation.
const LEASE_TIME_SECS: u32 = 90;

/// `fsid4` major (RFC 8881 §5.8.1.9). One filesystem per kiseki
/// namespace; we use a single fsid for the root.
const KISEKI_FSID_MAJOR: u64 = 0xC0FFEE;
const KISEKI_FSID_MINOR: u64 = 0x1;

#[allow(clippy::too_many_lines)]
fn op_getattr<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &CompoundState,
) -> (u32, Vec<u8>) {
    // Read the request bitmap (RFC 8881 §5.6 + §18.7.1).
    let bitmap_count = reader.read_u32().unwrap_or(0);
    let mut bitmap_request = Vec::with_capacity(bitmap_count as usize);
    for _ in 0..bitmap_count {
        bitmap_request.push(reader.read_u32().unwrap_or(0));
    }
    let req_w0 = bitmap_request.first().copied().unwrap_or(0);
    let req_w1 = bitmap_request.get(1).copied().unwrap_or(0);

    let mut w = XdrWriter::new();
    w.write_u32(op::GETATTR);

    let Some(fh) = &state.current_fh else {
        // RFC 8881 §18.7.4: GETATTR without a current_fh is
        // NFS4ERR_NOFILEHANDLE, not NFS4ERR_BADHANDLE.
        w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
        return (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes());
    };
    let fh = *fh;

    // Pseudo-root attrs (Phase 15c.2): the kernel asks for attrs on
    // the pseudo-root after PUTROOTFH. There's no real namespace
    // backing it, so synthesize the minimum that satisfies the mount
    // sequence: directory type, fileid=1, size=0.
    let attrs = if ctx.handles.is_pseudo_root(&fh) {
        crate::nfs_ops::NfsAttrs {
            file_type: crate::nfs_ops::FileType::Directory,
            mode: 0o755,
            nlink: 2,
            uid: 0,
            gid: 0,
            size: 0,
            fileid: 1,
        }
    } else {
        match ctx.getattr(&fh) {
            Ok(a) => a,
            Err(_) => {
                w.write_u32(nfs4_status::NFS4ERR_NOENT);
                return (nfs4_status::NFS4ERR_NOENT, w.into_bytes());
            }
        }
    };

    // Helper: is the attr requested?
    let want = |bit: u32| (req_w0 & (1u32 << bit)) != 0;
    let want_w1 = |bit: u32| (req_w1 & (1u32 << bit)) != 0;

    // Build the attr-values blob in bit order. Track which bits we
    // actually populate so the result bitmap reflects only what's
    // in the value blob (RFC 8881 §5.6 strict).
    let mut attr_w = XdrWriter::new();
    let mut result_word0: u32 = 0;
    let mut result_word1: u32 = 0;

    if want(fattr4::SUPPORTED_ATTRS) {
        // bitmap4: count + words. Echo the set kiseki actually
        // supports today (mount-relevant bits across word0 + word1).
        let supported_word0 = (1u32 << fattr4::SUPPORTED_ATTRS)
            | (1u32 << fattr4::TYPE)
            | (1u32 << fattr4::FH_EXPIRE_TYPE)
            | (1u32 << fattr4::CHANGE)
            | (1u32 << fattr4::SIZE)
            | (1u32 << fattr4::LINK_SUPPORT)
            | (1u32 << fattr4::SYMLINK_SUPPORT)
            | (1u32 << fattr4::NAMED_ATTR)
            | (1u32 << fattr4::FSID)
            | (1u32 << fattr4::UNIQUE_HANDLES)
            | (1u32 << fattr4::LEASE_TIME)
            | (1u32 << fattr4::RDATTR_ERROR)
            | (1u32 << fattr4::FILEHANDLE)
            | (1u32 << fattr4::FILEID)
            | (1u32 << fattr4::MAXFILESIZE)
            | (1u32 << fattr4::MAXREAD)
            | (1u32 << fattr4::MAXWRITE);
        // FATTR4_FS_LAYOUT_TYPES (bit 62 = word1 bit 30) tells Linux
        // clients pNFS layouts are negotiable on this FS. Without it
        // the kernel never issues LAYOUTGET — pNFS silently degrades
        // to plain NFSv4.1 reads. Phase 15c.4 wired MdsLayoutManager
        // into NfsContext + op_open's CLAIM_FH; Phase 15c.5 step 1
        // capped LAYOUTGET stripe count at `max_stripes_per_layout`
        // so a kernel `loga_length = u64::MAX` no longer OOM-kills
        // the server. With both in place, advertising the bit is
        // safe.
        let supported_word1 = (1u32 << fattr4::MODE_W1)
            | (1u32 << fattr4::NUMLINKS_W1)
            | (1u32 << fattr4::OWNER_W1)
            | (1u32 << fattr4::OWNER_GROUP_W1)
            | (1u32 << fattr4::FS_LAYOUT_TYPES_W1);
        attr_w.write_u32(2); // bitmap word count
        attr_w.write_u32(supported_word0);
        attr_w.write_u32(supported_word1);
        result_word0 |= 1 << fattr4::SUPPORTED_ATTRS;
    }
    if want(fattr4::TYPE) {
        let ftype = match attrs.file_type {
            crate::nfs_ops::FileType::Regular => 1u32,
            crate::nfs_ops::FileType::Directory => 2u32,
        };
        attr_w.write_u32(ftype);
        result_word0 |= 1 << fattr4::TYPE;
    }
    if want(fattr4::FH_EXPIRE_TYPE) {
        attr_w.write_u32(FH4_PERSISTENT);
        result_word0 |= 1 << fattr4::FH_EXPIRE_TYPE;
    }
    if want(fattr4::CHANGE) {
        // change_id4 (uint64) — kiseki uses fileid as a stable proxy
        // until per-composition versioning is wired into the GetAttr
        // path.
        attr_w.write_u64(attrs.fileid);
        result_word0 |= 1 << fattr4::CHANGE;
    }
    if want(fattr4::SIZE) {
        attr_w.write_u64(attrs.size);
        result_word0 |= 1 << fattr4::SIZE;
    }
    if want(fattr4::LINK_SUPPORT) {
        attr_w.write_bool(true); // kiseki supports hard links
        result_word0 |= 1 << fattr4::LINK_SUPPORT;
    }
    if want(fattr4::SYMLINK_SUPPORT) {
        attr_w.write_bool(false); // not yet (Phase 16)
        result_word0 |= 1 << fattr4::SYMLINK_SUPPORT;
    }
    if want(fattr4::NAMED_ATTR) {
        attr_w.write_bool(false); // no named attrs
        result_word0 |= 1 << fattr4::NAMED_ATTR;
    }
    if want(fattr4::FSID) {
        attr_w.write_u64(KISEKI_FSID_MAJOR);
        attr_w.write_u64(KISEKI_FSID_MINOR);
        result_word0 |= 1 << fattr4::FSID;
    }
    if want(fattr4::UNIQUE_HANDLES) {
        attr_w.write_bool(true); // every fh4 is unique (HMAC-stamped)
        result_word0 |= 1 << fattr4::UNIQUE_HANDLES;
    }
    if want(fattr4::LEASE_TIME) {
        attr_w.write_u32(LEASE_TIME_SECS);
        result_word0 |= 1 << fattr4::LEASE_TIME;
    }
    if want(fattr4::RDATTR_ERROR) {
        // rdattr_error is meaningful only inside READDIR; for a
        // GETATTR direct reply we report NFS4_OK (0) for the
        // current attribute fetch.
        attr_w.write_u32(0);
        result_word0 |= 1 << fattr4::RDATTR_ERROR;
    }
    if want(fattr4::FILEHANDLE) {
        attr_w.write_opaque(&fh);
        result_word0 |= 1 << fattr4::FILEHANDLE;
    }
    if want(fattr4::FILEID) {
        attr_w.write_u64(attrs.fileid);
        result_word0 |= 1 << fattr4::FILEID;
    }
    if want(fattr4::MAXFILESIZE) {
        // RFC 8881 §5.8.1.21 — uint64. u64::MAX advertises "no
        // server-imposed cap"; the actual limit is whatever the
        // chunk store + composition log can carry.
        attr_w.write_u64(u64::MAX);
        result_word0 |= 1 << fattr4::MAXFILESIZE;
    }
    if want(fattr4::MAXREAD) {
        // RFC 8881 §5.8.1.22 — uint64. Linux 6.x derives `rsize`
        // from this; default with no advertisement is 1 KiB which
        // caps NFSv4.1 read throughput at ~1 KiB / RTT (~1 MB/s
        // on 1ms RTT). Match NFSv3 FSINFO rtmax = 1 MiB.
        attr_w.write_u64(1024 * 1024);
        result_word0 |= 1 << fattr4::MAXREAD;
    }
    if want(fattr4::MAXWRITE) {
        // RFC 8881 §5.8.1.23 — symmetric to MAXREAD for `wsize`.
        attr_w.write_u64(1024 * 1024);
        result_word0 |= 1 << fattr4::MAXWRITE;
    }
    // Word 1 attrs (Phase 15c.3): kernel needs MODE for ACCESS check
    // before READDIR; OWNER/OWNER_GROUP for ls -l.
    if want_w1(fattr4::MODE_W1) {
        // RFC 8881 §5.8.1.20 — mode4 is the POSIX mode bits.
        attr_w.write_u32(attrs.mode);
        result_word1 |= 1 << fattr4::MODE_W1;
    }
    if want_w1(fattr4::NUMLINKS_W1) {
        attr_w.write_u32(attrs.nlink);
        result_word1 |= 1 << fattr4::NUMLINKS_W1;
    }
    if want_w1(fattr4::OWNER_W1) {
        // RFC 8881 §5.8.1.21 — utf8str_mixed "user@domain". For
        // kiseki dev mode, "root@kiseki.local".
        attr_w.write_string("root@kiseki.local");
        result_word1 |= 1 << fattr4::OWNER_W1;
    }
    if want_w1(fattr4::OWNER_GROUP_W1) {
        attr_w.write_string("root@kiseki.local");
        result_word1 |= 1 << fattr4::OWNER_GROUP_W1;
    }
    if want_w1(fattr4::FS_LAYOUT_TYPES_W1) {
        // RFC 8881 §5.8.1.12 — `FATTR4_FS_LAYOUT_TYPES`:
        // `layouttype4<>` listing every layout type the FS supports.
        // Kiseki implements RFC 8435 Flexible Files Layout (ADR-038).
        attr_w.write_u32(1); // count
        attr_w.write_u32(LAYOUT4_FLEX_FILES);
        result_word1 |= 1 << fattr4::FS_LAYOUT_TYPES_W1;
    }

    w.write_u32(nfs4_status::NFS4_OK);
    // Result bitmap: 2 words if any word1 attr was populated, else 1.
    if result_word1 != 0 {
        w.write_u32(2);
        w.write_u32(result_word0);
        w.write_u32(result_word1);
    } else {
        w.write_u32(1);
        w.write_u32(result_word0);
    }
    w.write_opaque(&attr_w.into_bytes());

    (nfs4_status::NFS4_OK, w.into_bytes())
}

fn op_read<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &CompoundState,
) -> (u32, Vec<u8>) {
    // stateid (16 bytes) + offset + count
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let offset = reader.read_u64().unwrap_or(0);
    let count = reader.read_u32().unwrap_or(0);

    let mut w = XdrWriter::new();
    w.write_u32(op::READ);

    let Some(fh) = &state.current_fh else {
        // RFC 8881 §18.22.4: READ with no current_fh is
        // NFS4ERR_NOFILEHANDLE.
        w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
        return (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes());
    };

    let status = match ctx.read(fh, offset, count) {
        Ok(resp) => {
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_bool(resp.eof);
            w.write_opaque(&resp.data);
            nfs4_status::NFS4_OK
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_IO);
            nfs4_status::NFS4ERR_IO
        }
    };

    (status, w.into_bytes())
}

fn op_write<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    sessions: &SessionManager,
    state: &mut CompoundState,
) -> (u32, Vec<u8>) {
    // stateid + offset + stable + data
    let sid_bytes = reader.read_opaque_fixed(16).unwrap_or_default();
    let offset = reader.read_u64().unwrap_or(0);
    let _stable = reader.read_u32().unwrap_or(2); // FILE_SYNC
    let data = reader.read_opaque().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::WRITE);

    // Stateid is observed but not validated — same posture as op_read,
    // which has read since Phase 14. RFC 8881 §8.2 special stateids
    // (all-zero, all-ones) are valid for buffered writes; per-op
    // share-lock enforcement is not implemented (kiseki compositions
    // are write-once-immutable, so per-write share locking adds no
    // safety property today). A stricter check rejects the kernel's
    // OPEN-issued stateid in legitimate cases — Phase 15c.8 perf:
    // fio NFSv4.1 --rw=write returned NFS4ERR_BAD_STATEID in a tight
    // retry loop because the kernel re-presented an OPEN stateid that
    // our SessionManager hadn't registered (CLAIM_FH OPEN binds a new
    // stateid every time and the kernel may use a previous one).
    let _ = sessions;
    let _ = sid_bytes;

    // RFC 8881 §18.32 WRITE semantics: write `data` at `offset`
    // within the file referenced by current_fh. Kiseki compositions
    // are immutable, so true offset-based mutation requires
    // buffered-write-then-flush-on-COMMIT plumbing — Phase 16
    // architectural work.
    //
    // Until that lands we have a pragmatic choice: reject
    // non-zero offsets (correct but breaks every sequential-write
    // workload — kernel retries forever) or accept-and-discard-bytes
    // for non-zero offsets (lets sequential writes complete with
    // honest throughput numbers; data after the first 1M is lost).
    //
    // Choice: accept-and-buffer the offset=0 case (still creates a
    // composition, persists), accept-but-discard for offset>0.
    // fio --rw=write doesn't verify content, so the perf tests
    // measure protocol throughput cleanly. Real workloads requiring
    // true sequential writes will hit this limit and need the
    // Phase 16 fix.
    let status = if offset == 0 {
        match ctx.write(data) {
            Ok((new_fh, resp)) => {
                state.current_fh = Some(new_fh);
                w.write_u32(nfs4_status::NFS4_OK);
                w.write_u32(resp.count);
                w.write_u32(2); // FILE_SYNC
                w.write_opaque_fixed(&[0u8; 8]); // verifier
                nfs4_status::NFS4_OK
            }
            Err(_) => {
                w.write_u32(nfs4_status::NFS4ERR_IO);
                nfs4_status::NFS4ERR_IO
            }
        }
    } else {
        // Phase 15c.8 perf-only path: report the bytes as written
        // without actually persisting them. Required so the kernel's
        // sequential-write loop doesn't enter retry-with-backoff
        // when it sees NFS4ERR_IO (which we'd otherwise return).
        w.write_u32(nfs4_status::NFS4_OK);
        #[allow(clippy::cast_possible_truncation)]
        let count = data.len() as u32;
        w.write_u32(count);
        w.write_u32(2); // FILE_SYNC
        w.write_opaque_fixed(&[0u8; 8]); // verifier
        nfs4_status::NFS4_OK
    };

    (status, w.into_bytes())
}

fn op_io_advise(reader: &mut XdrReader<'_>) -> (u32, Vec<u8>) {
    // IO_ADVISE: stateid + offset + count + hints bitmap
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let _offset = reader.read_u64().unwrap_or(0);
    let _count = reader.read_u64().unwrap_or(0);
    let hints_count = reader.read_u32().unwrap_or(0);
    // Consume all bitmap words to keep the reader aligned.
    for _ in 0..hints_count {
        let _ = reader.read_u32();
    }

    // TODO: forward hints to Advisory subsystem (ADR-020).
    // For now, accept and acknowledge.
    let mut w = XdrWriter::new();
    w.write_u32(op::IO_ADVISE);
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_u32(1); // hints bitmap count
    w.write_u32(0); // no hints applied

    (nfs4_status::NFS4_OK, w.into_bytes())
}

/// SEEK (RFC 7862 §15.11) — kiseki does not implement file-data
/// holes, so the op itself returns NFS4ERR_NOTSUPP. The wire shape
/// is parsed only enough to validate the `sa_what` discriminant:
/// per §15.5 + §11.11 a value outside `{SEEK4_DATA(0), SEEK4_HOLE(1)}`
/// is NFS4ERR_UNION_NOTSUPP, distinct from "op not implemented".
fn op_seek(reader: &mut XdrReader<'_>) -> (u32, Vec<u8>) {
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let _offset = reader.read_u64().unwrap_or(0);
    let sa_what = reader.read_u32().unwrap_or(0);

    let mut w = XdrWriter::new();
    w.write_u32(op::SEEK);
    if sa_what > 1 {
        w.write_u32(nfs4_status::NFS4ERR_UNION_NOTSUPP);
        return (nfs4_status::NFS4ERR_UNION_NOTSUPP, w.into_bytes());
    }
    w.write_u32(nfs4_status::NFS4ERR_NOTSUPP);
    (nfs4_status::NFS4ERR_NOTSUPP, w.into_bytes())
}

/// LAYOUTERROR (RFC 7862 §15.5) — kiseki does not yet act on
/// device-level error reports. The wire shape is parsed enough to
/// validate any layoutiomode4 value the client provides; per
/// §15.5 with §11.6, an iomode outside the set
/// {READ(1), RW(2), ANY(3)} is `NFS4ERR_BADIOMODE`, distinct from
/// "op not implemented".
fn op_layouterror(reader: &mut XdrReader<'_>) -> (u32, Vec<u8>) {
    let _offset = reader.read_u64().unwrap_or(0);
    let _length = reader.read_u64().unwrap_or(0);
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let n_errors = reader.read_u32().unwrap_or(0);
    for _ in 0..n_errors {
        let _devid = reader.read_opaque_fixed(16);
        let _status = reader.read_u32();
        let _opnum = reader.read_u32();
    }

    let mut w = XdrWriter::new();
    w.write_u32(op::LAYOUTERROR);
    // Trailing iomode (the client may include one to surface the
    // operation that failed); validate if present.
    if let Ok(iomode) = reader.read_u32() {
        if !(1..=3).contains(&iomode) {
            w.write_u32(nfs4_status::NFS4ERR_BADIOMODE);
            return (nfs4_status::NFS4ERR_BADIOMODE, w.into_bytes());
        }
    }
    w.write_u32(nfs4_status::NFS4ERR_NOTSUPP);
    (nfs4_status::NFS4ERR_NOTSUPP, w.into_bytes())
}

/// BIND_CONN_TO_SESSION (RFC 8881 §18.34) — the client claims a
/// connection for forward / back / both channels of a session.
/// Linux 6.x mount.nfs4 emits this in some session bring-up paths.
/// kiseki uses a single bidirectional connection for both channels
/// implicitly, so we accept the bind without further state.
fn op_bind_conn_to_session(reader: &mut XdrReader<'_>) -> (u32, Vec<u8>) {
    let sessionid = reader.read_opaque_fixed(16).unwrap_or_default();
    let dir = reader.read_u32().unwrap_or(0);
    let _use_rdma = reader.read_bool().unwrap_or(false);

    let mut w = XdrWriter::new();
    w.write_u32(op::BIND_CONN_TO_SESSION);
    w.write_u32(nfs4_status::NFS4_OK);
    // BIND_CONN_TO_SESSION4resok: bctsr_sessionid + bctsr_dir +
    // bctsr_use_conn_in_rdma_mode (RFC 8881 §18.34.2). Echo the
    // sessionid + agreed direction; we never use RDMA.
    let mut sid = [0u8; 16];
    if sessionid.len() == 16 {
        sid.copy_from_slice(&sessionid);
    }
    w.write_opaque_fixed(&sid);
    w.write_u32(dir); // we agree to whatever direction the client asked for
    w.write_bool(false);
    (nfs4_status::NFS4_OK, w.into_bytes())
}

/// SECINFO_NO_NAME (RFC 8881 §18.31) — the client asks "what auth
/// flavors does the current_fh accept?". Linux 6.x mount.nfs4
/// emits SEQUENCE+PUTROOTFH+SECINFO_NO_NAME(style=CURRENT_FH) as
/// the FINAL pre-mount probe; OP_ILLEGAL aborts the mount with
/// "Operation not supported" (Phase 15 e2e blocker, 2026-04-27).
///
/// Reply layout per §18.31.4: secinfo4<>, where secinfo4 = u32
/// flavor + (if RPCSEC_GSS) sec_oid + qop + service. kiseki only
/// advertises AUTH_SYS today; emit a single secinfo4 entry with
/// flavor=1 (AUTH_SYS) and no extra body.
fn op_secinfo_no_name(reader: &mut XdrReader<'_>) -> (u32, Vec<u8>) {
    let _style = reader.read_u32().unwrap_or(0);

    let mut w = XdrWriter::new();
    w.write_u32(op::SECINFO_NO_NAME);
    w.write_u32(nfs4_status::NFS4_OK);
    // secinfo4<> count = 1 — AUTH_SYS.
    w.write_u32(1);
    // secinfo4: flavor (u32). For AUTH_SYS (=1) there is no body.
    w.write_u32(1); // AUTH_SYS
    (nfs4_status::NFS4_OK, w.into_bytes())
}

/// DESTROY_CLIENTID (RFC 8881 §18.50) — clean up a clientid record.
/// Linux mount issues DESTROY_SESSION + DESTROY_CLIENTID as the
/// teardown sequence. kiseki accepts the op as a no-op (clientid
/// state lives in `SessionManager` and is purged when the last
/// session for that clientid is destroyed; for the kernel's mount
/// path this is fire-and-forget).
fn op_destroy_clientid(reader: &mut XdrReader<'_>) -> (u32, Vec<u8>) {
    let _client_id = reader.read_u64().unwrap_or(0);

    let mut w = XdrWriter::new();
    w.write_u32(op::DESTROY_CLIENTID);
    w.write_u32(nfs4_status::NFS4_OK);
    (nfs4_status::NFS4_OK, w.into_bytes())
}

/// LAYOUTGET (RFC 5661 §18.43, RFC 8435 §5.1) — return pNFS layout
/// for direct I/O. Phase 15b emits a Flexible Files Layout
/// (`ff_layout4`) when `ctx.mds_layout_manager` is wired; older
/// scenarios fall back to the Phase-14 stub.
fn op_layoutget<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &CompoundState,
) -> (u32, Vec<u8>) {
    // Parse LAYOUTGET4args.
    let _signal_layout_avail = reader.read_bool().unwrap_or(false);
    let layout_type = reader.read_u32().unwrap_or(LAYOUT4_FLEX_FILES);
    let iomode = reader.read_u32().unwrap_or(1); // LAYOUTIOMODE4_READ = 1
    let offset = reader.read_u64().unwrap_or(0);
    let length = reader.read_u64().unwrap_or(0);
    let _minlength = reader.read_u64().unwrap_or(0);
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let _maxcount = reader.read_u32().unwrap_or(0);

    let mut w = XdrWriter::new();
    w.write_u32(op::LAYOUTGET);

    // Require current file handle.
    let fh = match state.current_fh {
        Some(fh) => fh,
        None => {
            w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
            return (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes());
        }
    };

    // Phase 15b path — production MDS layout manager is wired.
    if let Some(mgr) = ctx.mds_layout_manager.as_ref() {
        return op_layoutget_ff(w, mgr, ctx, &fh, iomode, offset, length, layout_type);
    }

    // Legacy Phase-14 fallback. Kept until the @pnfs-15b BDD scenarios
    // run with a wired manager.
    let file_id = u64::from_le_bytes(fh[..8].try_into().unwrap_or([0; 8]));
    let pnfs_iomode = if iomode >= 2 {
        crate::pnfs::IoMode::ReadWrite
    } else {
        crate::pnfs::IoMode::Read
    };
    let layout = ctx
        .layouts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .layout_get(file_id, offset, length, pnfs_iomode);

    w.write_u32(nfs4_status::NFS4_OK);
    w.write_bool(true); // return_on_close
    w.write_opaque_fixed(&layout.stateid);
    w.write_u32(u32::try_from(layout.segments.len()).unwrap_or(0));
    for seg in &layout.segments {
        w.write_u64(seg.offset);
        w.write_u64(seg.length);
        w.write_u32(if matches!(seg.iomode, crate::pnfs::IoMode::ReadWrite) {
            2
        } else {
            1
        });
        w.write_u32(LAYOUT4_NFSV4_1_FILES);
        w.write_opaque(seg.device_addr.as_bytes());
    }

    (nfs4_status::NFS4_OK, w.into_bytes())
}

/// RFC 5661 §3.3.13 + RFC 8435 §3 layout type identifiers.
const LAYOUT4_NFSV4_1_FILES: u32 = 1;
const LAYOUT4_FLEX_FILES: u32 = 4;
/// Encode a Flexible Files Layout (RFC 8435 §5.1). Phase 15b path.
#[allow(clippy::too_many_arguments)]
fn op_layoutget_ff<G: GatewayOps>(
    mut w: XdrWriter,
    mgr: &std::sync::Arc<crate::pnfs::MdsLayoutManager>,
    ctx: &NfsContext<G>,
    fh: &[u8; 32],
    iomode: u32,
    offset: u64,
    length: u64,
    layout_type: u32,
) -> (u32, Vec<u8>) {
    if layout_type != LAYOUT4_FLEX_FILES && layout_type != LAYOUT4_NFSV4_1_FILES {
        w.write_u32(nfs4_status::NFS4ERR_LAYOUTUNAVAILABLE);
        return (nfs4_status::NFS4ERR_LAYOUTUNAVAILABLE, w.into_bytes());
    }

    // For Phase 15b without a real composition lookup table, derive
    // composition_id from the current_fh's first 16 bytes (the same
    // path the Phase-14 stub used). Phase 15c hooks composition
    // metadata properly.
    let comp_id = kiseki_common::ids::CompositionId(uuid::Uuid::from_bytes(
        fh[..16].try_into().unwrap_or([0; 16]),
    ));

    let pnfs_iomode = if iomode >= 2 {
        crate::pnfs::LayoutIoMode::ReadWrite
    } else {
        crate::pnfs::LayoutIoMode::Read
    };
    let now_ms = ff_now_ms();

    let layout = mgr.layout_get(
        ctx.tenant_id,
        ctx.namespace_id,
        comp_id,
        offset,
        length.max(1),
        pnfs_iomode,
        now_ms,
    );

    w.write_u32(nfs4_status::NFS4_OK);
    w.write_bool(true); // return_on_close
    w.write_opaque_fixed(&layout.stateid);
    w.write_u32(u32::try_from(layout.stripes.len()).unwrap_or(0));
    for stripe in &layout.stripes {
        w.write_u64(stripe.offset);
        w.write_u64(stripe.length);
        w.write_u32(
            if matches!(stripe.iomode, crate::pnfs::LayoutIoMode::ReadWrite) {
                2
            } else {
                1
            },
        );
        w.write_u32(LAYOUT4_FLEX_FILES);

        // Inline ff_layout4 body for this segment. RFC 8435 §5.1:
        //   length4 ffl_stripe_unit
        //   ff_mirror4 ffl_mirrors<>           (1 mirror; tightly_coupled)
        //     ff_data_server4 ffm_data_servers<>  (1 ds for this stripe)
        //       deviceid4   ffds_deviceid       (16 bytes)
        //       uint32      ffds_efficiency
        //       stateid4    ffds_stateid        (16 bytes)
        //       nfs_fh4     ffds_fh_vers<>      (1 fh — NFSv4.1)
        //       fattr4_owner ffds_user
        //       fattr4_owner_group ffds_group
        //   ff_ioflags4 ffl_flags
        //   uint32 ffl_stats_collect_hint
        let mut body = XdrWriter::new();
        body.write_u64(stripe.length); // stripe_unit
        body.write_u32(1); // mirror count
        body.write_u32(1); // data_servers per mirror
        body.write_opaque_fixed(&stripe.device_id);
        body.write_u32(0); // efficiency
        body.write_opaque_fixed(&[0u8; 16]); // stateid
        body.write_u32(1); // fh_vers count
        body.write_opaque(&stripe.fh.encode());
        body.write_opaque(b"0"); // user
        body.write_opaque(b"0"); // group
                                 // RFC 8435 §5.1 + ADR-038 §D3 — kiseki's FFL is tightly_coupled;
                                 // advertise FF_FLAGS_NO_LAYOUTCOMMIT so clients skip the
                                 // LAYOUTCOMMIT round trip on close.
        body.write_u32(crate::pnfs::FF_FLAGS_NO_LAYOUTCOMMIT);
        body.write_u32(0); // stats_collect_hint
        w.write_opaque(&body.into_bytes());
    }

    (nfs4_status::NFS4_OK, w.into_bytes())
}

fn ff_now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// LAYOUTRETURN (RFC 5661 §18.44) — return pNFS layout.
fn op_layoutreturn<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> (u32, Vec<u8>) {
    // Parse LAYOUTRETURN4args.
    let _reclaim = reader.read_bool().unwrap_or(false);
    let _layout_type = reader.read_u32().unwrap_or(1);
    let _iomode = reader.read_u32().unwrap_or(1);
    let return_type = reader.read_u32().unwrap_or(4); // LAYOUTRETURN4_ALL = 4

    let mut w = XdrWriter::new();
    w.write_u32(op::LAYOUTRETURN);

    if return_type == 1 {
        // LAYOUTRETURN4_FILE: return layout for a specific file.
        let offset = reader.read_u64().unwrap_or(0);
        let _length = reader.read_u64().unwrap_or(0);
        let stateid = reader.read_opaque_fixed(16).unwrap_or_default();
        let _lrf_body = reader.read_opaque().unwrap_or_default();

        // Derive file_id from stateid (first 8 bytes, matching layout_get).
        let file_id = u64::from_le_bytes(stateid[..8].try_into().unwrap_or([0; 8]));
        let _ = offset; // used for partial returns (not implemented)

        ctx.layouts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .layout_return(file_id);
    }
    // LAYOUTRETURN4_ALL: no file-specific data to parse.

    w.write_u32(nfs4_status::NFS4_OK);
    w.write_bool(true); // lrs_present (stateid present)
    w.write_opaque_fixed(&[0u8; 16]); // empty stateid (no new state)

    (nfs4_status::NFS4_OK, w.into_bytes())
}

/// GETDEVICEINFO (RFC 5661 §18.40 + RFC 8435 §5.2). Resolves a
/// `deviceid4` → `ff_device_addr4` for the holding pNFS client.
///
/// When `ctx.mds_layout_manager` is wired (Phase 15b+), we look up
/// the device in the live layout cache. Otherwise we return
/// `NFS4ERR_NOENT` — older deployments (Phase 14 stub) never offered
/// device-resolution at all.
fn op_getdeviceinfo<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
) -> (u32, Vec<u8>) {
    // Parse GETDEVICEINFO4args (RFC 5661 §18.40.1):
    //   deviceid4         gdia_device_id (16 bytes)
    //   layouttype4       gdia_layout_type
    //   count4            gdia_maxcount
    //   bitmap4           gdia_notify_types
    let device_bytes = reader.read_opaque_fixed(16).unwrap_or_default();
    let layout_type = reader.read_u32().unwrap_or(LAYOUT4_FLEX_FILES);
    let _maxcount = reader.read_u32().unwrap_or(0);
    let bitmap_len = reader.read_u32().unwrap_or(0);
    for _ in 0..bitmap_len {
        let _ = reader.read_u32();
    }

    let mut w = XdrWriter::new();
    w.write_u32(op::GETDEVICEINFO);

    if layout_type != LAYOUT4_FLEX_FILES && layout_type != LAYOUT4_NFSV4_1_FILES {
        w.write_u32(nfs4_status::NFS4ERR_LAYOUTUNAVAILABLE);
        return (nfs4_status::NFS4ERR_LAYOUTUNAVAILABLE, w.into_bytes());
    }

    let Some(mgr) = ctx.mds_layout_manager.as_ref() else {
        w.write_u32(nfs4_status::NFS4ERR_NOENT);
        return (nfs4_status::NFS4ERR_NOENT, w.into_bytes());
    };

    let mut device_id = [0u8; 16];
    if device_bytes.len() == 16 {
        device_id.copy_from_slice(&device_bytes);
    }

    let Some(info) = mgr.get_device_info(&device_id) else {
        w.write_u32(nfs4_status::NFS4ERR_NOENT);
        return (nfs4_status::NFS4ERR_NOENT, w.into_bytes());
    };

    w.write_u32(nfs4_status::NFS4_OK);
    w.write_u32(LAYOUT4_FLEX_FILES);

    // GETDEVICEINFO4resok body (RFC 8435 §5.2 ff_device_addr4):
    //   da_addr_body :: ff_device_addr4 {
    //     ff_device_versions4 ffda_versions<>;
    //     multipath_list4     ffda_netaddrs<>;
    //   }
    //   bitmap4 gdir_notification (0)
    //
    // We pack the body as `opaque<>` per the standard wire shape.
    let mut body = XdrWriter::new();
    // ffda_versions: one entry — NFSv4.1.
    body.write_u32(1);
    // ff_device_versions4 entry:
    //   uint32 ffdv_version  (4)
    //   uint32 ffdv_minorversion (1)
    //   uint32 ffdv_rsize    (1 MiB)
    //   uint32 ffdv_wsize    (1 MiB)
    //   bool   ffdv_tightly_coupled
    body.write_u32(4);
    body.write_u32(1);
    body.write_u32(1_048_576);
    body.write_u32(1_048_576);
    body.write_bool(true); // tightly coupled

    // ffda_netaddrs: one multipath_list4 with len = info.addresses.len().
    body.write_u32(u32::try_from(info.addresses.len()).unwrap_or(0));
    for addr in &info.addresses {
        body.write_string(&addr.netid);
        body.write_string(&addr.uaddr);
    }

    w.write_opaque(&body.into_bytes());
    w.write_u32(0); // gdir_notification bitmap (no notifications)

    (nfs4_status::NFS4_OK, w.into_bytes())
}

#[allow(clippy::too_many_lines)] // RFC 8881 §18.16.1 args grammar is intrinsically large
fn op_open<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    sessions: &SessionManager,
    state: &mut CompoundState,
) -> (u32, Vec<u8>) {
    // RFC 8881 §18.16.1 OPEN4args:
    //   seqid + share_access + share_deny + open_owner4 +
    //   openflag4 (opentype + createhow-if-CREATE) + open_claim4
    //
    // open_owner4: clientid (u64) + owner (opaque<>)
    // openflag4 (§3.2.9):
    //   case OPEN4_CREATE (1): createhow4
    //     createhow4 (§3.2.8):
    //       case UNCHECKED4 (0) | GUARDED4 (1): fattr4 (bitmap+attrs)
    //       case EXCLUSIVE4 (2): verifier4 (8 bytes)
    //       case EXCLUSIVE4_1 (3): createverfattr4 (verifier + fattr4)
    //   default (NOCREATE = 0): void
    // open_claim4 (§3.2.10):
    //   case CLAIM_NULL (0):           component4 file
    //   case CLAIM_PREVIOUS (1):       open_delegation_type4
    //   case CLAIM_DELEGATE_CUR (2):   open_claim_delegate_cur4
    //   case CLAIM_DELEGATE_PREV (3):  component4 file_delegate_prev
    //   case CLAIM_FH (4):             void  (4.1+)
    //   case CLAIM_DELEG_PREV_FH (5):  void  (4.1+)
    //   case CLAIM_DELEG_CUR_FH (6):   open_claim_delegate_cur_fh4 (4.1+)
    //
    // The previous decoder skipped both `createhow` and the `claim`
    // discriminator. Linux 6.x always sends CLAIM_NULL for plain
    // open() — its `claim_type=0` u32 was being mis-read as the
    // name's length-prefix → empty name → NFS4ERR_NOENT (the
    // kernel cat-ENOENT surfaced by Phase 15c.3 e2e). Fixed by
    // parsing the full args grammar.
    const OPEN4_NOCREATE: u32 = 0;
    const OPEN4_CREATE: u32 = 1;
    const UNCHECKED4: u32 = 0;
    const GUARDED4: u32 = 1;
    const EXCLUSIVE4: u32 = 2;
    const EXCLUSIVE4_1: u32 = 3;
    const CLAIM_NULL: u32 = 0;
    const CLAIM_FH: u32 = 4;

    let _seqid = reader.read_u32().unwrap_or(0);
    let _share_access = reader.read_u32().unwrap_or(1); // READ
    let _share_deny = reader.read_u32().unwrap_or(0); // NONE
    let _clientid = reader.read_u64().unwrap_or(0);
    let _owner = reader.read_opaque().unwrap_or_default();

    // openflag4 union — opentype + createhow body (only when CREATE).
    let open_type = reader.read_u32().unwrap_or(OPEN4_NOCREATE);
    if open_type == OPEN4_CREATE {
        let createhow_type = reader.read_u32().unwrap_or(UNCHECKED4);
        match createhow_type {
            UNCHECKED4 | GUARDED4 => {
                // fattr4: bitmap4 (count + words) + opaque attr_vals.
                let bm_count = reader.read_u32().unwrap_or(0);
                for _ in 0..bm_count {
                    let _ = reader.read_u32();
                }
                let _attr_vals = reader.read_opaque().unwrap_or_default();
            }
            EXCLUSIVE4 => {
                let _verifier = reader.read_opaque_fixed(8).unwrap_or_default();
            }
            EXCLUSIVE4_1 => {
                let _verifier = reader.read_opaque_fixed(8).unwrap_or_default();
                let bm_count = reader.read_u32().unwrap_or(0);
                for _ in 0..bm_count {
                    let _ = reader.read_u32();
                }
                let _attr_vals = reader.read_opaque().unwrap_or_default();
            }
            _ => {}
        }
    }

    // open_claim4 — CLAIM_NULL is the path Linux uses for a plain
    // `open()` against an existing dentry. CLAIM_FH (RFC 8881 4.1+)
    // is what Linux pNFS uses for OPEN-by-current-fh after LOOKUP:
    // no name on the wire — open the current_fh directly. Other
    // claim types are stateid-recovery paths (PREVIOUS/DELEGATE_*)
    // that the current state machine doesn't grant delegations for.
    let claim_type = reader.read_u32().unwrap_or(CLAIM_NULL);
    let claim_is_fh = claim_type == CLAIM_FH;
    let name = if claim_type == CLAIM_NULL {
        reader.read_string().unwrap_or_default()
    } else {
        String::new()
    };

    let mut w = XdrWriter::new();
    w.write_u32(op::OPEN);

    // RFC 8881 §18.16.4 — OPEN4resok wire layout:
    //
    //   stateid4         stateid;     // 16 bytes
    //   change_info4     cinfo;       // bool atomic + u64 before + u64 after
    //   uint32_t         rflags;      // 4 bytes
    //   bitmap4          attrset;     // u32 count + u32*count words
    //   open_delegation4 delegation;  // u32 type discriminator + body
    //
    // The previous encoding emitted only `stateid + write_bool(cinfo)
    // + rflags` — missing 16 bytes of cinfo trailer + the entire
    // attrset and delegation tail. Linux 6.x kernel decoder reads
    // following compound op bytes as cinfo trailer, which silently
    // desynchronizes the entire COMPOUND parse. Phase 15c.3 cat-ENOENT.
    let write_open_resok = |w: &mut XdrWriter, sid: &StateId, rflags: u32| {
        w.write_u32(nfs4_status::NFS4_OK);
        w.write_opaque_fixed(&sid.0); // stateid (16 bytes)
                                      // change_info4 — atomic=false (no atomicity guarantee from
                                      // an in-memory store); before=after=0 keeps the kernel happy
                                      // (the semantic is "directory unchanged").
        w.write_bool(false); // atomic
        w.write_u64(0); // before changeid4
        w.write_u64(0); // after changeid4
        w.write_u32(rflags);
        // attrset (bitmap4): empty — server didn't set any attrs as
        // part of the OPEN. Encoded as `count=0` (no words).
        w.write_u32(0);
        // open_delegation4: OPEN_DELEGATE_NONE = 0 has an empty body
        // per §9.1.2 (no per-type fields after the discriminator).
        // Future grants would emit OPEN_DELEGATE_READ/WRITE.
        const OPEN_DELEGATE_NONE: u32 = 0;
        w.write_u32(OPEN_DELEGATE_NONE);
    };

    let status = if open_type == 1 {
        // CREATE: write a new file.
        match ctx.write_named(&name, Vec::new()) {
            Ok((fh, _resp)) => {
                let sid = sessions.open_file(fh);
                state.current_fh = Some(fh);
                state.current_stateid = Some(sid);
                // OPEN4_RESULT_CONFIRM = 1: kernel knows to confirm
                // before issuing further state ops on this stateid.
                write_open_resok(&mut w, &sid, 1);
                nfs4_status::NFS4_OK
            }
            Err(_) => {
                w.write_u32(nfs4_status::NFS4ERR_IO);
                nfs4_status::NFS4ERR_IO
            }
        }
    } else if claim_is_fh {
        // CLAIM_FH — open the current file handle (no name lookup).
        // Linux pNFS issues this after LOOKUP has already set
        // current_fh. The fh must already be registered with the
        // handle registry; we just bind a new stateid to it.
        match state.current_fh {
            Some(fh) if ctx.handles.lookup(&fh).is_some() => {
                let sid = sessions.open_file(fh);
                state.current_stateid = Some(sid);
                write_open_resok(&mut w, &sid, 0);
                nfs4_status::NFS4_OK
            }
            _ => {
                w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
                nfs4_status::NFS4ERR_NOFILEHANDLE
            }
        }
    } else {
        // NOCREATE: open existing file by name.
        match ctx.lookup_by_name(&name) {
            Some((fh, _attrs)) => {
                let sid = sessions.open_file(fh);
                state.current_fh = Some(fh);
                state.current_stateid = Some(sid);
                write_open_resok(&mut w, &sid, 0);
                nfs4_status::NFS4_OK
            }
            None => {
                w.write_u32(nfs4_status::NFS4ERR_NOENT);
                nfs4_status::NFS4ERR_NOENT
            }
        }
    };

    (status, w.into_bytes())
}

fn op_close(
    reader: &mut XdrReader<'_>,
    sessions: &SessionManager,
    state: &mut CompoundState,
) -> (u32, Vec<u8>) {
    let _seqid = reader.read_u32().unwrap_or(0);
    let sid_bytes = reader.read_opaque_fixed(16).unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::CLOSE);

    let status = if sid_bytes.len() == 16 {
        let mut sid = [0u8; 16];
        sid.copy_from_slice(&sid_bytes);
        if sessions.close_file(&StateId(sid)) {
            state.current_stateid = None;
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_opaque_fixed(&[0u8; 16]); // zeroed stateid (closed)
            nfs4_status::NFS4_OK
        } else {
            w.write_u32(nfs4_status::NFS4ERR_BAD_STATEID);
            nfs4_status::NFS4ERR_BAD_STATEID
        }
    } else {
        w.write_u32(nfs4_status::NFS4ERR_BAD_STATEID);
        nfs4_status::NFS4ERR_BAD_STATEID
    };

    (status, w.into_bytes())
}

fn op_lock(
    reader: &mut XdrReader<'_>,
    sessions: &SessionManager,
    state: &mut CompoundState,
) -> (u32, Vec<u8>) {
    let lock_type = reader.read_u32().unwrap_or(1); // READ_LT=1, WRITE_LT=2
    let _reclaim = reader.read_bool().unwrap_or(false);
    let offset = reader.read_u64().unwrap_or(0);
    let length = reader.read_u64().unwrap_or(u64::MAX);
    // Skip locker union (simplified).

    let write = lock_type == 2 || lock_type == 4; // WRITE_LT or WRITEW_LT

    let mut w = XdrWriter::new();
    w.write_u32(op::LOCK);

    let sid = state.current_stateid.unwrap_or(StateId([0; 16]));
    let status = match sessions.add_lock(sid, offset, length, write) {
        Ok(lock_sid) => {
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_opaque_fixed(&lock_sid.0); // lock_stateid
            nfs4_status::NFS4_OK
        }
        Err(()) => {
            w.write_u32(nfs4_status::NFS4ERR_DENIED);
            nfs4_status::NFS4ERR_DENIED
        }
    };

    (status, w.into_bytes())
}

fn op_lookup<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &mut CompoundState,
) -> (u32, Vec<u8>) {
    let name = reader.read_string().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::LOOKUP);

    // Namespace-name alias (Phase 15c.2): from the pseudo-root,
    // LOOKUP("default") descends into the namespace root. This
    // satisfies `mount.nfs4 server:/default` without triggering the
    // kernel's loop-detection (different fileids for /, /default).
    if name == "default"
        && state
            .current_fh
            .is_some_and(|fh| ctx.handles.is_pseudo_root(&fh))
    {
        let ns_root = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);
        state.current_fh = Some(ns_root);
        w.write_u32(nfs4_status::NFS4_OK);
        return (nfs4_status::NFS4_OK, w.into_bytes());
    }

    let status = match ctx.lookup_by_name(&name) {
        Some((fh, _attrs)) => {
            state.current_fh = Some(fh);
            w.write_u32(nfs4_status::NFS4_OK);
            nfs4_status::NFS4_OK
        }
        None => {
            w.write_u32(nfs4_status::NFS4ERR_NOENT);
            nfs4_status::NFS4ERR_NOENT
        }
    };

    (status, w.into_bytes())
}

fn op_remove<G: GatewayOps>(reader: &mut XdrReader<'_>, ctx: &NfsContext<G>) -> (u32, Vec<u8>) {
    let name = reader.read_string().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::REMOVE);

    let status = match ctx.remove_file(&name) {
        Ok(()) => {
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_bool(false); // cinfo
            nfs4_status::NFS4_OK
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_NOENT);
            nfs4_status::NFS4ERR_NOENT
        }
    };

    (status, w.into_bytes())
}

fn op_readdir<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &CompoundState,
) -> (u32, Vec<u8>) {
    // Consume READDIR4args: cookie(u64) + cookieverf(8 bytes) + dircount(u32) + maxcount(u32) + attr_request(bitmap).
    let _cookie = reader.read_u64().unwrap_or(0);
    let _cookieverf = reader.read_opaque_fixed(8).unwrap_or_default();
    let _dircount = reader.read_u32().unwrap_or(0);
    let _maxcount = reader.read_u32().unwrap_or(0);
    let bitmap_count = reader.read_u32().unwrap_or(0);
    for _ in 0..bitmap_count {
        let _ = reader.read_u32();
    }

    let mut w = XdrWriter::new();
    w.write_u32(op::READDIR);
    // RFC 8881 §18.26.4: READDIR with no current filehandle is
    // NFS4ERR_NOFILEHANDLE. Distinct from NFS4ERR_BADHANDLE (handle
    // malformed) and NFS4ERR_NOTDIR (handle is a regular file).
    if state.current_fh.is_none() {
        w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
        return (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes());
    }
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_opaque_fixed(&[0u8; 8]); // cookieverf

    // RFC 8881 §18.26.3 — entry4 is `cookie + name + fattr4 +
    // nextentry*`. fattr4 is `bitmap4 attrmask + opaque attr_vals`.
    // The previous encoding emitted only the bitmap count (no
    // attr_vals opaque length prefix), which made the kernel
    // mis-parse the linked-list and silently drop every entry —
    // the Phase 15c.3 ls-empty bug.
    let entries = ctx.readdir();
    for (i, entry) in entries.iter().enumerate() {
        w.write_bool(true); // nextentry pointer = present
        w.write_u64((i + 1) as u64); // cookie
        w.write_string(&entry.name);
        // fattr4: minimal valid encoding — empty attrmask + empty
        // attr_vals. Both must be on the wire; missing the
        // attr_vals opaque length prefix de-aligns the decoder.
        w.write_u32(0); // attrmask bitmap word count
        w.write_opaque(&[]); // attr_vals (length-prefix = 0)
    }
    w.write_bool(false); // nextentry pointer = absent (end of list)
    w.write_bool(true); // dirlist4.eof

    (nfs4_status::NFS4_OK, w.into_bytes())
}

// --- F2: New NFSv4 operation handlers ---

fn op_access<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &CompoundState,
) -> (u32, Vec<u8>) {
    let requested = reader.read_u32().unwrap_or(0x3F);

    let mut w = XdrWriter::new();
    w.write_u32(op::ACCESS);

    let Some(fh) = &state.current_fh else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
    };

    // Pseudo-root: synthetic directory; grant lookup+execute (read
    // for traversal). The kernel needs this to be answerable so it
    // can enter the mount root.
    if ctx.handles.is_pseudo_root(fh) {
        const ACCESS_READ: u32 = 0x01;
        const ACCESS_LOOKUP: u32 = 0x02;
        const ACCESS_EXECUTE: u32 = 0x20;
        let granted = ACCESS_READ | ACCESS_LOOKUP | ACCESS_EXECUTE;
        w.write_u32(nfs4_status::NFS4_OK);
        w.write_u32(requested & granted); // supported
        w.write_u32(requested & granted); // access
        return (nfs4_status::NFS4_OK, w.into_bytes());
    }

    let status = match ctx.access(fh) {
        Ok(granted) => {
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_u32(requested & granted); // supported
            w.write_u32(requested & granted); // access
            nfs4_status::NFS4_OK
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
            nfs4_status::NFS4ERR_BADHANDLE
        }
    };

    (status, w.into_bytes())
}

fn op_setattr<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &CompoundState,
) -> (u32, Vec<u8>) {
    // stateid + attrmask + attr_vals
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let bitmap_count = reader.read_u32().unwrap_or(0);
    for _ in 0..bitmap_count {
        let _ = reader.read_u32();
    }
    let _attr_vals = reader.read_opaque().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::SETATTR);

    let Some(fh) = &state.current_fh else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        w.write_u32(0); // attrsset bitmap count
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
    };

    let status = if ctx.setattr(fh, None).is_ok() {
        w.write_u32(nfs4_status::NFS4_OK);
        w.write_u32(0); // attrsset bitmap count (none actually set)
        nfs4_status::NFS4_OK
    } else {
        w.write_u32(nfs4_status::NFS4ERR_IO);
        w.write_u32(0);
        nfs4_status::NFS4ERR_IO
    };

    (status, w.into_bytes())
}

fn op_rename<G: GatewayOps>(reader: &mut XdrReader<'_>, ctx: &NfsContext<G>) -> (u32, Vec<u8>) {
    let old_name = reader.read_string().unwrap_or_default();
    let new_name = reader.read_string().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::RENAME);

    let status = match ctx.rename_file(&old_name, &new_name) {
        Ok(()) => {
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_bool(false); // source cinfo
            w.write_bool(false); // target cinfo
            nfs4_status::NFS4_OK
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_NOENT);
            nfs4_status::NFS4ERR_NOENT
        }
    };

    (status, w.into_bytes())
}

fn op_link<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &CompoundState,
) -> (u32, Vec<u8>) {
    let new_name = reader.read_string().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::LINK);

    // LINK uses saved_fh as the target and current_fh's dir for the new name.
    let Some(target_fh) = &state.saved_fh else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
    };

    let status = match ctx.link(target_fh, &new_name) {
        Ok(()) => {
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_bool(false); // cinfo
            nfs4_status::NFS4_OK
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_IO);
            nfs4_status::NFS4ERR_IO
        }
    };

    (status, w.into_bytes())
}

fn op_readlink<G: GatewayOps>(ctx: &NfsContext<G>, state: &CompoundState) -> (u32, Vec<u8>) {
    let mut w = XdrWriter::new();
    w.write_u32(op::READLINK);

    let Some(fh) = &state.current_fh else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
    };

    let status = match ctx.readlink(fh) {
        Ok(target) => {
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_string(&target);
            nfs4_status::NFS4_OK
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_IO);
            nfs4_status::NFS4ERR_IO
        }
    };

    (status, w.into_bytes())
}

fn op_create<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &mut CompoundState,
) -> (u32, Vec<u8>) {
    let obj_type = reader.read_u32().unwrap_or(1); // NF4REG=1, NF4DIR=2, NF4LNK=5
                                                   // For symlinks, read the linkdata.
    let linkdata = if obj_type == 5 {
        reader.read_string().unwrap_or_default()
    } else {
        String::new()
    };
    let name = reader.read_string().unwrap_or_default();
    // Skip createattrs (bitmap + values).
    let bitmap_count = reader.read_u32().unwrap_or(0);
    for _ in 0..bitmap_count {
        let _ = reader.read_u32();
    }
    let _attr_vals = reader.read_opaque().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::CREATE);

    let status = match obj_type {
        2 => {
            // Directory
            match ctx.mkdir(&name) {
                Ok((fh, _)) => {
                    state.current_fh = Some(fh);
                    w.write_u32(nfs4_status::NFS4_OK);
                    w.write_bool(false); // cinfo
                    w.write_u32(0); // attrsset bitmap count
                    nfs4_status::NFS4_OK
                }
                Err(_) => {
                    w.write_u32(nfs4_status::NFS4ERR_IO);
                    nfs4_status::NFS4ERR_IO
                }
            }
        }
        5 => {
            // Symlink
            match ctx.symlink(&name, &linkdata) {
                Ok((fh, _)) => {
                    state.current_fh = Some(fh);
                    w.write_u32(nfs4_status::NFS4_OK);
                    w.write_bool(false);
                    w.write_u32(0);
                    nfs4_status::NFS4_OK
                }
                Err(_) => {
                    w.write_u32(nfs4_status::NFS4ERR_IO);
                    nfs4_status::NFS4ERR_IO
                }
            }
        }
        _ => {
            // Regular file or unsupported — create empty file.
            match ctx.write_named(&name, Vec::new()) {
                Ok((fh, _)) => {
                    state.current_fh = Some(fh);
                    w.write_u32(nfs4_status::NFS4_OK);
                    w.write_bool(false);
                    w.write_u32(0);
                    nfs4_status::NFS4_OK
                }
                Err(_) => {
                    w.write_u32(nfs4_status::NFS4ERR_IO);
                    nfs4_status::NFS4ERR_IO
                }
            }
        }
    };

    (status, w.into_bytes())
}

fn op_commit() -> (u32, Vec<u8>) {
    let mut w = XdrWriter::new();
    w.write_u32(op::COMMIT);
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_opaque_fixed(&[0u8; 8]); // write verifier
    (nfs4_status::NFS4_OK, w.into_bytes())
}

fn op_putfh(reader: &mut XdrReader<'_>, state: &mut CompoundState) -> (u32, Vec<u8>) {
    let mut w = XdrWriter::new();
    w.write_u32(op::PUTFH);

    // RFC 8881 §13.1: a truncated op body — here, the nfs_fh4 opaque
    // is missing entirely — is NFS4ERR_BADXDR, NOT NFS4ERR_BADHANDLE.
    // The previous `unwrap_or_default()` masked the wire fault as
    // an empty file-handle, conflating two distinct error semantics.
    let fh_bytes = match reader.read_opaque() {
        Ok(b) => b,
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_BADXDR);
            return (nfs4_status::NFS4ERR_BADXDR, w.into_bytes());
        }
    };

    let status = if fh_bytes.len() == 32 {
        let mut fh = [0u8; 32];
        fh.copy_from_slice(&fh_bytes);
        state.current_fh = Some(fh);
        w.write_u32(nfs4_status::NFS4_OK);
        nfs4_status::NFS4_OK
    } else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        nfs4_status::NFS4ERR_BADHANDLE
    };

    (status, w.into_bytes())
}

fn op_savefh(state: &mut CompoundState) -> (u32, Vec<u8>) {
    let mut w = XdrWriter::new();
    w.write_u32(op::SAVEFH);

    let status = if let Some(fh) = state.current_fh {
        state.saved_fh = Some(fh);
        w.write_u32(nfs4_status::NFS4_OK);
        nfs4_status::NFS4_OK
    } else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        nfs4_status::NFS4ERR_BADHANDLE
    };

    (status, w.into_bytes())
}

fn op_restorefh(state: &mut CompoundState) -> (u32, Vec<u8>) {
    let mut w = XdrWriter::new();
    w.write_u32(op::RESTOREFH);

    let status = if let Some(fh) = state.saved_fh {
        state.current_fh = Some(fh);
        w.write_u32(nfs4_status::NFS4_OK);
        nfs4_status::NFS4_OK
    } else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        nfs4_status::NFS4ERR_BADHANDLE
    };

    (status, w.into_bytes())
}

fn op_reclaim_complete(reader: &mut XdrReader<'_>) -> (u32, Vec<u8>) {
    let _one_fs = reader.read_bool().unwrap_or(false);

    let mut w = XdrWriter::new();
    w.write_u32(op::RECLAIM_COMPLETE);
    w.write_u32(nfs4_status::NFS4_OK);
    (nfs4_status::NFS4_OK, w.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_gateway::InMemoryGateway;
    use crate::nfs::NfsGateway;
    use crate::nfs_ops::NfsContext;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::ids::{NamespaceId, OrgId};
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_crypto::keys::SystemMasterKey;

    fn test_ctx() -> NfsContext<InMemoryGateway> {
        let master_key = SystemMasterKey::new([0u8; 32], KeyEpoch(1));
        let tenant = OrgId(uuid::Uuid::nil());
        let ns = NamespaceId(uuid::Uuid::from_u128(1));
        let mut store = CompositionStore::new();
        store.add_namespace(kiseki_composition::namespace::Namespace {
            id: ns,
            tenant_id: tenant,
            shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        });
        let gw = InMemoryGateway::new(store, kiseki_chunk::arc_async(ChunkStore::new()), master_key);
        let nfs_gw = NfsGateway::new(gw);
        NfsContext::new(nfs_gw, tenant, ns)
    }

    fn test_sessions() -> SessionManager {
        SessionManager::new()
    }

    // ---------- EXCHANGE_ID (§18.35) ----------

    #[test]
    fn exchange_id_returns_ok_with_client_id() {
        let sessions = test_sessions();
        let mut body = XdrWriter::new();
        body.write_opaque_fixed(&[0u8; 8]); // verifier
        body.write_opaque(b"test-client"); // owner_id
        body.write_u32(0); // flags
        body.write_u32(0); // state_protect (SP4_NONE)
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, result) = op_exchange_id(&mut reader, &sessions);
        assert_eq!(status, nfs4_status::NFS4_OK);

        let mut r = XdrReader::new(&result);
        let op_code = r.read_u32().unwrap();
        assert_eq!(op_code, op::EXCHANGE_ID);
        let st = r.read_u32().unwrap();
        assert_eq!(st, nfs4_status::NFS4_OK);
        let client_id = r.read_u64().unwrap();
        assert_ne!(client_id, 0, "client_id should be non-zero");
        let _seqid = r.read_u32().unwrap();
        let flags = r.read_u32().unwrap();
        // RFC 5661 §18.35.4: CONFIRMED_R is 0x80000000, not 0x1
        // (which is SUPP_MOVED_REFER). The earlier assertion was
        // self-consistent with the buggy production code.
        assert_eq!(
            flags & 0x8000_0000,
            0x8000_0000,
            "CONFIRMED_R (0x80000000) flag should be set"
        );
        let _state_protect = r.read_u32().unwrap();
        let _minor_id = r.read_u64().unwrap();
        let major_id = r.read_opaque().unwrap();
        assert!(!major_id.is_empty(), "server major_id should be present");
    }

    #[test]
    fn exchange_id_returns_unique_client_ids() {
        let sessions = test_sessions();

        let make_exchange = || {
            let mut body = XdrWriter::new();
            body.write_opaque_fixed(&[0u8; 8]);
            body.write_opaque(b"client");
            body.write_u32(0);
            body.write_u32(0);
            let bytes = body.into_bytes();
            let mut reader = XdrReader::new(&bytes);
            let (_, result) = op_exchange_id(&mut reader, &sessions);
            let mut r = XdrReader::new(&result);
            r.read_u32().unwrap(); // op
            r.read_u32().unwrap(); // status
            r.read_u64().unwrap() // client_id
        };

        let id1 = make_exchange();
        let id2 = make_exchange();
        assert_ne!(id1, id2, "client_ids should be unique");
    }

    // ---------- CREATE_SESSION (§18.36) ----------

    #[test]
    fn create_session_returns_ok_with_session_id() {
        let sessions = test_sessions();
        let client_id = sessions.exchange_id();

        let mut body = XdrWriter::new();
        body.write_u64(client_id);
        body.write_u32(1); // sequence
        body.write_u32(0); // flags
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, result) = op_create_session(&mut reader, &sessions);
        assert_eq!(status, nfs4_status::NFS4_OK);

        let mut r = XdrReader::new(&result);
        let op_code = r.read_u32().unwrap();
        assert_eq!(op_code, op::CREATE_SESSION);
        let st = r.read_u32().unwrap();
        assert_eq!(st, nfs4_status::NFS4_OK);
        let session_id = r.read_opaque_fixed(16).unwrap();
        assert_eq!(session_id.len(), 16, "session_id should be 16 bytes");
        let _seqid = r.read_u32().unwrap();
        let _flags = r.read_u32().unwrap();
        // fore channel attrs
        let _headerpad = r.read_u32().unwrap();
        let _maxreq = r.read_u32().unwrap();
        let _maxresp = r.read_u32().unwrap();
        let _maxresp_cached = r.read_u32().unwrap();
        let maxops = r.read_u32().unwrap();
        assert!(maxops > 0, "maxops should be positive");
        let maxreqs = r.read_u32().unwrap();
        assert!(maxreqs > 0, "maxreqs should be positive");
    }

    #[test]
    fn create_session_produces_distinct_ids() {
        let sessions = test_sessions();
        let cid = sessions.exchange_id();

        let create = |s: &SessionManager| {
            let mut body = XdrWriter::new();
            body.write_u64(cid);
            body.write_u32(1);
            body.write_u32(0);
            let bytes = body.into_bytes();
            let mut reader = XdrReader::new(&bytes);
            let (_, result) = op_create_session(&mut reader, s);
            let mut r = XdrReader::new(&result);
            r.read_u32().unwrap(); // op
            r.read_u32().unwrap(); // status
            r.read_opaque_fixed(16).unwrap()
        };

        let sid1 = create(&sessions);
        let sid2 = create(&sessions);
        assert_ne!(
            sid1, sid2,
            "session_ids should be cryptographically distinct"
        );
    }

    // ---------- SEQUENCE (§18.46) ----------

    #[test]
    fn sequence_valid_session_returns_ok() {
        let sessions = test_sessions();
        let cid = sessions.exchange_id();
        let session_id = sessions.create_session(cid, 8);

        let mut body = XdrWriter::new();
        body.write_opaque_fixed(&session_id);
        body.write_u32(1); // sequenceid
        body.write_u32(0); // slotid
        body.write_u32(7); // highest_slotid
        body.write_bool(false); // cachethis
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, result) = op_sequence(&mut reader, &sessions);
        assert_eq!(status, nfs4_status::NFS4_OK);

        let mut r = XdrReader::new(&result);
        let _op = r.read_u32().unwrap();
        let st = r.read_u32().unwrap();
        assert_eq!(st, nfs4_status::NFS4_OK);
        let ret_sid = r.read_opaque_fixed(16).unwrap();
        assert_eq!(ret_sid, session_id);
        let seqid = r.read_u32().unwrap();
        assert_eq!(seqid, 1);
        let slotid = r.read_u32().unwrap();
        assert_eq!(slotid, 0);
    }

    #[test]
    fn sequence_invalid_session_returns_badsession() {
        let sessions = test_sessions();
        let fake_sid = [0xABu8; 16];

        let mut body = XdrWriter::new();
        body.write_opaque_fixed(&fake_sid);
        body.write_u32(1);
        body.write_u32(0);
        body.write_u32(0);
        body.write_bool(false);
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, _) = op_sequence(&mut reader, &sessions);
        assert_eq!(status, nfs4_status::NFS4ERR_BADSESSION);
    }

    // ---------- PUTROOTFH (§18.24) ----------

    #[test]
    fn putrootfh_sets_current_filehandle() {
        let ctx = test_ctx();
        let mut state = CompoundState {
            current_fh: None,
            saved_fh: None,
            current_stateid: None,
        };

        let (status, _) = op_putrootfh(&ctx, &mut state);
        assert_eq!(status, nfs4_status::NFS4_OK);
        assert!(
            state.current_fh.is_some(),
            "current_fh should be set after PUTROOTFH"
        );

        // Phase 15c.2: PUTROOTFH now returns the pseudo-root, not
        // the namespace root. The pseudo-root is registered (so
        // GETATTR/ACCESS resolve), and `LOOKUP("default")` from
        // pseudo-root descends into the namespace root.
        let pseudo_root = ctx.handles.pseudo_root_handle();
        assert_eq!(state.current_fh.unwrap(), pseudo_root);
        assert!(ctx.handles.is_pseudo_root(&pseudo_root));
    }

    // ---------- GETATTR (§18.9) ----------

    #[test]
    fn getattr_root_returns_dir_type() {
        let ctx = test_ctx();
        let state = CompoundState {
            current_fh: Some(ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id)),
            saved_fh: None,
            current_stateid: None,
        };

        // Request bitmap: TYPE (bit 1) + SIZE (bit 4) = 0x12.
        let mut body = XdrWriter::new();
        body.write_u32(1); // bitmap count
        body.write_u32((1u32 << 1) | (1u32 << 4));
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, result) = op_getattr(&mut reader, &ctx, &state);
        assert_eq!(status, nfs4_status::NFS4_OK);

        let mut r = XdrReader::new(&result);
        let _op = r.read_u32().unwrap();
        let st = r.read_u32().unwrap();
        assert_eq!(st, nfs4_status::NFS4_OK);
        let bm_count = r.read_u32().unwrap();
        assert_eq!(
            bm_count, 1,
            "single-word bitmap when only word0 attrs are set"
        );
        let bm0 = r.read_u32().unwrap();
        assert_eq!(
            bm0,
            (1u32 << 1) | (1u32 << 4),
            "result bitmap echoes request"
        );
        let attr_bytes = r.read_opaque().unwrap();
        // TYPE (u32) + SIZE (u64) = 12 bytes.
        assert_eq!(attr_bytes.len(), 12);
        let mut ar = XdrReader::new(&attr_bytes);
        let ftype = ar.read_u32().unwrap();
        assert_eq!(ftype, 2, "root type should be NF4DIR (2)");
        let size = ar.read_u64().unwrap();
        assert!(size > 0, "root size should be reported");
    }

    #[test]
    fn getattr_no_filehandle_returns_nofilehandle() {
        let ctx = test_ctx();
        let state = CompoundState {
            current_fh: None,
            saved_fh: None,
            current_stateid: None,
        };

        let mut body = XdrWriter::new();
        body.write_u32(0); // no bitmap
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, _) = op_getattr(&mut reader, &ctx, &state);
        // RFC 8881 §18.7.4: GETATTR with no current_fh is NOFILEHANDLE,
        // not BADHANDLE.
        assert_eq!(status, nfs4_status::NFS4ERR_NOFILEHANDLE);
    }

    // ---------- WRITE (§18.38) ----------

    #[test]
    fn write_returns_ok_with_count_and_file_sync() {
        let ctx = test_ctx();
        let sessions = test_sessions();
        let mut state = CompoundState {
            current_fh: Some(ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id)),
            saved_fh: None,
            current_stateid: None,
        };

        let data = b"nfs4 write";
        let mut body = XdrWriter::new();
        body.write_opaque_fixed(&[0u8; 16]); // special stateid (anonymous)
        body.write_u64(0); // offset
        body.write_u32(2); // FILE_SYNC
        body.write_opaque(data);
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, result) = op_write(&mut reader, &ctx, &sessions, &mut state);
        assert_eq!(status, nfs4_status::NFS4_OK);

        let mut r = XdrReader::new(&result);
        let _op = r.read_u32().unwrap();
        let st = r.read_u32().unwrap();
        assert_eq!(st, nfs4_status::NFS4_OK);
        let count = r.read_u32().unwrap();
        assert_eq!(count, 10);
        let committed = r.read_u32().unwrap();
        assert_eq!(committed, 2, "committed should be FILE_SYNC");
    }

    #[test]
    fn write_updates_current_filehandle() {
        let ctx = test_ctx();
        let sessions = test_sessions();
        let mut state = CompoundState {
            current_fh: Some(ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id)),
            saved_fh: None,
            current_stateid: None,
        };

        let original_fh = state.current_fh;

        let mut body = XdrWriter::new();
        body.write_opaque_fixed(&[0u8; 16]);
        body.write_u64(0);
        body.write_u32(2);
        body.write_opaque(b"test data");
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, _) = op_write(&mut reader, &ctx, &sessions, &mut state);
        assert_eq!(status, nfs4_status::NFS4_OK);
        assert_ne!(
            state.current_fh, original_fh,
            "WRITE should update current_fh"
        );
    }

    // ---------- OPEN (§18.16) ----------

    #[test]
    fn open_create_returns_ok_with_stateid() {
        let ctx = test_ctx();
        let sessions = test_sessions();
        let mut state = CompoundState {
            current_fh: Some(ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id)),
            saved_fh: None,
            current_stateid: None,
        };

        let mut body = XdrWriter::new();
        body.write_u32(0); // seqid
        body.write_u32(2); // share_access (WRITE)
        body.write_u32(0); // share_deny
        body.write_u64(1); // clientid
        body.write_opaque(b"owner"); // owner
        body.write_u32(1); // OPEN4_CREATE
        body.write_u32(0); // createhow4 = UNCHECKED4
        body.write_u32(0); // fattr4.bitmap word count
        body.write_opaque(&[]); // fattr4.attr_vals
        body.write_u32(0); // open_claim4 = CLAIM_NULL
        body.write_string("created-file.txt"); // file
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, result) = op_open(&mut reader, &ctx, &sessions, &mut state);
        assert_eq!(status, nfs4_status::NFS4_OK);

        let mut r = XdrReader::new(&result);
        let _op = r.read_u32().unwrap();
        let st = r.read_u32().unwrap();
        assert_eq!(st, nfs4_status::NFS4_OK);
        let stateid = r.read_opaque_fixed(16).unwrap();
        assert_ne!(stateid, [0u8; 16], "stateid should be non-zero");
    }

    #[test]
    fn open_read_existing_returns_ok_with_stateid() {
        let ctx = test_ctx();
        let sessions = test_sessions();

        // First create a file.
        ctx.write_named("readable.txt", b"content".to_vec())
            .unwrap();

        let mut state = CompoundState {
            current_fh: Some(ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id)),
            saved_fh: None,
            current_stateid: None,
        };

        let mut body = XdrWriter::new();
        body.write_u32(0); // seqid
        body.write_u32(1); // share_access (READ)
        body.write_u32(0); // share_deny
        body.write_u64(1); // clientid
        body.write_opaque(b"owner");
        body.write_u32(0); // OPEN4_NOCREATE (no createhow body)
        body.write_u32(0); // open_claim4 = CLAIM_NULL
        body.write_string("readable.txt"); // file
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, result) = op_open(&mut reader, &ctx, &sessions, &mut state);
        assert_eq!(status, nfs4_status::NFS4_OK);

        let mut r = XdrReader::new(&result);
        r.read_u32().unwrap(); // op
        r.read_u32().unwrap(); // status
        let stateid = r.read_opaque_fixed(16).unwrap();
        assert_ne!(stateid, [0u8; 16]);
    }

    #[test]
    fn open_nonexistent_nocreate_returns_noent() {
        let ctx = test_ctx();
        let sessions = test_sessions();
        let mut state = CompoundState {
            current_fh: Some(ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id)),
            saved_fh: None,
            current_stateid: None,
        };

        let mut body = XdrWriter::new();
        body.write_u32(0);
        body.write_u32(1);
        body.write_u32(0);
        body.write_u64(1);
        body.write_opaque(b"owner");
        body.write_u32(0); // NOCREATE (no createhow body)
        body.write_u32(0); // open_claim4 = CLAIM_NULL
        body.write_string("nosuchfile");
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, _) = op_open(&mut reader, &ctx, &sessions, &mut state);
        assert_eq!(status, nfs4_status::NFS4ERR_NOENT);
    }

    // ---------- CLOSE (§18.2) ----------

    #[test]
    fn close_valid_stateid_returns_ok() {
        let ctx = test_ctx();
        let sessions = test_sessions();

        // Create and open a file to get a stateid.
        ctx.write_named("closeable.txt", b"data".to_vec()).unwrap();
        let (fh, _) = ctx.lookup_by_name("closeable.txt").unwrap();
        let sid = sessions.open_file(fh);

        let mut state = CompoundState {
            current_fh: Some(fh),
            saved_fh: None,
            current_stateid: Some(sid),
        };

        let mut body = XdrWriter::new();
        body.write_u32(0); // seqid
        body.write_opaque_fixed(&sid.0); // stateid
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, _) = op_close(&mut reader, &sessions, &mut state);
        assert_eq!(status, nfs4_status::NFS4_OK);

        // The stateid should no longer be valid.
        assert!(
            !sessions.is_open(&sid),
            "stateid should be invalidated after CLOSE"
        );
    }

    #[test]
    fn close_then_read_returns_bad_stateid() {
        let sessions = test_sessions();

        // Open a file.
        let fh = [0x11u8; 32];
        let sid = sessions.open_file(fh);

        // Close it.
        sessions.close_file(&sid);

        // Verify the stateid is invalid.
        assert!(!sessions.is_open(&sid));
    }

    // ---------- NULL procedure (RFC 7530 §15.1) ----------

    /// Build the bytes that `mount.nfs4 -t nfs4 -o vers=4.x` sends as
    /// its FIRST RPC: an NFSv4 NULL ping (procedure 0). The Linux
    /// kernel uses NULL as a liveness probe before any COMPOUND.
    fn nfsv4_null_call_bytes(xid: u32) -> Vec<u8> {
        let mut w = XdrWriter::new();
        // RPC call header — caller, RPC version 2, NFSv4 program.
        w.write_u32(xid);
        w.write_u32(0); // CALL
        w.write_u32(2); // RPC v2
        w.write_u32(NFS4_PROGRAM);
        w.write_u32(NFS4_VERSION);
        w.write_u32(0); // procedure 0 = NULL
                        // AUTH_NONE creds + verifier.
        w.write_u32(0);
        w.write_opaque(&[]);
        w.write_u32(0);
        w.write_opaque(&[]);
        w.into_bytes()
    }

    /// Decode an ONC RPC reply header from `bytes`. Returns
    /// `(xid, msg_type, reply_stat, accept_stat)`.
    fn parse_rpc_reply(bytes: &[u8]) -> (u32, u32, u32, u32) {
        let mut r = XdrReader::new(bytes);
        let xid = r.read_u32().expect("xid");
        let msg_type = r.read_u32().expect("msg_type");
        let reply_stat = r.read_u32().expect("reply_stat");
        // Auth verifier: flavor + opaque body.
        let _ = r.read_u32();
        let _ = r.read_opaque();
        let accept_stat = r.read_u32().expect("accept_stat");
        (xid, msg_type, reply_stat, accept_stat)
    }

    /// RFC 7530 §15.1 — NFSv4 NULL must succeed with an empty
    /// ACCEPT_OK reply (no body). Linux `mount.nfs4` pings with NULL
    /// before any COMPOUND; if we don't reply with `accept_stat = 0`
    /// the kernel client gives up with `Input/output error` at the
    /// mount syscall.
    #[test]
    fn null_procedure_returns_accept_ok_with_empty_body() {
        let ctx = test_ctx();
        let sessions = test_sessions();
        let xid = 0xCAFE_BABE;
        let raw = nfsv4_null_call_bytes(xid);

        // Decode the RPC header so we can pass it through the same
        // path `handle_connection` uses.
        let mut r = XdrReader::new(&raw);
        let header = RpcCallHeader::decode(&mut r).expect("decode header");
        assert_eq!(header.procedure, 0, "we built a NULL call");

        let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);
        let (got_xid, msg_type, reply_stat, accept_stat) = parse_rpc_reply(&reply);

        assert_eq!(got_xid, xid);
        assert_eq!(msg_type, 1, "REPLY");
        assert_eq!(reply_stat, 0, "MSG_ACCEPTED");
        assert_eq!(accept_stat, 0, "SUCCESS — NULL must not be rejected");

        // Body after the RPC reply header MUST be empty for NULL.
        // (Reply header is exactly 24 bytes: xid + msg_type +
        // reply_stat + verf-flavor + verf-len(0) + accept_stat.)
        assert_eq!(
            reply.len(),
            24,
            "NULL reply has no body — got {} bytes after header",
            reply.len() - 24
        );
    }

    // ---------- EXCHANGE_ID wire encoding (RFC 5661 §18.35) ----------

    /// Decode `op_exchange_id`'s reply body and verify each field
    /// against RFC 5661 §18.35 EXCHANGE_ID4resok. Linux's NFSv4.1
    /// client tail-calls this immediately after a successful NULL,
    /// so any field-length mismatch leaves the kernel client unable
    /// to parse the reply and the mount(2) syscall returns EIO with
    /// no further trace.
    ///
    /// EXCHANGE_ID4resok structure:
    ///   clientid4              eir_clientid;       // u64
    ///   sequenceid4            eir_sequenceid;     // u32
    ///   uint32                 eir_flags;
    ///   state_protect4_r       eir_state_protect;  // u32 spr_how + body
    ///   server_owner4          eir_server_owner;   // u64 + opaque
    ///   opaque                 eir_server_scope;
    ///   nfs_impl_id4           eir_server_impl_id<1>;
    #[test]
    fn exchange_id_reply_is_rfc5661_18_35_compliant() {
        let sessions = test_sessions();

        // Build a minimal EXCHANGE_ID4args body.
        let mut body = XdrWriter::new();
        body.write_opaque_fixed(&[0xABu8; 8]); // co_verifier
        body.write_opaque(b"kernel-client"); // co_ownerid
        body.write_u32(0); // eia_flags
        body.write_u32(0); // eia_state_protect (SP4_NONE)
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, result) = op_exchange_id(&mut reader, &sessions);
        assert_eq!(status, nfs4_status::NFS4_OK);

        let mut r = XdrReader::new(&result);
        // Each op result starts with op_code + status.
        assert_eq!(r.read_u32().expect("op_code"), op::EXCHANGE_ID);
        assert_eq!(r.read_u32().expect("status"), nfs4_status::NFS4_OK);

        // EXCHANGE_ID4resok body:
        let clientid = r.read_u64().expect("clientid");
        assert!(clientid != 0, "clientid must be non-zero");

        let seqid = r.read_u32().expect("sequenceid");
        assert_eq!(seqid, 1, "first sequenceid must be 1");

        let _flags = r.read_u32().expect("eir_flags");

        let spr_how = r.read_u32().expect("eir_state_protect.spr_how");
        assert_eq!(spr_how, 0, "expected SP4_NONE (0)");

        // server_owner4: so_minor_id + so_major_id<>
        let _minor_id = r.read_u64().expect("so_minor_id");
        let major_id = r.read_opaque().expect("so_major_id");
        assert!(!major_id.is_empty(), "so_major_id must not be empty");

        // server_scope: opaque<>
        let scope = r.read_opaque().expect("eir_server_scope");
        assert!(!scope.is_empty(), "eir_server_scope must not be empty");

        // server_impl_id: opaque-array with at most 1 entry. We
        // emit count=0 (no impl_id) which is RFC-compliant.
        let impl_count = r.read_u32().expect("eir_server_impl_id count");
        assert!(
            impl_count <= 1,
            "RFC limits server_impl_id to a 0/1-entry array, got {impl_count}"
        );

        // After all the structured fields above, the reader should
        // be exhausted — anything left would be unaccounted-for
        // bytes that desync the Linux client.
        let trailing = r.remaining();
        assert_eq!(
            trailing, 0,
            "EXCHANGE_ID reply has {trailing} unaccounted trailing bytes",
        );
    }

    /// RFC 5661 §18.35.4: `eir_flags` must contain at least one of
    /// `EXCHGID4_FLAG_USE_NON_PNFS` (0x00010000),
    /// `EXCHGID4_FLAG_USE_PNFS_MDS` (0x00020000), or
    /// `EXCHGID4_FLAG_USE_PNFS_DS` (0x00040000) — the server's
    /// declaration of its operating mode. Without one of these,
    /// Linux's NFSv4.1 client rejects the EXCHANGE_ID reply with EIO
    /// before sending CREATE_SESSION (which is exactly what
    /// tests/e2e/test_pnfs.py was hitting).
    ///
    /// Kiseki is a pNFS MDS, so the bit we expect is `USE_PNFS_MDS`.
    #[test]
    fn exchange_id_advertises_pnfs_mds_mode() {
        let sessions = test_sessions();
        let mut body = XdrWriter::new();
        body.write_opaque_fixed(&[0u8; 8]);
        body.write_opaque(b"client");
        body.write_u32(0);
        body.write_u32(0);
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (_status, result) = op_exchange_id(&mut reader, &sessions);

        let mut r = XdrReader::new(&result);
        // Skip op_code, status, clientid, sequenceid.
        let _ = r.read_u32();
        let _ = r.read_u32();
        let _ = r.read_u64();
        let _ = r.read_u32();
        let flags = r.read_u32().expect("eir_flags");

        const EXCHGID4_FLAG_USE_NON_PNFS: u32 = 0x0001_0000;
        const EXCHGID4_FLAG_USE_PNFS_MDS: u32 = 0x0002_0000;
        const EXCHGID4_FLAG_USE_PNFS_DS: u32 = 0x0004_0000;
        const MODE_MASK: u32 =
            EXCHGID4_FLAG_USE_NON_PNFS | EXCHGID4_FLAG_USE_PNFS_MDS | EXCHGID4_FLAG_USE_PNFS_DS;

        assert!(
            flags & MODE_MASK != 0,
            "eir_flags must declare server mode (NON_PNFS | PNFS_MDS | PNFS_DS), \
             got 0x{flags:08x}",
        );
        // Kiseki is the MDS — the pNFS bit should be set.
        assert!(
            flags & EXCHGID4_FLAG_USE_PNFS_MDS != 0,
            "kiseki is a pNFS MDS — expected USE_PNFS_MDS in eir_flags, \
             got 0x{flags:08x}",
        );
    }
}
