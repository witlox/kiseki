//! NFSv4.1/4.2 client (RFC 8881/7862) — session-based COMPOUND RPCs.
//!
//! Session lifecycle: EXCHANGE_ID → CREATE_SESSION → per-request
//! SEQUENCE + ops. Session established lazily on first use.

use std::net::SocketAddr;
use std::sync::Mutex;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_gateway::error::GatewayError;
use kiseki_gateway::nfs4_server::op;
use kiseki_gateway::nfs_xdr::{XdrReader, XdrWriter};
use kiseki_gateway::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};

use super::transport::RpcTransport;

const NFS_PROGRAM: u32 = 100003;
const NFS_VERSION: u32 = 4;
const NFS_COMPOUND_PROC: u32 = 1;
const NFS4_OK: u32 = 0;

struct Nfs4Session {
    transport: RpcTransport,
    client_id: u64,
    session_id: [u8; 16],
    sequence: u32,
}

/// NFSv4.1 client with session management.
pub struct Nfs4Client {
    addr: SocketAddr,
    minor_version: u32, // 1 for NFSv4.1, 2 for NFSv4.2
    session: Mutex<Option<Nfs4Session>>,
}

impl Nfs4Client {
    /// Create an NFSv4.1 client.
    pub fn v41(addr: SocketAddr) -> Self {
        Self {
            addr,
            minor_version: 1,
            session: Mutex::new(None),
        }
    }

    /// Create an NFSv4.2 client.
    pub fn v42(addr: SocketAddr) -> Self {
        Self {
            addr,
            minor_version: 2,
            session: Mutex::new(None),
        }
    }

    fn ensure_session(&self) -> Result<std::sync::MutexGuard<'_, Option<Nfs4Session>>, GatewayError> {
        let mut guard = self.session.lock().map_err(|e| {
            GatewayError::ProtocolError(format!("lock: {e}"))
        })?;
        if guard.is_none() {
            *guard = Some(self.establish_session()?);
        }
        Ok(guard)
    }

    fn establish_session(&self) -> Result<Nfs4Session, GatewayError> {
        let mut transport = RpcTransport::connect(self.addr)?;

        // EXCHANGE_ID
        let mut body = XdrWriter::new();
        body.write_u32(0); // tag len
        body.write_u32(1); // minor_version = 1 (session ops)
        body.write_u32(1); // 1 op
        body.write_u32(op::EXCHANGE_ID);
        body.write_opaque_fixed(&[0u8; 8]); // verifier
        body.write_opaque(b"kiseki-client"); // owner_id
        body.write_u32(0); // flags
        body.write_u32(0); // SP4_NONE
        body.write_u32(0); // impl_id count

        let reply = transport.call(NFS_PROGRAM, NFS_VERSION, NFS_COMPOUND_PROC, &body.into_bytes())?;
        let (client_id, _) = parse_compound_single_op(&reply, op::EXCHANGE_ID, |r| {
            r.read_u64().map_err(xdr_err)
        })?;

        // CREATE_SESSION
        let mut body = XdrWriter::new();
        body.write_u32(0); // tag
        body.write_u32(1); // minor_version
        body.write_u32(1); // 1 op
        body.write_u32(op::CREATE_SESSION);
        body.write_u64(client_id);
        body.write_u32(1); // sequence
        body.write_u32(0); // flags

        let reply = transport.call(NFS_PROGRAM, NFS_VERSION, NFS_COMPOUND_PROC, &body.into_bytes())?;
        let (session_id, _) = parse_compound_single_op(&reply, op::CREATE_SESSION, |r| {
            let sid = r.read_opaque_fixed(16).map_err(xdr_err)?;
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&sid);
            Ok(arr)
        })?;

        Ok(Nfs4Session {
            transport,
            client_id,
            session_id,
            sequence: 1,
        })
    }
}

impl Nfs4Session {
    /// Send COMPOUND with SEQUENCE prepended. Returns op results
    /// after the SEQUENCE result.
    fn sequenced_compound(
        &mut self,
        minor_version: u32,
        ops: &[(u32, Vec<u8>)],
    ) -> Result<Vec<u8>, GatewayError> {
        let mut body = XdrWriter::new();
        body.write_u32(0); // tag
        body.write_u32(minor_version);
        body.write_u32((1 + ops.len()) as u32); // SEQUENCE + ops

        // SEQUENCE
        body.write_u32(op::SEQUENCE);
        body.write_opaque_fixed(&self.session_id);
        body.write_u32(self.sequence);
        body.write_u32(0); // slot_id
        body.write_u32(0); // highest_slot_id
        body.write_u32(0); // sa_cachethis

        // Remaining ops
        for (op_code, args) in ops {
            body.write_u32(*op_code);
            body.write_opaque_fixed(args);
        }

        self.sequence += 1;

        let reply = self.transport.call(
            NFS_PROGRAM,
            NFS_VERSION,
            NFS_COMPOUND_PROC,
            &body.into_bytes(),
        )?;

        // Parse COMPOUND header
        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(xdr_err)?;
        if status != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!(
                "COMPOUND failed: {status}"
            )));
        }
        let _tag = r.read_opaque().map_err(xdr_err)?;
        let _num = r.read_u32().map_err(xdr_err)?;

        // Skip SEQUENCE result: op(4) + status(4) + session(16) + seqid(4) + slot(4) + highest(4) + flags(4)
        let seq_op = r.read_u32().map_err(xdr_err)?;
        let seq_st = r.read_u32().map_err(xdr_err)?;
        if seq_st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!(
                "SEQUENCE failed: {seq_st}"
            )));
        }
        let _ = r.read_opaque_fixed(16).map_err(xdr_err)?; // session_id echo
        let _ = r.read_u32().map_err(xdr_err)?; // sequenceid
        let _ = r.read_u32().map_err(xdr_err)?; // slotid
        let _ = r.read_u32().map_err(xdr_err)?; // highest_slotid
        let _ = r.read_u32().map_err(xdr_err)?; // target_highest_slotid
        let _ = r.read_u32().map_err(xdr_err)?; // status_flags

        // Return remaining bytes (all subsequent op results)
        let pos = reply.len() - r.remaining();
        Ok(reply[pos..].to_vec())
    }
}

fn xdr_err(e: std::io::Error) -> GatewayError {
    GatewayError::ProtocolError(format!("XDR: {e}"))
}

/// Parse a COMPOUND reply containing a single op result.
fn parse_compound_single_op<T>(
    reply: &[u8],
    expected_op: u32,
    parse_result: impl FnOnce(&mut XdrReader<'_>) -> Result<T, GatewayError>,
) -> Result<(T, Vec<u8>), GatewayError> {
    let mut r = XdrReader::new(reply);
    let status = r.read_u32().map_err(xdr_err)?;
    if status != NFS4_OK {
        return Err(GatewayError::ProtocolError(format!(
            "COMPOUND failed: {status}"
        )));
    }
    let _tag = r.read_opaque().map_err(xdr_err)?;
    let _num = r.read_u32().map_err(xdr_err)?;

    let actual_op = r.read_u32().map_err(xdr_err)?;
    if actual_op != expected_op {
        return Err(GatewayError::ProtocolError(format!(
            "expected op {expected_op}, got {actual_op}"
        )));
    }
    let op_status = r.read_u32().map_err(xdr_err)?;
    if op_status != NFS4_OK {
        return Err(GatewayError::ProtocolError(format!(
            "op {expected_op} failed: {op_status}"
        )));
    }
    let result = parse_result(&mut r)?;
    let remaining = reply[reply.len() - r.remaining()..].to_vec();
    Ok((result, remaining))
}

#[async_trait::async_trait]
impl GatewayOps for Nfs4Client {
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        let mut guard = self.ensure_session()?;
        let sess = guard.as_mut().unwrap();

        let filename = uuid::Uuid::new_v4().to_string();

        // PUTROOTFH (no args)
        let putrootfh = (op::PUTROOTFH, Vec::new());

        // OPEN (CREATE)
        let mut w = XdrWriter::new();
        w.write_u32(0); // seqid
        w.write_u32(2); // share_access = WRITE
        w.write_u32(0); // share_deny
        w.write_u64(sess.client_id);
        w.write_opaque(b"kiseki-client");
        w.write_u32(1); // OPEN4_CREATE
        w.write_u32(0); // UNCHECKED4
        w.write_u32(0); // fattr4 bitmap count = 0
        w.write_opaque(&[]); // fattr4 vals
        w.write_u32(0); // CLAIM_NULL
        w.write_string(&filename);
        let open = (op::OPEN, w.into_bytes());

        // GETFH — retrieves the file handle after OPEN sets current_fh.
        // The handle's first 16 bytes are the composition UUID.
        let getfh = (op::GETFH, Vec::new());

        // WRITE
        let mut w = XdrWriter::new();
        w.write_u32(0); // stateid seqid
        w.write_opaque_fixed(&[0u8; 12]); // stateid other (anonymous)
        w.write_u64(0); // offset
        w.write_u32(2); // FILE_SYNC
        w.write_opaque(&req.data);
        let write = (op::WRITE, w.into_bytes());

        let reply = sess.sequenced_compound(
            self.minor_version,
            &[putrootfh, open, write, getfh],
        )?;

        // Walk the op results sequentially using XdrReader.
        let mut r = XdrReader::new(&reply);

        // PUTROOTFH result: op(4) + status(4)
        let _ = r.read_u32().map_err(xdr_err)?; // op
        let st = r.read_u32().map_err(xdr_err)?;
        if st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("PUTROOTFH: {st}")));
        }

        // OPEN result: op(4) + status(4) + stateid(16) + cinfo(1+8+8=17) +
        //   rflags(4) + attrset_count(4) + delegation_type(4)
        let _ = r.read_u32().map_err(xdr_err)?; // op
        let open_st = r.read_u32().map_err(xdr_err)?;
        if open_st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("OPEN: {open_st}")));
        }
        // stateid4: seqid(4) + other(12)
        let _ = r.read_u32().map_err(xdr_err)?;
        let _ = r.read_opaque_fixed(12).map_err(xdr_err)?;
        // change_info4: atomic(4) + before(8) + after(8)
        let _ = r.read_u32().map_err(xdr_err)?;
        let _ = r.read_u64().map_err(xdr_err)?;
        let _ = r.read_u64().map_err(xdr_err)?;
        // rflags
        let _ = r.read_u32().map_err(xdr_err)?;
        // attrset bitmap4: count + words
        let bm_count = r.read_u32().map_err(xdr_err)?;
        for _ in 0..bm_count {
            let _ = r.read_u32().map_err(xdr_err)?;
        }
        // open_delegation4: type (0=NONE, no body)
        let _ = r.read_u32().map_err(xdr_err)?;

        // WRITE result: op(4) + status(4) + count(4) + committed(4) + verifier(8)
        let _ = r.read_u32().map_err(xdr_err)?; // op
        let write_st = r.read_u32().map_err(xdr_err)?;
        if write_st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("WRITE: {write_st}")));
        }
        let count = r.read_u32().map_err(xdr_err)?;
        let _ = r.read_u32().map_err(xdr_err)?; // committed
        let _ = r.read_opaque_fixed(8).map_err(xdr_err)?; // verifier

        // GETFH result: op(4) + status(4) + fh4(opaque)
        // GETFH after WRITE picks up the file handle that WRITE set
        // (which contains the composition UUID for the written data).
        let _ = r.read_u32().map_err(xdr_err)?; // op
        let getfh_st = r.read_u32().map_err(xdr_err)?;
        if getfh_st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("GETFH: {getfh_st}")));
        }
        let fh = r.read_opaque().map_err(xdr_err)?;

        // Extract composition UUID from file handle (first 16 bytes).
        let composition_id = if fh.len() >= 16 {
            CompositionId(uuid::Uuid::from_slice(&fh[..16]).unwrap_or_else(|_| uuid::Uuid::new_v4()))
        } else {
            CompositionId(uuid::Uuid::new_v4())
        };

        Ok(WriteResponse {
            composition_id,
            bytes_written: count as u64,
        })
    }

    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        let mut guard = self.ensure_session()?;
        let sess = guard.as_mut().unwrap();

        let filename = req.composition_id.0.to_string();

        let putrootfh = (op::PUTROOTFH, Vec::new());

        // OPEN (read existing)
        let mut w = XdrWriter::new();
        w.write_u32(0);
        w.write_u32(1); // READ
        w.write_u32(0);
        w.write_u64(sess.client_id);
        w.write_opaque(b"kiseki-client");
        w.write_u32(0); // OPEN4_NOCREATE
        w.write_u32(0); // CLAIM_NULL
        w.write_string(&filename);
        let open = (op::OPEN, w.into_bytes());

        // READ
        let mut w = XdrWriter::new();
        w.write_u32(0);
        w.write_opaque_fixed(&[0u8; 12]);
        w.write_u64(req.offset);
        w.write_u32(req.length.min(u32::MAX as u64) as u32);
        let read = (op::READ, w.into_bytes());

        let reply = sess.sequenced_compound(
            self.minor_version,
            &[putrootfh, open, read],
        )?;

        // Find READ result — scan for the op code
        let read_bytes = op::READ.to_be_bytes();
        let pos = reply
            .windows(4)
            .rposition(|w| w == read_bytes)
            .ok_or_else(|| GatewayError::ProtocolError("READ op not in reply".into()))?;

        let r = &reply[pos..];
        if r.len() < 16 {
            return Err(GatewayError::ProtocolError("READ reply short".into()));
        }
        let st = u32::from_be_bytes(r[4..8].try_into().unwrap());
        if st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("READ: {st}")));
        }
        let eof = u32::from_be_bytes(r[8..12].try_into().unwrap()) != 0;
        let data_len = u32::from_be_bytes(r[12..16].try_into().unwrap()) as usize;
        let data = r[16..16 + data_len].to_vec();

        Ok(ReadResponse {
            data,
            eof,
            content_type: None,
        })
    }

    async fn list(
        &self,
        _tenant_id: OrgId,
        _namespace_id: NamespaceId,
    ) -> Result<Vec<(CompositionId, u64)>, GatewayError> {
        Ok(Vec::new())
    }

    async fn delete(
        &self,
        _tenant_id: OrgId,
        _namespace_id: NamespaceId,
        composition_id: CompositionId,
    ) -> Result<(), GatewayError> {
        let mut guard = self.ensure_session()?;
        let sess = guard.as_mut().unwrap();

        let putrootfh = (op::PUTROOTFH, Vec::new());

        let mut w = XdrWriter::new();
        w.write_string(&composition_id.0.to_string());
        let remove = (op::REMOVE, w.into_bytes());

        let _ = sess.sequenced_compound(self.minor_version, &[putrootfh, remove])?;
        Ok(())
    }
}
