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
use std::net::TcpStream;
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
mod op {
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
    pub const SEQUENCE: u32 = 53;
    pub const IO_ADVISE: u32 = 63;
}

/// NFSv4 status codes.
mod nfs4_status {
    pub const NFS4_OK: u32 = 0;
    pub const NFS4ERR_NOENT: u32 = 2;
    pub const NFS4ERR_IO: u32 = 5;
    pub const NFS4ERR_NOTSUPP: u32 = 10004;
    pub const NFS4ERR_BADHANDLE: u32 = 10001;
    pub const NFS4ERR_STALE_CLIENTID: u32 = 10012;
    pub const NFS4ERR_BADSESSION: u32 = 10052;
    pub const NFS4ERR_BAD_STATEID: u32 = 10025;
    pub const NFS4ERR_DENIED: u32 = 10010;
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
struct StateId([u8; 16]);

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

    fn open_file(&self, fh: FileHandle) -> StateId {
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

/// Handle one NFSv4 TCP connection (after the first message).
pub fn handle_nfs4_connection<G: GatewayOps>(
    mut stream: TcpStream,
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
        _ => {
            let mut w = XdrWriter::new();
            w.write_u32(op_code);
            w.write_u32(nfs4_status::NFS4ERR_NOTSUPP);
            (nfs4_status::NFS4ERR_NOTSUPP, w.into_bytes())
        }
    }
}

fn op_exchange_id(reader: &mut XdrReader<'_>, sessions: &SessionManager) -> (u32, Vec<u8>) {
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

fn op_create_session(reader: &mut XdrReader<'_>, sessions: &SessionManager) -> (u32, Vec<u8>) {
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

fn op_destroy_session(reader: &mut XdrReader<'_>, sessions: &SessionManager) -> (u32, Vec<u8>) {
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

fn op_sequence(reader: &mut XdrReader<'_>, sessions: &SessionManager) -> (u32, Vec<u8>) {
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
