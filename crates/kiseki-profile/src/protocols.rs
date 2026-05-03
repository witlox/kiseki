//! Per-protocol workload drivers. Every driver exposes the same
//! `put` / `get` shape so the worker loop in `main` is protocol-
//! agnostic; the actual wire path is whatever the underlying client
//! does (HTTP, NFSv3 RPCs, NFSv4 COMPOUNDs, pNFS LAYOUTGET → DS,
//! FUSE → GatewayOps → S3).

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
/// just stash the composition_id; FUSE additionally tracks its own
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

pub async fn build(protocol: Protocol, server: &ProfileServer) -> Result<Arc<dyn Driver>, String> {
    match protocol {
        Protocol::S3 => Ok(Arc::new(S3Driver::new(&server.s3_base))),
        Protocol::Nfs3 => Ok(Arc::new(Nfs3Driver::new(server.nfs_addr))),
        Protocol::Nfs4 => Ok(Arc::new(Nfs4Driver::new(server.nfs_addr))),
        Protocol::Pnfs => Ok(Arc::new(PnfsDriver::new(server.nfs_addr))),
        Protocol::Fuse => Ok(Arc::new(FuseDriver::new(&server.s3_base))),
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
            namespace_id: NamespaceId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_DNS,
                b"default",
            )),
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
    fn new(nfs_addr: SocketAddr) -> Self {
        Self {
            inner: Arc::new(Nfs3Client::new(nfs_addr)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_DNS,
                b"default",
            )),
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
    fn new(nfs_addr: SocketAddr) -> Self {
        Self {
            inner: Arc::new(Nfs4Client::v41(nfs_addr)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_DNS,
                b"default",
            )),
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

struct PnfsDriver {
    nfs_addr: SocketAddr,
    writer: Arc<Nfs4Client>,
    layout_cache:
        tokio::sync::Mutex<std::collections::HashMap<CompositionId, (SocketAddr, Vec<u8>)>>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
}

impl PnfsDriver {
    fn new(nfs_addr: SocketAddr) -> Self {
        Self {
            nfs_addr,
            writer: Arc::new(Nfs4Client::v41(nfs_addr)),
            layout_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_DNS,
                b"default",
            )),
        }
    }

    /// LAYOUTGET against the MDS for `comp_id`, then GETDEVICEINFO
    /// per device. Returns the first (uaddr, fh) pair so the caller
    /// can connect to the DS directly.
    async fn fetch_layout(
        &self,
        comp_id: CompositionId,
    ) -> Result<(SocketAddr, Vec<u8>), String> {
        let mut transport =
            RpcTransport::connect(self.nfs_addr).map_err(|e| format!("MDS connect: {e}"))?;
        let (client_id, _) = exchange_id(&mut transport, b"pnfs-profile-mds")?;
        let session_id = create_session(&mut transport, client_id)?;

        // SEQUENCE + PUTROOTFH + LOOKUP + LAYOUTGET.
        let mut body = XdrWriter::new();
        body.write_u32(0);
        body.write_u32(1);
        body.write_u32(4);
        body.write_u32(op::SEQUENCE);
        body.write_opaque_fixed(&session_id);
        body.write_u32(2);
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

        let reply = transport
            .call(100_003, 4, 1, &body.into_bytes())
            .map_err(|e| format!("LAYOUTGET COMPOUND: {e}"))?;
        let (device_id, fh) = parse_layoutget_first(&reply)?;

        // GETDEVICEINFO for that device.
        let mut body = XdrWriter::new();
        body.write_u32(0);
        body.write_u32(1);
        body.write_u32(2);
        body.write_u32(op::SEQUENCE);
        body.write_opaque_fixed(&session_id);
        body.write_u32(3);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(op::GETDEVICEINFO);
        body.write_opaque_fixed(&device_id);
        body.write_u32(4);
        body.write_u32(65_536);
        body.write_u32(0);
        let reply = transport
            .call(100_003, 4, 1, &body.into_bytes())
            .map_err(|e| format!("GETDEVICEINFO COMPOUND: {e}"))?;
        let uaddr = parse_getdeviceinfo_first(&reply)?;
        let addr = uaddr_to_socket(&uaddr).ok_or_else(|| format!("bad uaddr {uaddr}"))?;
        Ok((addr, fh))
    }

    async fn ds_read(addr: SocketAddr, fh: &[u8], length: usize) -> Result<usize, String> {
        let mut transport =
            RpcTransport::connect(addr).map_err(|e| format!("DS connect {addr}: {e}"))?;
        let (client_id, _) = exchange_id(&mut transport, b"pnfs-profile-ds")?;
        let session_id = create_session(&mut transport, client_id)?;

        let mut body = XdrWriter::new();
        body.write_u32(0);
        body.write_u32(1);
        body.write_u32(3);
        body.write_u32(op::SEQUENCE);
        body.write_opaque_fixed(&session_id);
        body.write_u32(2);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(op::PUTFH);
        body.write_opaque(fh);
        body.write_u32(op::READ);
        body.write_opaque_fixed(&[0u8; 16]);
        body.write_u64(0);
        body.write_u32(u32::try_from(length).unwrap_or(u32::MAX));

        let reply = transport
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
            self.layout_cache.lock().await.insert(key.composition_id, v.clone());
            v
        };
        // The DS GET via the Linux kernel reads u32::MAX → server
        // bounded by composition size. Mirror that here.
        Self::ds_read(addr, &fh, 4 * 1024 * 1024).await
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
    /// per call would spawn a new runtime thread per op (KisekiFuse
    /// owns a dedicated runtime) and quickly hit thread-spawn EAGAIN
    /// at any non-trivial concurrency.
    fs: std::sync::Mutex<kiseki_client::fuse_fs::KisekiFuse<RemoteHttpGateway>>,
}

impl FuseDriver {
    fn new(s3_base: &str) -> Self {
        let gateway = RemoteHttpGateway::new(s3_base);
        let fs = kiseki_client::fuse_fs::KisekiFuse::new(
            gateway,
            OrgId(uuid::Uuid::from_u128(1)),
            NamespaceId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_DNS,
                b"default",
            )),
        );
        Self {
            fs: std::sync::Mutex::new(fs),
        }
    }
}

#[async_trait]
impl Driver for FuseDriver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let payload = payload.to_vec();
        let name = format!("fuse-prof-{}", uuid::Uuid::new_v4().simple());
        let name_for_return = name.clone();
        let mut fs = self
            .fs
            .lock()
            .map_err(|e| format!("fuse lock: {e}"))?;
        // KisekiFuse handles the gateway round-trip on a dedicated
        // tokio runtime via block_on, so this `create` is sync from
        // our perspective. We're already inside the outer worker's
        // async context but block_in_place isn't safe to use from
        // the std mutex guard — KisekiFuse uses block_on directly.
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
        let fs = self
            .fs
            .lock()
            .map_err(|e| format!("fuse lock: {e}"))?;
        let attr = fs.lookup(&name).map_err(|e| format!("fuse lookup errno {e}"))?;
        let bytes = fs
            .read(attr.ino, 0, attr.size as u32)
            .map_err(|e| format!("fuse read errno {e}"))?;
        Ok(bytes.len())
    }
}

// ---------------------------------------------------------------------------
// NFSv4.1 helpers shared by Pnfs driver
// ---------------------------------------------------------------------------

fn exchange_id(
    transport: &mut RpcTransport,
    owner: &[u8],
) -> Result<(u64, [u8; 16]), String> {
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
