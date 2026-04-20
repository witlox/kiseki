//! NFSv4.2 COMPOUND server (RFC 7862).
//!
//! Handles NFSv4.2 COMPOUND requests — each RPC contains a sequence
//! of operations processed in order. Session and lease management
//! for stateful file access.
//!
//! Program: 100003, Version: 4 (minor version 2).

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

/// NFSv4 operation codes (subset for MVP).
mod op {
    pub const GETATTR: u32 = 9;
    pub const GETFH: u32 = 10;
    pub const LOOKUP: u32 = 15;
    pub const PUTROOTFH: u32 = 24;
    pub const READ: u32 = 25;
    pub const WRITE: u32 = 38;
    pub const EXCHANGE_ID: u32 = 42;
    pub const CREATE_SESSION: u32 = 43;
    pub const DESTROY_SESSION: u32 = 44;
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
}

/// NFSv4 session state.
#[derive(Clone)]
struct Session {
    session_id: [u8; 16],
    client_id: u64,
    fore_channel_slots: u32,
    sequence_ids: Vec<u32>,
}

/// Per-connection NFSv4 COMPOUND state.
struct CompoundState {
    current_fh: Option<FileHandle>,
}

/// NFSv4 session manager — tracks active sessions and client IDs.
pub struct SessionManager {
    next_client_id: Mutex<u64>,
    sessions: Mutex<HashMap<[u8; 16], Session>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            next_client_id: Mutex::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn exchange_id(&self) -> u64 {
        let mut id = self.next_client_id.lock().unwrap();
        let client_id = *id;
        *id += 1;
        client_id
    }

    fn create_session(&self, client_id: u64, slots: u32) -> [u8; 16] {
        let mut session_id = [0u8; 16];
        session_id[..8].copy_from_slice(&client_id.to_be_bytes());
        session_id[8..16].copy_from_slice(&1u64.to_be_bytes());

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
    let num_ops = reader.read_u32().unwrap_or(0);

    let mut op_results: Vec<Vec<u8>> = Vec::new();
    let mut compound_status = nfs4_status::NFS4_OK;
    let mut state = CompoundState { current_fh: None };

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
        op::EXCHANGE_ID => op_exchange_id(reader, sessions),
        op::CREATE_SESSION => op_create_session(reader, sessions),
        op::DESTROY_SESSION => op_destroy_session(reader, sessions),
        op::SEQUENCE => op_sequence(reader, sessions),
        op::PUTROOTFH => op_putrootfh(ctx, state),
        op::GETFH => op_getfh(state),
        op::GETATTR => op_getattr(reader, ctx, state),
        op::READ => op_read(reader, ctx, state),
        op::WRITE => op_write(reader, ctx, state),
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
    } else {
        w.write_u32(nfs4_status::NFS4ERR_BADSESSION);
    }
    (nfs4_status::NFS4_OK, w.into_bytes())
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
        }
        None => {
            w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        }
    }
    (nfs4_status::NFS4_OK, w.into_bytes())
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

    match ctx.getattr(fh) {
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
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_NOENT);
        }
    }

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
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
    };

    match ctx.read(fh, offset, count) {
        Ok(resp) => {
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_bool(resp.eof);
            w.write_opaque(&resp.data);
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_IO);
        }
    }

    (nfs4_status::NFS4_OK, w.into_bytes())
}

fn op_write<G: GatewayOps>(
    reader: &mut XdrReader<'_>,
    ctx: &NfsContext<G>,
    state: &mut CompoundState,
) -> (u32, Vec<u8>) {
    // stateid + offset + stable + data
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let _offset = reader.read_u64().unwrap_or(0);
    let _stable = reader.read_u32().unwrap_or(2); // FILE_SYNC
    let data = reader.read_opaque().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::WRITE);

    match ctx.write(data) {
        Ok((new_fh, resp)) => {
            state.current_fh = Some(new_fh);
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_u32(resp.count); // count
            w.write_u32(2); // committed = FILE_SYNC
            w.write_opaque_fixed(&[0u8; 8]); // write verifier
        }
        Err(_) => {
            w.write_u32(nfs4_status::NFS4ERR_IO);
        }
    }

    (nfs4_status::NFS4_OK, w.into_bytes())
}

fn op_io_advise(reader: &mut XdrReader<'_>) -> (u32, Vec<u8>) {
    // IO_ADVISE: stateid + offset + count + hints bitmap
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let _offset = reader.read_u64().unwrap_or(0);
    let _count = reader.read_u64().unwrap_or(0);
    let _hints_count = reader.read_u32().unwrap_or(0);

    // TODO: forward hints to Advisory subsystem (ADR-020).
    // For now, accept and acknowledge.
    let mut w = XdrWriter::new();
    w.write_u32(op::IO_ADVISE);
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_u32(1); // hints bitmap count
    w.write_u32(0); // no hints applied

    (nfs4_status::NFS4_OK, w.into_bytes())
}
