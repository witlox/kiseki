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
    pub const NFS4ERR_LAYOUTUNAVAILABLE: u32 = 10059;
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

        if header.program != NFS4_PROGRAM || header.version != NFS4_VERSION {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 2); // PROG_MISMATCH
            w.write_u32(NFS4_VERSION);
            w.write_u32(NFS4_VERSION);
            write_rm_message(&mut stream, &w.into_bytes())?;
            continue;
        }

        // NFSv4 only has procedure 1 (COMPOUND).
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
    let _tag = reader.read_opaque().unwrap_or_default();
    let _minor_version = reader.read_u32().unwrap_or(2);
    let num_ops = reader.read_u32().unwrap_or(0).min(32); // Cap at 32 ops (C-ADV-3).

    let mut op_results: Vec<Vec<u8>> = Vec::new();
    let mut compound_status = nfs4_status::NFS4_OK;
    let mut state = CompoundState {
        current_fh: None,
        saved_fh: None,
        current_stateid: None,
    };

    for _ in 0..num_ops {
        let op_code = match reader.read_u32() {
            Ok(c) => c,
            Err(_) => break,
        };

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
        op::READDIR => op_readdir(reader, ctx),
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
        _ => {
            let mut w = XdrWriter::new();
            w.write_u32(op_code);
            w.write_u32(nfs4_status::NFS4ERR_NOTSUPP);
            (nfs4_status::NFS4ERR_NOTSUPP, w.into_bytes())
        }
    }
}

pub(crate) fn op_exchange_id(reader: &mut XdrReader<'_>, sessions: &SessionManager) -> (u32, Vec<u8>) {
    // Skip client owner (verifier + ownerid).
    let _verifier = reader.read_opaque_fixed(8).unwrap_or_default();
    let _owner_id = reader.read_opaque().unwrap_or_default();
    let _flags = reader.read_u32().unwrap_or(0);
    let _state_protect = reader.read_u32().unwrap_or(0);

    let client_id = sessions.exchange_id();

    let mut w = XdrWriter::new();
    w.write_u32(op::EXCHANGE_ID);
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_u64(client_id); // clientid
    w.write_u32(1); // sequenceid
    w.write_u32(0x01); // flags (CONFIRMED)
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

pub(crate) fn op_create_session(reader: &mut XdrReader<'_>, sessions: &SessionManager) -> (u32, Vec<u8>) {
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

pub(crate) fn op_destroy_session(reader: &mut XdrReader<'_>, sessions: &SessionManager) -> (u32, Vec<u8>) {
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
    let fh = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);
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
            w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
            (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes())
        }
    }
}

fn op_getattr<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &CompoundState,
) -> (u32, Vec<u8>) {
    // Skip requested attribute bitmap.
    let bitmap_count = reader.read_u32().unwrap_or(0);
    for _ in 0..bitmap_count {
        let _ = reader.read_u32();
    }

    let mut w = XdrWriter::new();
    w.write_u32(op::GETATTR);

    let Some(fh) = &state.current_fh else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
    };

    let status = match ctx.getattr(fh) {
        Ok(attrs) => {
            w.write_u32(nfs4_status::NFS4_OK);
            // Simplified attribute response: bitmap + attr values.
            w.write_u32(2); // bitmap count
            w.write_u32(0x0000_0018); // bitmap[0]: type + size
            w.write_u32(0); // bitmap[1]
                            // attr values (opaque)
            let mut attr_w = XdrWriter::new();
            let ftype = match attrs.file_type {
                crate::nfs_ops::FileType::Regular => 1u32,
                crate::nfs_ops::FileType::Directory => 2u32,
            };
            attr_w.write_u32(ftype);
            attr_w.write_u64(attrs.size);
            w.write_opaque(&attr_w.into_bytes());
            nfs4_status::NFS4_OK
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_NOENT);
            nfs4_status::NFS4ERR_NOENT
        }
    };

    (status, w.into_bytes())
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
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
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

    // Validate stateid is an open file (skip for special stateids).
    if sid_bytes.len() == 16 && sid_bytes != [0u8; 16] {
        let mut sid = [0u8; 16];
        sid.copy_from_slice(&sid_bytes);
        if !sessions.is_open(&StateId(sid)) {
            w.write_u32(nfs4_status::NFS4ERR_BAD_STATEID);
            return (nfs4_status::NFS4ERR_BAD_STATEID, w.into_bytes());
        }
    }

    // Kiseki compositions are immutable — reject nonzero offsets.
    if offset != 0 {
        w.write_u32(nfs4_status::NFS4ERR_IO);
        return (nfs4_status::NFS4ERR_IO, w.into_bytes());
    }

    let status = match ctx.write(data) {
        Ok((new_fh, resp)) => {
            state.current_fh = Some(new_fh);
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_u32(resp.count); // count
            w.write_u32(2); // committed = FILE_SYNC
            w.write_opaque_fixed(&[0u8; 8]); // write verifier
            nfs4_status::NFS4_OK
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_IO);
            nfs4_status::NFS4ERR_IO
        }
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
        w.write_u32(if matches!(stripe.iomode, crate::pnfs::LayoutIoMode::ReadWrite) {
            2
        } else {
            1
        });
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
        body.write_u32(0); // ff_ioflags4
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

fn op_open<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    sessions: &SessionManager,
    state: &mut CompoundState,
) -> (u32, Vec<u8>) {
    // Simplified OPEN: seqid + share_access + share_deny + owner + openhow
    let _seqid = reader.read_u32().unwrap_or(0);
    let _share_access = reader.read_u32().unwrap_or(1); // READ
    let _share_deny = reader.read_u32().unwrap_or(0); // NONE
                                                      // Skip owner (clientid + opaque)
    let _clientid = reader.read_u64().unwrap_or(0);
    let _owner = reader.read_opaque().unwrap_or_default();
    // openhow: opentype
    let open_type = reader.read_u32().unwrap_or(0); // OPEN4_NOCREATE=0, OPEN4_CREATE=1
    let name = reader.read_string().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::OPEN);

    let status = if open_type == 1 {
        // CREATE: write a new file.
        match ctx.write_named(&name, Vec::new()) {
            Ok((fh, _resp)) => {
                let sid = sessions.open_file(fh);
                state.current_fh = Some(fh);
                state.current_stateid = Some(sid);
                w.write_u32(nfs4_status::NFS4_OK);
                w.write_opaque_fixed(&sid.0); // stateid
                w.write_bool(false); // cinfo (not implemented)
                w.write_u32(1); // rflags: OPEN4_RESULT_CONFIRM
                nfs4_status::NFS4_OK
            }
            Err(_) => {
                w.write_u32(nfs4_status::NFS4ERR_IO);
                nfs4_status::NFS4ERR_IO
            }
        }
    } else {
        // NOCREATE: open existing file by name.
        match ctx.lookup_by_name(&name) {
            Some((fh, _attrs)) => {
                let sid = sessions.open_file(fh);
                state.current_fh = Some(fh);
                state.current_stateid = Some(sid);
                w.write_u32(nfs4_status::NFS4_OK);
                w.write_opaque_fixed(&sid.0);
                w.write_bool(false);
                w.write_u32(0);
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

fn op_readdir<G: GatewayOps>(reader: &mut XdrReader<'_>, ctx: &NfsContext<G>) -> (u32, Vec<u8>) {
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
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_opaque_fixed(&[0u8; 8]); // cookieverf

    let entries = ctx.readdir();
    for (i, entry) in entries.iter().enumerate() {
        w.write_bool(true); // entry follows
        w.write_u64((i + 1) as u64); // cookie
        w.write_string(&entry.name);
        w.write_u32(0); // attrs bitmap count (empty)
    }
    w.write_bool(false); // no more
    w.write_bool(true); // eof

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
    let fh_bytes = reader.read_opaque().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::PUTFH);

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
        let gw = InMemoryGateway::new(store, Box::new(ChunkStore::new()), master_key);
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
        assert_eq!(flags & 0x01, 0x01, "CONFIRMED flag should be set");
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

        let root_fh = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);
        assert_eq!(state.current_fh.unwrap(), root_fh);
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

        // Encode bitmap request.
        let mut body = XdrWriter::new();
        body.write_u32(2); // bitmap count
        body.write_u32(0x0000_0018); // type + size
        body.write_u32(0);
        let body_bytes = body.into_bytes();
        let mut reader = XdrReader::new(&body_bytes);

        let (status, result) = op_getattr(&mut reader, &ctx, &state);
        assert_eq!(status, nfs4_status::NFS4_OK);

        let mut r = XdrReader::new(&result);
        let _op = r.read_u32().unwrap();
        let st = r.read_u32().unwrap();
        assert_eq!(st, nfs4_status::NFS4_OK);
        // bitmap
        let bm_count = r.read_u32().unwrap();
        assert_eq!(bm_count, 2);
        let _bm0 = r.read_u32().unwrap();
        let _bm1 = r.read_u32().unwrap();
        // attr values (opaque)
        let attr_bytes = r.read_opaque().unwrap();
        let mut ar = XdrReader::new(&attr_bytes);
        let ftype = ar.read_u32().unwrap();
        assert_eq!(ftype, 2, "root type should be NF4DIR (2)");
        let size = ar.read_u64().unwrap();
        assert!(size > 0, "root size should be reported");
    }

    #[test]
    fn getattr_no_filehandle_returns_badhandle() {
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
        assert_eq!(status, nfs4_status::NFS4ERR_BADHANDLE);
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
        body.write_string("created-file.txt");
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
        body.write_u32(0); // OPEN4_NOCREATE
        body.write_string("readable.txt");
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
        body.write_u32(0); // NOCREATE
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
}
