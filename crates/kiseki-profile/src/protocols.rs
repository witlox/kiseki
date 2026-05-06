//! Per-protocol workload drivers. Every driver exposes the same
//! `put` / `get` shape so the worker loop in `main` is protocol-
//! agnostic; the actual wire path is whatever the underlying client
//! does (HTTP, `NFSv3` RPCs, `NFSv4` COMPOUNDs, pNFS LAYOUTGET → DS,
//! FUSE → `GatewayOps` → S3).

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use kiseki_client::remote_http::RemoteHttpGateway;
use kiseki_client::remote_nfs::transport::RpcTransport;
use kiseki_client::remote_nfs::v3::Nfs3Client;
use kiseki_client::remote_nfs::v4::Nfs4Client;
use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_gateway::nfs4_server::op;
use kiseki_gateway::nfs_xdr::{XdrReader, XdrWriter};
use kiseki_gateway::ops::{GatewayOps, ReadRequest, WriteRequest};

use crate::harness::ProfileServer;
use crate::Protocol;

/// Opaque per-driver handle for a previously-PUT object. Most drivers
/// just stash the `composition_id`; FUSE additionally tracks its own
/// inode → name mapping but the workload loop treats `Key` as opaque.
#[derive(Clone, Debug)]
pub struct Key {
    pub composition_id: CompositionId,
    pub name: Option<String>,
}

#[async_trait]
pub trait Driver: Send + Sync {
    async fn put(&self, payload: &[u8]) -> Result<Key, String>;
    async fn get(&self, key: &Key) -> Result<usize, String>;
}

pub async fn build(
    protocol: Protocol,
    server: Option<&ProfileServer>,
    pool_size: usize,
) -> Result<Arc<dyn Driver>, String> {
    match protocol {
        Protocol::InProcess => {
            // No server needed.
            Ok(Arc::new(InProcessDriver::new()))
        }
        Protocol::InProcessPersistent => {
            // No server needed; mirrors `kiseki-server`'s persistent
            // wiring inside this process.
            Ok(Arc::new(InProcessPersistentDriver::new().await?))
        }
        Protocol::S3 => {
            let s = server.ok_or("S3 driver requires --server-bin (none was passed)")?;
            Ok(Arc::new(S3Driver::new(&s.s3_base)))
        }
        Protocol::Nfs3 => {
            let s = server.ok_or("Nfs3 driver requires --server-bin")?;
            Ok(Arc::new(Nfs3Driver::new(s.nfs_addr, pool_size)))
        }
        Protocol::Nfs4 => {
            let s = server.ok_or("Nfs4 driver requires --server-bin")?;
            Ok(Arc::new(Nfs4Driver::new(s.nfs_addr, pool_size)))
        }
        Protocol::Pnfs => {
            let s = server.ok_or("Pnfs driver requires --server-bin")?;
            Ok(Arc::new(PnfsDriver::new(s.nfs_addr, pool_size)))
        }
        Protocol::Fuse => {
            let s = server.ok_or("Fuse driver requires --server-bin")?;
            Ok(Arc::new(FuseDriver::new(&s.s3_base)))
        }
        Protocol::Native => {
            let s = server.ok_or("Native driver requires --server-bin")?;
            let addr = format!("127.0.0.1:{}", s.ports.grpc_data);
            Ok(Arc::new(NativeDriver::new(&addr).await?))
        }
    }
}

// ---------------------------------------------------------------------------
// S3
// ---------------------------------------------------------------------------

struct S3Driver {
    inner: RemoteHttpGateway,
    namespace_id: NamespaceId,
    tenant_id: OrgId,
}

impl S3Driver {
    fn new(s3_base: &str) -> Self {
        Self {
            inner: RemoteHttpGateway::new(s3_base),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default")),
        }
    }
}

#[async_trait]
impl Driver for S3Driver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let resp = self
            .inner
            .write(WriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data: payload.to_vec(),
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await
            .map_err(|e| format!("s3 put: {e}"))?;
        Ok(Key {
            composition_id: resp.composition_id,
            name: None,
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        let resp = self
            .inner
            .read(ReadRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                composition_id: key.composition_id,
                offset: 0,
                length: u64::MAX,
            })
            .await
            .map_err(|e| format!("s3 get: {e}"))?;
        Ok(resp.data.len())
    }
}

// ---------------------------------------------------------------------------
// NFSv3
// ---------------------------------------------------------------------------

struct Nfs3Driver {
    inner: Arc<Nfs3Client>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
}

impl Nfs3Driver {
    fn new(nfs_addr: SocketAddr, pool_size: usize) -> Self {
        Self {
            inner: Arc::new(Nfs3Client::with_pool(nfs_addr, pool_size)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default")),
        }
    }
}

#[async_trait]
impl Driver for Nfs3Driver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let resp = self
            .inner
            .write(WriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data: payload.to_vec(),
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await
            .map_err(|e| format!("nfs3 put: {e}"))?;
        Ok(Key {
            composition_id: resp.composition_id,
            name: None,
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        let resp = self
            .inner
            .read(ReadRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                composition_id: key.composition_id,
                offset: 0,
                length: u64::MAX,
            })
            .await
            .map_err(|e| format!("nfs3 get: {e}"))?;
        Ok(resp.data.len())
    }
}

// ---------------------------------------------------------------------------
// NFSv4.1
// ---------------------------------------------------------------------------

struct Nfs4Driver {
    inner: Arc<Nfs4Client>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
}

impl Nfs4Driver {
    fn new(nfs_addr: SocketAddr, pool_size: usize) -> Self {
        Self {
            inner: Arc::new(Nfs4Client::v41_with_pool(nfs_addr, pool_size)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default")),
        }
    }
}

#[async_trait]
impl Driver for Nfs4Driver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let resp = self
            .inner
            .write(WriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data: payload.to_vec(),
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await
            .map_err(|e| format!("nfs4 put: {e}"))?;
        Ok(Key {
            composition_id: resp.composition_id,
            name: None,
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        let resp = self
            .inner
            .read(ReadRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                composition_id: key.composition_id,
                offset: 0,
                length: u64::MAX,
            })
            .await
            .map_err(|e| format!("nfs4 get: {e}"))?;
        Ok(resp.data.len())
    }
}

// ---------------------------------------------------------------------------
// pNFS Flexible Files
// ---------------------------------------------------------------------------
//
// Write path: NFSv4.1 OPEN+WRITE+COMMIT against the MDS — same shape
// as the Nfs4Driver. Read path: LAYOUTGET against the MDS to get a
// per-stripe fh + DS uaddr, connect to the DS, EXCHANGE_ID +
// CREATE_SESSION + (SEQUENCE+PUTFH+READ). That's what the Linux
// kernel pNFS client does.
//
// Profiling-relevant wrinkle: kernel pNFS reuses the layout for the
// composition's TTL (~5 min). We do the same — cache (comp_id →
// (uaddr, fh)) on first read; subsequent reads of the same comp
// skip LAYOUTGET. Without the cache we'd be measuring 3 RPCs per
// read instead of 1.

/// Cached NFSv4.1 session — reusable across calls. The `sequence`
/// counter must be monotonically increasing per the protocol, so
/// each call increments it under the same mutex that serializes
/// the wire write. Real kernel pNFS keeps one session per (client,
/// MDS or DS) pair; the harness mirrors that for accurate
/// throughput measurements.
struct PnfsSession {
    transport: RpcTransport,
    session_id: [u8; 16],
    sequence: u32,
}

struct PnfsDriver {
    nfs_addr: SocketAddr,
    writer: Arc<Nfs4Client>,
    /// One MDS session shared by all workers — protocol allows it
    /// because the SEQUENCE op serializes through `sequence`.
    mds_session: tokio::sync::Mutex<Option<PnfsSession>>,
    /// One session per DS address. Map under a sync mutex (lookups
    /// are O(1) and brief); the per-session mutex serializes wire
    /// access. Without this cache every GET paid 2 fresh RTTs for
    /// `EXCHANGE_ID` + `CREATE_SESSION` before the actual READ.
    ds_sessions: std::sync::Mutex<
        std::collections::HashMap<SocketAddr, Arc<tokio::sync::Mutex<PnfsSession>>>,
    >,
    layout_cache:
        tokio::sync::Mutex<std::collections::HashMap<CompositionId, (SocketAddr, Vec<u8>)>>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
}

impl PnfsDriver {
    fn new(nfs_addr: SocketAddr, pool_size: usize) -> Self {
        Self {
            nfs_addr,
            writer: Arc::new(Nfs4Client::v41_with_pool(nfs_addr, pool_size)),
            mds_session: tokio::sync::Mutex::new(None),
            ds_sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
            layout_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default")),
        }
    }

    fn open_session(addr: SocketAddr, owner: &[u8]) -> Result<PnfsSession, String> {
        let mut transport =
            RpcTransport::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
        let (client_id, _) = exchange_id(&mut transport, owner)?;
        let session_id = create_session(&mut transport, client_id)?;
        Ok(PnfsSession {
            transport,
            session_id,
            sequence: 1,
        })
    }

    fn ds_session(&self, addr: SocketAddr) -> Result<Arc<tokio::sync::Mutex<PnfsSession>>, String> {
        // Fast path — already cached.
        {
            let m = self
                .ds_sessions
                .lock()
                .map_err(|e| format!("ds map: {e}"))?;
            if let Some(s) = m.get(&addr) {
                return Ok(Arc::clone(s));
            }
        }
        // Slow path — create a session outside the map lock so
        // concurrent first-time-misses on different addrs don't
        // serialize through it. Last writer wins on duplicate inserts;
        // the loser drops their freshly-built session, which is fine.
        let sess = Self::open_session(addr, b"pnfs-profile-ds")?;
        let arc = Arc::new(tokio::sync::Mutex::new(sess));
        let mut m = self
            .ds_sessions
            .lock()
            .map_err(|e| format!("ds map: {e}"))?;
        Ok(Arc::clone(m.entry(addr).or_insert(arc)))
    }

    /// LAYOUTGET against the MDS for `comp_id`, then GETDEVICEINFO
    /// per device. Returns the first (uaddr, fh) pair so the caller
    /// can connect to the DS directly.
    async fn fetch_layout(&self, comp_id: CompositionId) -> Result<(SocketAddr, Vec<u8>), String> {
        let mut guard = self.mds_session.lock().await;
        if guard.is_none() {
            *guard = Some(Self::open_session(self.nfs_addr, b"pnfs-profile-mds")?);
        }
        let sess = guard
            .as_mut()
            .expect("transport not initialized — call connect() first");

        // SEQUENCE + PUTROOTFH + LOOKUP + LAYOUTGET.
        let seq = sess.sequence;
        sess.sequence += 1;
        let mut body = XdrWriter::new();
        body.write_u32(0);
        body.write_u32(1);
        body.write_u32(4);
        body.write_u32(op::SEQUENCE);
        body.write_opaque_fixed(&sess.session_id);
        body.write_u32(seq);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(op::PUTROOTFH);
        body.write_u32(op::LOOKUP);
        body.write_string(&comp_id.0.to_string());
        body.write_u32(op::LAYOUTGET);
        body.write_bool(false);
        body.write_u32(4); // FF
        body.write_u32(1); // READ
        body.write_u64(0);
        body.write_u64(u64::MAX);
        body.write_u64(0);
        body.write_opaque_fixed(&[0u8; 16]);
        body.write_u32(65_536);

        let reply = sess
            .transport
            .call(100_003, 4, 1, &body.into_bytes())
            .map_err(|e| format!("LAYOUTGET COMPOUND: {e}"))?;
        let (device_id, fh) = parse_layoutget_first(&reply)?;

        // GETDEVICEINFO for that device.
        let seq = sess.sequence;
        sess.sequence += 1;
        let mut body = XdrWriter::new();
        body.write_u32(0);
        body.write_u32(1);
        body.write_u32(2);
        body.write_u32(op::SEQUENCE);
        body.write_opaque_fixed(&sess.session_id);
        body.write_u32(seq);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(op::GETDEVICEINFO);
        body.write_opaque_fixed(&device_id);
        body.write_u32(4);
        body.write_u32(65_536);
        body.write_u32(0);
        let reply = sess
            .transport
            .call(100_003, 4, 1, &body.into_bytes())
            .map_err(|e| format!("GETDEVICEINFO COMPOUND: {e}"))?;
        let uaddr = parse_getdeviceinfo_first(&reply)?;
        let addr = uaddr_to_socket(&uaddr).ok_or_else(|| format!("bad uaddr {uaddr}"))?;
        Ok((addr, fh))
    }

    async fn ds_read(&self, addr: SocketAddr, fh: &[u8], length: usize) -> Result<usize, String> {
        let arc = self.ds_session(addr)?;
        let mut sess = arc.lock().await;

        let seq = sess.sequence;
        sess.sequence += 1;
        let mut body = XdrWriter::new();
        body.write_u32(0);
        body.write_u32(1);
        body.write_u32(3);
        body.write_u32(op::SEQUENCE);
        body.write_opaque_fixed(&sess.session_id);
        body.write_u32(seq);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(op::PUTFH);
        body.write_opaque(fh);
        body.write_u32(op::READ);
        body.write_opaque_fixed(&[0u8; 16]);
        body.write_u64(0);
        body.write_u32(u32::try_from(length).unwrap_or(u32::MAX));

        let reply = sess
            .transport
            .call(100_003, 4, 1, &body.into_bytes())
            .map_err(|e| format!("DS READ COMPOUND: {e}"))?;
        let mut r = XdrReader::new(&reply);
        let _ = r.read_u32().map_err(|e| format!("status: {e}"))?;
        let _ = r.read_opaque();
        let _ = r.read_u32();
        // SEQUENCE
        let _ = r.read_u32();
        let seq_st = r.read_u32().map_err(|e| format!("seq: {e}"))?;
        if seq_st != 0 {
            return Err(format!("DS SEQUENCE failed: {seq_st}"));
        }
        let _ = r.read_opaque_fixed(16);
        for _ in 0..5 {
            let _ = r.read_u32();
        }
        // PUTFH
        let _ = r.read_u32();
        let pf_st = r.read_u32().map_err(|e| format!("putfh: {e}"))?;
        if pf_st != 0 {
            return Err(format!("DS PUTFH failed: {pf_st}"));
        }
        // READ
        let _ = r.read_u32();
        let rd_st = r.read_u32().map_err(|e| format!("read: {e}"))?;
        if rd_st != 0 {
            return Err(format!("DS READ failed: {rd_st}"));
        }
        let _eof = r.read_bool();
        let data = r.read_opaque().map_err(|e| format!("data: {e}"))?;
        Ok(data.len())
    }
}

#[async_trait]
impl Driver for PnfsDriver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let resp = self
            .writer
            .write(WriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data: payload.to_vec(),
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await
            .map_err(|e| format!("pnfs put: {e}"))?;
        Ok(Key {
            composition_id: resp.composition_id,
            name: None,
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        let cached = {
            let cache = self.layout_cache.lock().await;
            cache.get(&key.composition_id).cloned()
        };
        let (addr, fh) = if let Some(v) = cached {
            v
        } else {
            let v = self.fetch_layout(key.composition_id).await?;
            self.layout_cache
                .lock()
                .await
                .insert(key.composition_id, v.clone());
            v
        };
        // The DS GET via the Linux kernel reads u32::MAX → server
        // bounded by composition size. Mirror that here.
        self.ds_read(addr, &fh, 4 * 1024 * 1024).await
    }
}

// ---------------------------------------------------------------------------
// FUSE → GatewayOps → S3 wire
// ---------------------------------------------------------------------------
//
// `KisekiFuse` is a sync POSIX-style API backed by an async
// `GatewayOps` impl. We point it at `RemoteHttpGateway` so every
// `fs.create()` is a real HTTP PUT to the running server, every
// `fs.read()` is a real HTTP GET. The KisekiFuse instance manages
// its own internal tokio runtime; we run each op via
// `spawn_blocking` so the outer worker stays async.

struct FuseDriver {
    /// One shared `KisekiFuse` instance. The wrapped Mutex
    /// serializes the `&mut self` POSIX ops (create/write/unlink) —
    /// this matches a real kernel-mounted FUSE which has one inode
    /// table per mount and per-inode locking. Re-creating the FS
    /// per call would spawn a new runtime thread per op (`KisekiFuse`
    /// owns a dedicated runtime) and quickly hit thread-spawn EAGAIN
    /// at any non-trivial concurrency.
    ///
    /// `tokio::sync::Mutex`, not `std::sync::Mutex` — `put`/`get`
    /// hold this across `KisekiFuse`'s internal `block_on`, so a
    /// std mutex would block tokio worker threads under concurrency
    /// (same starvation pattern fixed for `Nfs4Client`). Measured
    /// pre-fix: c=1 p99 = 630µs, c=16 p99 = 218 ms.
    fs: tokio::sync::Mutex<kiseki_client::fuse_fs::KisekiFuse<RemoteHttpGateway>>,
}

impl FuseDriver {
    fn new(s3_base: &str) -> Self {
        let gateway = RemoteHttpGateway::new(s3_base);
        let fs = kiseki_client::fuse_fs::KisekiFuse::new(
            gateway,
            OrgId(uuid::Uuid::from_u128(1)),
            NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default")),
        );
        Self {
            fs: tokio::sync::Mutex::new(fs),
        }
    }
}

#[async_trait]
impl Driver for FuseDriver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let payload = payload.to_vec();
        let name = format!("fuse-prof-{}", uuid::Uuid::new_v4().simple());
        let name_for_return = name.clone();
        let mut fs = self.fs.lock().await;
        // KisekiFuse handles the gateway round-trip on a dedicated
        // tokio runtime via block_on, so this `create` is sync from
        // our perspective. The outer mutex is tokio::sync::Mutex so
        // contended acquirers yield instead of blocking workers.
        fs.create(&name, payload)
            .map_err(|e| format!("fuse create errno {e}"))?;
        Ok(Key {
            composition_id: CompositionId(uuid::Uuid::nil()),
            name: Some(name_for_return),
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        let name = key
            .name
            .clone()
            .ok_or_else(|| "fuse get: key missing name".to_owned())?;
        let fs = self.fs.lock().await;
        let attr = fs
            .lookup(&name)
            .map_err(|e| format!("fuse lookup errno {e}"))?;
        let bytes = fs
            .read(attr.ino, 0, u32::try_from(attr.size).unwrap_or(u32::MAX))
            .map_err(|e| format!("fuse read errno {e}"))?;
        Ok(bytes.len())
    }
}

// ---------------------------------------------------------------------------
// NFSv4.1 helpers shared by Pnfs driver
// ---------------------------------------------------------------------------

fn exchange_id(transport: &mut RpcTransport, owner: &[u8]) -> Result<(u64, [u8; 16]), String> {
    let mut body = XdrWriter::new();
    body.write_u32(0);
    body.write_u32(1);
    body.write_u32(1);
    body.write_u32(op::EXCHANGE_ID);
    body.write_opaque_fixed(&[0u8; 8]);
    body.write_opaque(owner);
    body.write_u32(0);
    body.write_u32(0);
    body.write_u32(0);
    let reply = transport
        .call(100_003, 4, 1, &body.into_bytes())
        .map_err(|e| format!("EXCHANGE_ID call: {e}"))?;
    let mut r = XdrReader::new(&reply);
    let _ = r.read_u32().map_err(|e| format!("st: {e}"))?;
    let _ = r.read_opaque();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let st = r.read_u32().map_err(|e| format!("op_st: {e}"))?;
    if st != 0 {
        return Err(format!("EXCHANGE_ID returned {st}"));
    }
    let cid = r.read_u64().map_err(|e| format!("client_id: {e}"))?;
    Ok((cid, [0u8; 16]))
}

fn create_session(transport: &mut RpcTransport, client_id: u64) -> Result<[u8; 16], String> {
    let mut body = XdrWriter::new();
    body.write_u32(0);
    body.write_u32(1);
    body.write_u32(1);
    body.write_u32(op::CREATE_SESSION);
    body.write_u64(client_id);
    body.write_u32(1);
    body.write_u32(0);
    let reply = transport
        .call(100_003, 4, 1, &body.into_bytes())
        .map_err(|e| format!("CREATE_SESSION call: {e}"))?;
    let mut r = XdrReader::new(&reply);
    let _ = r.read_u32().map_err(|e| format!("st: {e}"))?;
    let _ = r.read_opaque();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let st = r.read_u32().map_err(|e| format!("op_st: {e}"))?;
    if st != 0 {
        return Err(format!("CREATE_SESSION returned {st}"));
    }
    let bytes = r
        .read_opaque_fixed(16)
        .map_err(|e| format!("session_id: {e}"))?;
    let mut sid = [0u8; 16];
    sid.copy_from_slice(&bytes);
    Ok(sid)
}

fn parse_layoutget_first(reply: &[u8]) -> Result<([u8; 16], Vec<u8>), String> {
    let mut r = XdrReader::new(reply);
    let _ = r.read_u32().map_err(|e| format!("compound st: {e}"))?;
    let _ = r.read_opaque();
    let _ = r.read_u32();
    // SEQUENCE
    let _ = r.read_u32();
    let st = r.read_u32().map_err(|e| format!("seq: {e}"))?;
    if st != 0 {
        return Err(format!("SEQUENCE failed: {st}"));
    }
    let _ = r.read_opaque_fixed(16);
    for _ in 0..5 {
        let _ = r.read_u32();
    }
    // PUTROOTFH
    let _ = r.read_u32();
    let st = r.read_u32().map_err(|e| format!("putrootfh: {e}"))?;
    if st != 0 {
        return Err(format!("PUTROOTFH failed: {st}"));
    }
    // LOOKUP
    let _ = r.read_u32();
    let st = r.read_u32().map_err(|e| format!("lookup: {e}"))?;
    if st != 0 {
        return Err(format!("LOOKUP failed: {st}"));
    }
    // LAYOUTGET
    let _ = r.read_u32();
    let st = r.read_u32().map_err(|e| format!("layoutget: {e}"))?;
    if st != 0 {
        return Err(format!("LAYOUTGET failed: {st}"));
    }
    let _roc = r.read_bool();
    let _stateid = r.read_opaque_fixed(16);
    let n_segments = r.read_u32().map_err(|e| format!("segments: {e}"))? as usize;
    if n_segments == 0 {
        return Err("LAYOUTGET returned 0 segments".into());
    }
    let _ = r.read_u64();
    let _ = r.read_u64();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let body = r.read_opaque().map_err(|e| format!("layout body: {e}"))?;
    let mut br = XdrReader::new(&body);
    let _stripe_unit = br.read_u64();
    let n_mirrors = br.read_u32().map_err(|e| format!("mirrors: {e}"))? as usize;
    if n_mirrors == 0 {
        return Err("FF body has 0 mirrors".into());
    }
    let n_ds = br.read_u32().map_err(|e| format!("ds: {e}"))? as usize;
    if n_ds == 0 {
        return Err("FF mirror has 0 data servers".into());
    }
    let did = br.read_opaque_fixed(16).map_err(|e| format!("did: {e}"))?;
    let mut device_id = [0u8; 16];
    device_id.copy_from_slice(&did);
    let _ = br.read_u32(); // efficiency
    let _ = br.read_opaque_fixed(16); // stateid
    let n_fh = br.read_u32().map_err(|e| format!("fh count: {e}"))?;
    if n_fh == 0 {
        return Err("FF data server has 0 fhs".into());
    }
    let fh = br.read_opaque().map_err(|e| format!("fh: {e}"))?;
    Ok((device_id, fh))
}

fn parse_getdeviceinfo_first(reply: &[u8]) -> Result<String, String> {
    let mut r = XdrReader::new(reply);
    let _ = r.read_u32();
    let _ = r.read_opaque();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let st = r.read_u32().map_err(|e| format!("seq st: {e}"))?;
    if st != 0 {
        return Err(format!("SEQUENCE failed: {st}"));
    }
    let _ = r.read_opaque_fixed(16);
    for _ in 0..5 {
        let _ = r.read_u32();
    }
    let _ = r.read_u32();
    let st = r.read_u32().map_err(|e| format!("gdi st: {e}"))?;
    if st != 0 {
        return Err(format!("GETDEVICEINFO failed: {st}"));
    }
    let _layout_type = r.read_u32();
    let body = r.read_opaque().map_err(|e| format!("gdi body: {e}"))?;
    let mut br = XdrReader::new(&body);
    let n_addrs = br.read_u32().map_err(|e| format!("netaddrs: {e}"))?;
    if n_addrs == 0 {
        return Err("GETDEVICEINFO returned 0 netaddrs".into());
    }
    let _netid = br.read_string().map_err(|e| format!("netid: {e}"))?;
    let uaddr = br.read_string().map_err(|e| format!("uaddr: {e}"))?;
    Ok(uaddr)
}

fn uaddr_to_socket(uaddr: &str) -> Option<SocketAddr> {
    let parts: Vec<&str> = uaddr.split('.').collect();
    if parts.len() != 6 {
        return None;
    }
    let ip = format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3]);
    let hi: u16 = parts[4].parse().ok()?;
    let lo: u16 = parts[5].parse().ok()?;
    let port = (hi << 8) | lo;
    format!("{ip}:{port}").parse().ok()
}

// ---------------------------------------------------------------------------
// In-process gateway floor (ADR-042 graduation gate)
// ---------------------------------------------------------------------------

/// Drives `InMemoryGateway` directly with no IPC. Measures the upper
/// bound any wire protocol could possibly serve at this hardware.
struct InProcessDriver {
    gateway: kiseki_gateway::mem_gateway::InMemoryGateway,
    namespace_id: NamespaceId,
    tenant_id: OrgId,
}

impl InProcessDriver {
    fn new() -> Self {
        use kiseki_chunk::store::ChunkStore;
        use kiseki_common::ids::ShardId;
        use kiseki_common::tenancy::KeyEpoch;
        use kiseki_composition::composition::CompositionStore;
        use kiseki_composition::namespace::Namespace;
        use kiseki_crypto::keys::SystemMasterKey;
        use kiseki_gateway::mem_gateway::InMemoryGateway;


        let tenant_id = OrgId(uuid::Uuid::from_u128(100));
        let namespace_id = NamespaceId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            b"in-process-floor",
        ));
        let compositions = CompositionStore::new();
        compositions.add_namespace(Namespace {
            id: namespace_id,
            tenant_id,
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        });
        let chunks = ChunkStore::new();
        let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let gateway =
            InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);
        Self {
            gateway,
            namespace_id,
            tenant_id,
        }
    }
}

#[async_trait]
impl Driver for InProcessDriver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let resp = self
            .gateway
            .write(WriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data: payload.to_vec(),
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await
            .map_err(|e| format!("in-process put: {e}"))?;
        Ok(Key {
            composition_id: resp.composition_id,
            name: None,
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        let resp = self
            .gateway
            .read(kiseki_gateway::ops::ReadRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                composition_id: key.composition_id,
                offset: 0,
                length: u64::MAX,
            })
            .await
            .map_err(|e| format!("in-process get: {e}"))?;
        Ok(resp.data.len())
    }
}

// ---------------------------------------------------------------------------
// Native gRPC (ADR-042)
// ---------------------------------------------------------------------------

/// Drives the `kiseki.v1.native.GatewayDataService` over real tonic
/// gRPC. The harness's `ProfileServer` runs in plaintext mode (no TLS
/// material), so the `SanInterceptor` falls through to the synthetic
/// "dev" tenant principal — the cross-check against the payload's
/// `tenant_id` is a no-op in that posture, matching what S3 and NFS
/// drivers do today (they use the same single-tenant cluster).
pub struct NativeDriver {
    /// Pool of pre-built gRPC clients, one per backing TCP / h2
    /// connection. tonic's default `Channel::connect()` produces a
    /// single-connection channel; on the server side, each h2
    /// connection is processed by ONE tokio task — so when many
    /// concurrent unary RPCs share one connection, the server-side
    /// h2 stream polling serializes through that single task and
    /// CPU stays pinned to one core regardless of concurrency.
    /// Holding N independent channels gives the server N parallel
    /// h2-connection tasks; the workers round-robin across the pool
    /// so streams fan out across cores.
    clients: Vec<kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient<
        tonic::transport::Channel,
    >>,
    /// Per-call selector — atomic so workers don't all hash to slot 0.
    next: std::sync::atomic::AtomicUsize,
    namespace_id: NamespaceId,
    tenant_id: OrgId,
}

impl NativeDriver {
    pub async fn new(grpc_addr: &str) -> Result<Self, String> {
        // Pool size: matches the typical worker concurrency (16) so
        // every worker gets its own connection. Configurable via
        // KISEKI_NATIVE_DRIVER_POOL.
        let pool_size: usize = std::env::var("KISEKI_NATIVE_DRIVER_POOL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16);
        let mut clients = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{grpc_addr}"))
                .map_err(|e| format!("native endpoint: {e}"))?
                .tcp_nodelay(true)
                // Match the server's HTTP/2 flow-control windows
                // (runtime.rs sets 16 MiB stream / 32 MiB connection).
                .initial_stream_window_size(Some(16 * 1024 * 1024))
                .initial_connection_window_size(Some(32 * 1024 * 1024))
                .timeout(std::time::Duration::from_secs(30));
            let channel = endpoint
                .connect()
                .await
                .map_err(|e| format!("native connect: {e}"))?;
            let client = kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient::new(
                channel,
            )
            .max_decoding_message_size(64 * 1024 * 1024)
            .max_encoding_message_size(64 * 1024 * 1024);
            clients.push(client);
        }
        Ok(Self {
            clients,
            next: std::sync::atomic::AtomicUsize::new(0),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default")),
        })
    }

    fn pick_client(
        &self,
    ) -> kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient<
        tonic::transport::Channel,
    > {
        let idx = self
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.clients.len();
        self.clients[idx].clone()
    }

    fn ctrl(&self) -> kiseki_proto::v1::native::ControlFields {
        kiseki_proto::v1::native::ControlFields {
            tenant_id: Some(kiseki_proto::v1::OrgId {
                value: self.tenant_id.0.to_string(),
            }),
            // 16-byte random idempotency key per call. Dedup window
            // bookkeeping isn't wired in v1 (Phase 4 follow-up), but
            // a unique key per call is the right shape for when it
            // is.
            idempotency_key: uuid::Uuid::new_v4().as_bytes().to_vec(),
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
        }
    }
}

#[async_trait]
impl Driver for NativeDriver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let mut client = self.pick_client();
        let req = tonic::Request::new(kiseki_proto::v1::native::PutObjectRequest {
            control: Some(self.ctrl()),
            namespace_id: Some(kiseki_proto::v1::NamespaceId {
                value: self.namespace_id.0.to_string(),
            }),
            name: format!("perf-{}", uuid::Uuid::new_v4().simple()),
            data: payload.to_vec(),
        });
        let resp = client
            .put_object(req)
            .await
            .map_err(|e| format!("native put: {e}"))?
            .into_inner();
        let comp = resp
            .composition_id
            .ok_or_else(|| "native put: response missing composition_id".to_string())?;
        let uuid = uuid::Uuid::parse_str(&comp.value)
            .map_err(|e| format!("native put: composition_id parse: {e}"))?;
        Ok(Key {
            composition_id: kiseki_common::ids::CompositionId(uuid),
            name: None,
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        let mut client = self.pick_client();
        let req = tonic::Request::new(kiseki_proto::v1::native::GetObjectRequest {
            control: Some(self.ctrl()),
            namespace_id: Some(kiseki_proto::v1::NamespaceId {
                value: self.namespace_id.0.to_string(),
            }),
            range_start: 0,
            range_end: 0,
            key: Some(kiseki_proto::v1::native::get_object_request::Key::CompositionId(
                kiseki_proto::v1::CompositionId {
                    value: key.composition_id.0.to_string(),
                },
            )),
        });
        let resp = client
            .get_object(req)
            .await
            .map_err(|e| format!("native get: {e}"))?
            .into_inner();
        Ok(resp.data.len())
    }
}

// ---------------------------------------------------------------------------
// In-process gateway WITH the persistent stores (the "transport-tax floor")
// ---------------------------------------------------------------------------

/// Same gateway shape `kiseki-server` runs (redb-backed
/// `CompositionStore`, raw-block `PersistentChunkStore` with
/// group-commit fsync), but called directly in this process — no
/// gRPC, no h2, no tonic. The gap between this driver's throughput
/// and the [`InProcessDriver`]'s pure-RAM measurement is the
/// **persistence tax**; the gap between this driver and the
/// [`NativeDriver`] is the **transport tax**.
///
/// `kiseki-server` also runs Raft + view store + workflow table +
/// metrics histograms; we deliberately omit those because they're
/// orthogonal to the per-write cost we're trying to bound. With
/// the listed bits in place a single PUT exercises the same redb
/// commit + chunk device write + group-commit fsync pipeline the
/// production server pays.
pub struct InProcessPersistentDriver {
    gateway: kiseki_gateway::mem_gateway::InMemoryGateway,
    namespace_id: NamespaceId,
    tenant_id: OrgId,
    /// Tempdir owning the redb files + raw block device. Drops at
    /// the end of the run.
    _data_dir: tempfile::TempDir,
}

impl InProcessPersistentDriver {
    pub async fn new() -> Result<Self, String> {
        use kiseki_common::ids::ShardId;
        use kiseki_common::tenancy::KeyEpoch;
        use kiseki_composition::composition::CompositionStore;
        use kiseki_composition::namespace::Namespace;
        use kiseki_crypto::keys::SystemMasterKey;
        use kiseki_gateway::mem_gateway::InMemoryGateway;

        let data_dir = tempfile::tempdir()
            .map_err(|e| format!("InProcessPersistent tempdir: {e}"))?;
        let dir = data_dir.path();

        // 1. Persistent chunk store — same shape as runtime.rs:
        //    raw block device + group-commit fsync (sync_per_write =
        //    false). 4 GiB is plenty for a 30 s run; the spawned
        //    `kiseki-server` uses the same default.
        std::fs::create_dir_all(dir.join("chunks"))
            .map_err(|e| format!("chunks dir: {e}"))?;
        let dev_path = dir.join("chunks").join("data.dev");
        let meta_path = dir.join("chunks").join("meta.json");
        let chunks = kiseki_chunk::PersistentChunkStore::init(
            &dev_path,
            &meta_path,
            4 * 1024 * 1024 * 1024,
        )
        .map_err(|e| format!("chunk store init: {e}"))?;
        chunks.set_sync_per_write(false);
        let chunks_async = kiseki_chunk::arc_async(chunks);

        // 2. Persistent composition store — redb-backed, with the
        //    same write-behind queue config the runtime uses by
        //    default (KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100).
        std::fs::create_dir_all(dir.join("metadata"))
            .map_err(|e| format!("metadata dir: {e}"))?;
        let comp_path = dir.join("metadata").join("compositions.redb");
        let interval_ms = std::env::var("KISEKI_COMPOSITION_FLUSH_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(100);
        let mut comp_store_redb =
            kiseki_composition::persistent::PersistentRedbStorage::open(&comp_path)
                .map_err(|e| format!("composition redb open: {e}"))?
                .with_eventual_durability(true);
        let drainer = comp_store_redb.enable_write_behind(
            4096,
            std::time::Duration::from_millis(interval_ms),
            1024,
        );
        tokio::spawn(drainer.run());
        let comp_storage: Box<dyn kiseki_composition::persistent::CompositionStorage> =
            Box::new(comp_store_redb);
        let compositions = CompositionStore::with_storage(comp_storage);

        // 3. Single-tenant namespace registration so PUT/GET have
        //    a target. Distinct from the runtime's bootstrap tenant
        //    (Uuid::from_u128(1)) so this driver doesn't share state
        //    with anything else if the run is invoked in a shared
        //    workspace.
        let tenant_id = OrgId(uuid::Uuid::from_u128(101));
        let namespace_id = NamespaceId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            b"in-process-persistent-floor",
        ));
        compositions.add_namespace(Namespace {
            id: namespace_id,
            tenant_id,
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        });

        // 4. Build the InMemoryGateway with these persistent stores.
        let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let gateway = InMemoryGateway::new(compositions, chunks_async, master_key);
        Ok(Self {
            gateway,
            namespace_id,
            tenant_id,
            _data_dir: data_dir,
        })
    }
}

#[async_trait]
impl Driver for InProcessPersistentDriver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let resp = self
            .gateway
            .write(WriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data: payload.to_vec(),
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await
            .map_err(|e| format!("in-process-persistent put: {e}"))?;
        Ok(Key {
            composition_id: resp.composition_id,
            name: None,
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        let resp = self
            .gateway
            .read(kiseki_gateway::ops::ReadRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                composition_id: key.composition_id,
                offset: 0,
                length: u64::MAX,
            })
            .await
            .map_err(|e| format!("in-process-persistent get: {e}"))?;
        Ok(resp.data.len())
    }
}
