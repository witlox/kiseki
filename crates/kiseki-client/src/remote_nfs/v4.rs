//! NFSv4.1/4.2 client (RFC 8881/7862) — session-based COMPOUND RPCs.
//!
//! Session lifecycle: `EXCHANGE_ID` → `CREATE_SESSION` → per-request
//! SEQUENCE + ops. Session established lazily on first use.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_gateway::error::GatewayError;
use kiseki_gateway::nfs4_server::op;
use kiseki_gateway::nfs_xdr::{XdrReader, XdrWriter};
use kiseki_gateway::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};

use super::transport::RpcTransport;

type MultipartBuffer = std::sync::Mutex<HashMap<String, Vec<(u32, Vec<u8>)>>>;

const NFS_PROGRAM: u32 = 100_003;
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
    /// Pool of independent NFSv4.1 sessions. Each slot holds an
    /// `AsyncMutex<Option<Nfs4Session>>`; the mutex serializes
    /// wire access to that session (NFSv4.1 SEQUENCE seqid must be
    /// monotonic per session). Calls pick a slot via the `next`
    /// round-robin counter, so N concurrent requests land on N
    /// different sessions — N × single-conn throughput.
    ///
    /// `tokio::sync::Mutex` (not `std::sync::Mutex`): `read`/`write`
    /// hold the slot across blocking sync TCP IO inside an
    /// `async fn`, and a std mutex would block tokio worker threads
    /// for every concurrent acquirer. Pre-fix (single-slot, std
    /// mutex): c=16 reads p99 = 30 s (= run deadline) due to runtime
    /// starvation. Post-fix with pool: throughput scales with
    /// `pool_size`.
    sessions: Vec<AsyncMutex<Option<Nfs4Session>>>,
    /// Round-robin slot selector. Wrapping add with `% sessions.len()`
    /// hands out connections fairly without locking the pool itself.
    next: AtomicUsize,
    /// Client-side multipart buffers. NFS has no native multipart concept,
    /// so we buffer parts locally and concatenate on complete.
    multipart_buffers: MultipartBuffer,
}

impl Nfs4Client {
    /// Create an NFSv4.1 client with a single session (= prior
    /// behavior). Use [`Self::v41_with_pool`] for concurrent workloads.
    #[must_use]
    pub fn v41(addr: SocketAddr) -> Self {
        Self::with_pool(addr, 1, 1)
    }

    /// Create an NFSv4.2 client with a single session.
    #[must_use]
    pub fn v42(addr: SocketAddr) -> Self {
        Self::with_pool(addr, 2, 1)
    }

    /// Create an NFSv4.1 client with `pool_size` independent sessions.
    /// Throughput on a single connection is wire-bounded; with N
    /// sessions, throughput scales by N (verified at N=4: ~38k op/s
    /// vs ~9k op/s for N=1, 64 KiB GETs).
    #[must_use]
    pub fn v41_with_pool(addr: SocketAddr, pool_size: usize) -> Self {
        Self::with_pool(addr, 1, pool_size)
    }

    /// Create an NFSv4.2 client with `pool_size` independent sessions.
    #[must_use]
    pub fn v42_with_pool(addr: SocketAddr, pool_size: usize) -> Self {
        Self::with_pool(addr, 2, pool_size)
    }

    fn with_pool(addr: SocketAddr, minor_version: u32, pool_size: usize) -> Self {
        let pool_size = pool_size.max(1);
        let mut sessions = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            sessions.push(AsyncMutex::new(None));
        }
        Self {
            addr,
            minor_version,
            sessions,
            next: AtomicUsize::new(0),
            multipart_buffers: Mutex::new(HashMap::new()),
        }
    }

    async fn ensure_session(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<Nfs4Session>>, GatewayError> {
        // Round-robin pick. `Relaxed` is fine: we only need fair
        // distribution, not ordering with other atomics.
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.sessions.len();
        let mut guard = self.sessions[idx].lock().await;
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

        let reply = transport.call(
            NFS_PROGRAM,
            NFS_VERSION,
            NFS_COMPOUND_PROC,
            &body.into_bytes(),
        )?;
        let (client_id, _) = parse_compound_single_op(&reply, op::EXCHANGE_ID, |r| {
            r.read_u64().map_err(|e| xdr_err(&e))
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

        let reply = transport.call(
            NFS_PROGRAM,
            NFS_VERSION,
            NFS_COMPOUND_PROC,
            &body.into_bytes(),
        )?;
        let (session_id, _) = parse_compound_single_op(&reply, op::CREATE_SESSION, |r| {
            let sid = r.read_opaque_fixed(16).map_err(|e| xdr_err(&e))?;
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
        let status = r.read_u32().map_err(|e| xdr_err(&e))?;
        if status != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!(
                "COMPOUND failed: {status}"
            )));
        }
        let _tag = r.read_opaque().map_err(|e| xdr_err(&e))?;
        let _num = r.read_u32().map_err(|e| xdr_err(&e))?;

        // Skip SEQUENCE result: op(4) + status(4) + session(16) + seqid(4) + slot(4) + highest(4) + flags(4)
        let _seq_op = r.read_u32().map_err(|e| xdr_err(&e))?;
        let seq_st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if seq_st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!(
                "SEQUENCE failed: {seq_st}"
            )));
        }
        let _ = r.read_opaque_fixed(16).map_err(|e| xdr_err(&e))?; // session_id echo
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // sequenceid
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // slotid
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // highest_slotid
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // target_highest_slotid
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // status_flags

        // Return remaining bytes (all subsequent op results)
        let pos = reply.len() - r.remaining();
        Ok(reply[pos..].to_vec())
    }
}

fn xdr_err(e: &std::io::Error) -> GatewayError {
    GatewayError::ProtocolError(format!("XDR: {e}"))
}

/// Parse a COMPOUND reply containing a single op result.
fn parse_compound_single_op<T>(
    reply: &[u8],
    expected_op: u32,
    parse_result: impl FnOnce(&mut XdrReader<'_>) -> Result<T, GatewayError>,
) -> Result<(T, Vec<u8>), GatewayError> {
    let mut r = XdrReader::new(reply);
    let status = r.read_u32().map_err(|e| xdr_err(&e))?;
    if status != NFS4_OK {
        return Err(GatewayError::ProtocolError(format!(
            "COMPOUND failed: {status}"
        )));
    }
    let _tag = r.read_opaque().map_err(|e| xdr_err(&e))?;
    let _num = r.read_u32().map_err(|e| xdr_err(&e))?;

    let actual_op = r.read_u32().map_err(|e| xdr_err(&e))?;
    if actual_op != expected_op {
        return Err(GatewayError::ProtocolError(format!(
            "expected op {expected_op}, got {actual_op}"
        )));
    }
    let op_status = r.read_u32().map_err(|e| xdr_err(&e))?;
    if op_status != NFS4_OK {
        return Err(GatewayError::ProtocolError(format!(
            "op {expected_op} failed: {op_status}"
        )));
    }
    let result = parse_result(&mut r)?;
    let remaining = reply[reply.len() - r.remaining()..].to_vec();
    Ok((result, remaining))
}

impl Nfs4Client {
    /// Drive a real NFSv4.1 OPEN(CREATE) + WRITE×N + COMMIT + GETFH
    /// COMPOUND against a single fresh file. Mirrors what a Linux
    /// `mount.nfs4` client does for a `dd if=/dev/urandom of=file
    /// bs=4K count=N`-style sequential write — every WRITE targets
    /// the same fh that OPEN created, the COMMIT flushes the
    /// per-fh buffer to one composition, and GETFH retrieves the
    /// resulting fh whose first 16 bytes carry the composition id.
    ///
    /// Parts are written in order; later parts at higher offsets
    /// overlay/extend earlier ones in the per-fh buffer (per
    /// `nfs_ops.rs::buffer_write`). Returns the composition id of
    /// the merged result.
    // Async even though no `.await` runs today — `RpcTransport` is
    // currently sync but is on the path to becoming async (Phase 16
    // fabric work uses `tokio::net::TcpStream` already). Keeping the
    // signature async means callers don't change when that lands.
    #[allow(clippy::unused_async)]
    pub async fn write_at_offsets(
        &self,
        parts: &[(u64, Vec<u8>)],
    ) -> Result<CompositionId, GatewayError> {
        let mut guard = self.ensure_session().await?;
        let sess = guard
            .as_mut()
            .expect("session not initialized — call connect_v41() first");

        let filename = uuid::Uuid::new_v4().to_string();
        let putrootfh = (op::PUTROOTFH, Vec::new());

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

        let mut compound: Vec<(u32, Vec<u8>)> = vec![putrootfh, open];
        for (offset, data) in parts {
            let mut w = XdrWriter::new();
            w.write_u32(0); // stateid seqid
            w.write_opaque_fixed(&[0u8; 12]); // anonymous stateid
            w.write_u64(*offset);
            w.write_u32(2); // FILE_SYNC
            w.write_opaque(data);
            compound.push((op::WRITE, w.into_bytes()));
        }
        let mut w = XdrWriter::new();
        w.write_u64(0); // commit offset
        w.write_u32(0); // commit count (0 = flush all)
        compound.push((op::COMMIT, w.into_bytes()));
        compound.push((op::GETFH, Vec::new()));

        let reply = sess.sequenced_compound(self.minor_version, &compound)?;
        let mut r = XdrReader::new(&reply);

        // PUTROOTFH
        r.read_u32().map_err(|e| xdr_err(&e))?;
        let st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("PUTROOTFH: {st}")));
        }
        // OPEN
        r.read_u32().map_err(|e| xdr_err(&e))?;
        let st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("OPEN: {st}")));
        }
        // stateid + change_info + rflags + attrset + delegation_type
        r.read_u32().map_err(|e| xdr_err(&e))?;
        r.read_opaque_fixed(12).map_err(|e| xdr_err(&e))?;
        r.read_u32().map_err(|e| xdr_err(&e))?;
        r.read_u64().map_err(|e| xdr_err(&e))?;
        r.read_u64().map_err(|e| xdr_err(&e))?;
        r.read_u32().map_err(|e| xdr_err(&e))?;
        let bm = r.read_u32().map_err(|e| xdr_err(&e))?;
        for _ in 0..bm {
            r.read_u32().map_err(|e| xdr_err(&e))?;
        }
        r.read_u32().map_err(|e| xdr_err(&e))?; // delegation_type=NONE
                                                // WRITEs
        for (i, _) in parts.iter().enumerate() {
            r.read_u32().map_err(|e| xdr_err(&e))?; // op
            let st = r.read_u32().map_err(|e| xdr_err(&e))?;
            if st != NFS4_OK {
                return Err(GatewayError::ProtocolError(format!("WRITE[{i}]: {st}")));
            }
            r.read_u32().map_err(|e| xdr_err(&e))?; // count
            r.read_u32().map_err(|e| xdr_err(&e))?; // committed
            r.read_opaque_fixed(8).map_err(|e| xdr_err(&e))?; // verifier
        }
        // COMMIT
        r.read_u32().map_err(|e| xdr_err(&e))?;
        let st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("COMMIT: {st}")));
        }
        r.read_opaque_fixed(8).map_err(|e| xdr_err(&e))?;
        // GETFH
        r.read_u32().map_err(|e| xdr_err(&e))?;
        let st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("GETFH: {st}")));
        }
        let fh = r.read_opaque().map_err(|e| xdr_err(&e))?;
        let composition_id = if fh.len() >= 16 {
            CompositionId(
                uuid::Uuid::from_slice(&fh[..16]).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            )
        } else {
            CompositionId(uuid::Uuid::new_v4())
        };
        Ok(composition_id)
    }
}

#[async_trait::async_trait]
impl GatewayOps for Nfs4Client {
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        let mut guard = self.ensure_session().await?;
        let sess = guard
            .as_mut()
            .expect("session not initialized — call connect_v41() first");

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

        // COMMIT — flushes buffered writes to a composition.
        // No arguments needed (RFC 8881 §18.3: offset=0, count=0 = flush all).
        let mut w = XdrWriter::new();
        w.write_u64(0); // offset
        w.write_u32(0); // count
        let commit = (op::COMMIT, w.into_bytes());

        let reply =
            sess.sequenced_compound(self.minor_version, &[putrootfh, open, write, commit, getfh])?;

        // Walk the op results sequentially using XdrReader.
        let mut r = XdrReader::new(&reply);

        // PUTROOTFH result: op(4) + status(4)
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // op
        let st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("PUTROOTFH: {st}")));
        }

        // OPEN result: op(4) + status(4) + stateid(16) + cinfo(1+8+8=17) +
        //   rflags(4) + attrset_count(4) + delegation_type(4)
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // op
        let open_st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if open_st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("OPEN: {open_st}")));
        }
        // stateid4: seqid(4) + other(12)
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?;
        let _ = r.read_opaque_fixed(12).map_err(|e| xdr_err(&e))?;
        // change_info4: atomic(4) + before(8) + after(8)
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?;
        let _ = r.read_u64().map_err(|e| xdr_err(&e))?;
        let _ = r.read_u64().map_err(|e| xdr_err(&e))?;
        // rflags
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?;
        // attrset bitmap4: count + words
        let bm_count = r.read_u32().map_err(|e| xdr_err(&e))?;
        for _ in 0..bm_count {
            let _ = r.read_u32().map_err(|e| xdr_err(&e))?;
        }
        // open_delegation4: type (0=NONE, no body)
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?;

        // WRITE result: op(4) + status(4) + count(4) + committed(4) + verifier(8)
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // op
        let write_st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if write_st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("WRITE: {write_st}")));
        }
        let count = r.read_u32().map_err(|e| xdr_err(&e))?;
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // committed
        let _ = r.read_opaque_fixed(8).map_err(|e| xdr_err(&e))?; // verifier

        // COMMIT result: op(4) + status(4) + verifier(8)
        // COMMIT flushes the write buffer to a composition.
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // op
        let commit_st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if commit_st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("COMMIT: {commit_st}")));
        }
        let _ = r.read_opaque_fixed(8).map_err(|e| xdr_err(&e))?; // verifier

        // GETFH result: op(4) + status(4) + fh4(opaque)
        // GETFH after COMMIT picks up the file handle that flush_writes
        // set (the composition with the actual data).
        let _ = r.read_u32().map_err(|e| xdr_err(&e))?; // op
        let getfh_st = r.read_u32().map_err(|e| xdr_err(&e))?;
        if getfh_st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("GETFH: {getfh_st}")));
        }
        let fh = r.read_opaque().map_err(|e| xdr_err(&e))?;

        // Extract composition UUID from file handle (first 16 bytes).
        let composition_id = if fh.len() >= 16 {
            CompositionId(
                uuid::Uuid::from_slice(&fh[..16]).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            )
        } else {
            CompositionId(uuid::Uuid::new_v4())
        };

        Ok(WriteResponse {
            composition_id,
            bytes_written: u64::from(count),
        })
    }

    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        let mut guard = self.ensure_session().await?;
        let sess = guard
            .as_mut()
            .expect("session not initialized — call connect_v41() first");

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
        w.write_u32(u32::try_from(req.length).unwrap_or(u32::MAX));
        let read = (op::READ, w.into_bytes());

        let reply = sess.sequenced_compound(self.minor_version, &[putrootfh, open, read])?;

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
        let st = u32::from_be_bytes(
            r[4..8]
                .try_into()
                .expect("byte slice has the exact fixed length required"),
        );
        if st != NFS4_OK {
            return Err(GatewayError::ProtocolError(format!("READ: {st}")));
        }
        let eof = u32::from_be_bytes(
            r[8..12]
                .try_into()
                .expect("byte slice has the exact fixed length required"),
        ) != 0;
        let data_len = u32::from_be_bytes(
            r[12..16]
                .try_into()
                .expect("byte slice has the exact fixed length required"),
        ) as usize;
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
        let mut guard = self.ensure_session().await?;
        let sess = guard
            .as_mut()
            .expect("session not initialized — call connect_v41() first");

        let putrootfh = (op::PUTROOTFH, Vec::new());

        let mut w = XdrWriter::new();
        w.write_string(&composition_id.0.to_string());
        let remove = (op::REMOVE, w.into_bytes());

        let _ = sess.sequenced_compound(self.minor_version, &[putrootfh, remove])?;
        Ok(())
    }

    /// Start a multipart upload. NFS has no native multipart concept, so
    /// we return a client-side UUID and buffer parts locally until
    /// `complete_multipart` concatenates and writes them in one OPEN+WRITE.
    async fn start_multipart(&self, _namespace_id: NamespaceId) -> Result<String, GatewayError> {
        let upload_id = uuid::Uuid::new_v4().to_string();
        let mut buffers = self
            .multipart_buffers
            .lock()
            .map_err(|e| GatewayError::ProtocolError(format!("lock: {e}")))?;
        buffers.insert(upload_id.clone(), Vec::new());
        Ok(upload_id)
    }

    /// Buffer a part client-side. Returns the part number as the `ETag`
    /// (no server-side tracking for NFS).
    async fn upload_part(
        &self,
        upload_id: &str,
        part_number: u32,
        data: &[u8],
    ) -> Result<String, GatewayError> {
        let mut buffers = self
            .multipart_buffers
            .lock()
            .map_err(|e| GatewayError::ProtocolError(format!("lock: {e}")))?;
        let parts = buffers.get_mut(upload_id).ok_or_else(|| {
            GatewayError::ProtocolError(format!("unknown upload_id: {upload_id}"))
        })?;
        parts.push((part_number, data.to_vec()));
        Ok(part_number.to_string())
    }

    /// Concatenate all buffered parts (sorted by part number) and write
    /// them as a single NFS OPEN+WRITE.
    async fn complete_multipart(
        &self,
        upload_id: &str,
        _name: Option<&str>,
    ) -> Result<CompositionId, GatewayError> {
        let mut parts = {
            let mut buffers = self
                .multipart_buffers
                .lock()
                .map_err(|e| GatewayError::ProtocolError(format!("lock: {e}")))?;
            buffers.remove(upload_id).ok_or_else(|| {
                GatewayError::ProtocolError(format!("unknown upload_id: {upload_id}"))
            })?
        };
        parts.sort_by_key(|(n, _)| *n);
        let data: Vec<u8> = parts.into_iter().flat_map(|(_, d)| d).collect();

        let resp = self
            .write(WriteRequest {
                tenant_id: OrgId(uuid::Uuid::nil()),
                namespace_id: NamespaceId(uuid::Uuid::nil()),
                data,
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await?;
        Ok(resp.composition_id)
    }

    /// Drop the buffered parts for a multipart upload.
    async fn abort_multipart(&self, upload_id: &str) -> Result<(), GatewayError> {
        let mut buffers = self
            .multipart_buffers
            .lock()
            .map_err(|e| GatewayError::ProtocolError(format!("lock: {e}")))?;
        buffers.remove(upload_id);
        Ok(())
    }

    /// No-op: NFS has no content-type concept.
    async fn set_object_content_type(
        &self,
        _composition_id: CompositionId,
        _content_type: Option<String>,
    ) -> Result<(), GatewayError> {
        Ok(())
    }

    /// No-op: NFS namespaces are server-managed.
    async fn ensure_namespace(
        &self,
        _tenant_id: OrgId,
        _namespace_id: NamespaceId,
    ) -> Result<(), GatewayError> {
        Ok(())
    }
}
