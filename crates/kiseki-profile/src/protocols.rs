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

use crate::harness::{Cluster, ProfileServer};
use crate::{NativeBinding, Protocol};

/// Endpoints + tenant/namespace identity the per-protocol drivers need.
/// Single-node mode picks endpoints from [`ProfileServer`]; multi-node
/// picks them from [`Cluster`]'s leader handle. Same shape either way
/// — the drivers don't care which spawned the server(s).
///
/// The `tenant_id` / `namespace_id` pair is fixed at construction so
/// the multi-node path can drive a separate bench namespace
/// (`kiseki-bench`) without colliding with the system `default`
/// namespace's single-shard topology.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub s3_base: String,
    pub nfs_addr: SocketAddr,
    /// Surface-only — kept for parity with `ProfileServer::ds_addr`
    /// and for logging. The pNFS driver discovers DS addresses via
    /// LAYOUTGET at runtime; no driver currently constructs from
    /// this field directly.
    #[allow(dead_code)]
    pub ds_addr: SocketAddr,
    pub tcp_framed_port: u16,
    pub grpc_data_port: u16,
    pub tenant_id: OrgId,
    pub namespace_id: NamespaceId,
}

impl Endpoints {
    /// Single-node — drive the `default` namespace seeded by bootstrap.
    pub fn from_profile_server(s: &ProfileServer) -> Self {
        Self {
            s3_base: s.s3_base.clone(),
            nfs_addr: s.nfs_addr,
            ds_addr: s.ds_addr,
            tcp_framed_port: s.ports.tcp_framed,
            grpc_data_port: s.ports.grpc_data,
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default")),
        }
    }

    /// Multi-node — drive the `kiseki-bench` namespace provisioned via
    /// `Cluster::provision_bench_topology`. Same UUIDs as
    /// `kiseki-client::bench::bench_default_ids`.
    pub fn from_cluster_bench(c: &Cluster) -> Self {
        Self {
            s3_base: c.leader_s3_base().to_owned(),
            nfs_addr: c.leader_nfs_addr(),
            ds_addr: c.leader_ds_addr(),
            tcp_framed_port: c.leader_tcp_framed(),
            grpc_data_port: c.leader_grpc_data(),
            tenant_id: OrgId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_DNS,
                b"kiseki-bench-tenant",
            )),
            namespace_id: NamespaceId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_DNS,
                b"kiseki-bench",
            )),
        }
    }
}

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
    binding: NativeBinding,
    endpoints: Option<&Endpoints>,
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
            let e = endpoints.ok_or("S3 driver requires --server-bin (none was passed)")?;
            Ok(Arc::new(S3Driver::new(
                &e.s3_base,
                e.tenant_id,
                e.namespace_id,
            )))
        }
        Protocol::Nfs3 => {
            let e = endpoints.ok_or("Nfs3 driver requires --server-bin")?;
            Ok(Arc::new(Nfs3Driver::new(
                e.nfs_addr,
                pool_size,
                e.tenant_id,
                e.namespace_id,
            )))
        }
        Protocol::Nfs4 => {
            let e = endpoints.ok_or("Nfs4 driver requires --server-bin")?;
            Ok(Arc::new(Nfs4Driver::new(
                e.nfs_addr,
                pool_size,
                e.tenant_id,
                e.namespace_id,
            )))
        }
        Protocol::Pnfs => {
            let e = endpoints.ok_or("Pnfs driver requires --server-bin")?;
            Ok(Arc::new(PnfsDriver::new(
                e.nfs_addr,
                pool_size,
                e.tenant_id,
                e.namespace_id,
            )))
        }
        Protocol::Fuse => {
            let e = endpoints.ok_or("Fuse driver requires --server-bin")?;
            // ADR-042: FUSE rides the TCP-framed binding directly,
            // not the S3 listener. The S3-over-HTTP path FUSE used
            // to take capped at ~7.6 k op/s; the native binding
            // measures ~70 k op/s on the same hardware.
            let addr = format!("127.0.0.1:{}", e.tcp_framed_port);
            Ok(Arc::new(
                FuseDriver::new(&addr, pool_size, e.tenant_id, e.namespace_id).await?,
            ))
        }
        Protocol::Native => {
            let e = endpoints.ok_or("Native driver requires --server-bin")?;
            // ADR-042 §16.1 phase 7: per-binding driver. `auto`
            // resolves to TCP-framed (the new default after Phase 8
            // measurement showed +70 % PUT / +183 % GET vs gRPC).
            // When the client-side selector lands TopologyCache
            // integration this branch consults it.
            match binding {
                NativeBinding::Tcp | NativeBinding::Auto => {
                    let addr = format!("127.0.0.1:{}", e.tcp_framed_port);
                    Ok(Arc::new(
                        TcpFramedNativeDriver::new(&addr, pool_size, e.tenant_id, e.namespace_id)
                            .await?,
                    ))
                }
                NativeBinding::Grpc => {
                    let addr = format!("127.0.0.1:{}", e.grpc_data_port);
                    Ok(Arc::new(
                        NativeDriver::new(&addr, e.tenant_id, e.namespace_id).await?,
                    ))
                }
            }
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
    fn new(s3_base: &str, tenant_id: OrgId, namespace_id: NamespaceId) -> Self {
        Self {
            inner: RemoteHttpGateway::new(s3_base),
            tenant_id,
            namespace_id,
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
                idempotency_key: None,

                forwarded_from_node: None,
                comp_id_override: None,
                tier: None,
                surface: kiseki_gateway::ops::WriteSurface::S3,
                base_composition_id: None,
                base_bytes: 0,
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
    fn new(
        nfs_addr: SocketAddr,
        pool_size: usize,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Self {
        Self {
            inner: Arc::new(Nfs3Client::with_pool(nfs_addr, pool_size)),
            tenant_id,
            namespace_id,
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
                idempotency_key: None,

                forwarded_from_node: None,
                comp_id_override: None,
                tier: None,
                surface: kiseki_gateway::ops::WriteSurface::Nfs,
                base_composition_id: None,
                base_bytes: 0,
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
    fn new(
        nfs_addr: SocketAddr,
        pool_size: usize,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Self {
        Self {
            inner: Arc::new(Nfs4Client::v41_with_pool(nfs_addr, pool_size)),
            tenant_id,
            namespace_id,
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
                idempotency_key: None,

                forwarded_from_node: None,
                comp_id_override: None,
                tier: None,
                surface: kiseki_gateway::ops::WriteSurface::Nfs,
                base_composition_id: None,
                base_bytes: 0,
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

/// Per-DS-address session pool. Pre-`pool_size` revision used a
/// single `Mutex<PnfsSession>` per address — every GET serialized
/// through one DS connection, capping pNFS GET at ~16 k op/s
/// (1 / per-call DS round-trip) regardless of harness concurrency.
/// This pool round-robins across N independent sessions so 16
/// concurrent driver tasks light up 16 sessions in parallel,
/// matching the [`Nfs3Client`] / [`Nfs4Client`] pool shape.
///
/// Lazy slot init: each slot starts `None`; the first lock acquirer
/// for that slot opens a session against `addr` and stores it.
/// Subsequent acquirers re-use it. Concurrent first-misses across
/// DIFFERENT slots never serialize on each other.
///
/// RFC 8881 §2.10.4 caveat: a Linux kernel pNFS client opens ONE
/// session per `(client_id, DS, principal)` and pipelines via the
/// SEQUENCE slot table. The harness opens N sessions instead —
/// over-provisioned vs the kernel client, but the simpler model
/// gives an upper-bound on what the server's DS path can sustain
/// without conflating server cost with kernel slot-table dynamics.
struct DsSessionPool {
    addr: SocketAddr,
    sessions: Vec<tokio::sync::Mutex<Option<PnfsSession>>>,
    next: std::sync::atomic::AtomicUsize,
}

impl DsSessionPool {
    fn new(addr: SocketAddr, pool_size: usize) -> Self {
        let pool_size = pool_size.max(1);
        let mut sessions = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            sessions.push(tokio::sync::Mutex::new(None));
        }
        Self {
            addr,
            sessions,
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Acquire the next slot's guard, lazily opening the DS session
    /// on first use. Round-robin selection so concurrent callers
    /// land on disjoint slots and don't serialize on a single Mutex.
    async fn acquire(&self) -> Result<tokio::sync::MutexGuard<'_, Option<PnfsSession>>, String> {
        let idx =
            self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.sessions.len();
        let mut guard = self.sessions[idx].lock().await;
        if guard.is_none() {
            *guard = Some(PnfsDriver::open_session(self.addr, b"pnfs-profile-ds")?);
        }
        Ok(guard)
    }
}

struct PnfsDriver {
    nfs_addr: SocketAddr,
    pool_size: usize,
    writer: Arc<Nfs4Client>,
    /// One MDS session shared by all workers — protocol allows it
    /// because the SEQUENCE op serializes through `sequence`. Only
    /// the first GET of a given `composition_id` touches the MDS;
    /// subsequent GETs hit `layout_cache` so MDS contention is
    /// negligible in steady state (the put-heavy path does NOT use
    /// the MDS — `writer` is a separate `Nfs4Client` pool).
    mds_session: tokio::sync::Mutex<Option<PnfsSession>>,
    /// Per-DS-address session pool. See [`DsSessionPool`] for the
    /// rationale and the round-robin selection policy.
    ds_sessions: std::sync::Mutex<std::collections::HashMap<SocketAddr, Arc<DsSessionPool>>>,
    layout_cache:
        tokio::sync::Mutex<std::collections::HashMap<CompositionId, (SocketAddr, Vec<u8>)>>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
}

impl PnfsDriver {
    fn new(
        nfs_addr: SocketAddr,
        pool_size: usize,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Self {
        Self {
            nfs_addr,
            pool_size,
            writer: Arc::new(Nfs4Client::v41_with_pool(nfs_addr, pool_size)),
            mds_session: tokio::sync::Mutex::new(None),
            ds_sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
            layout_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            tenant_id,
            namespace_id,
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

    fn ds_session_pool(&self, addr: SocketAddr) -> Result<Arc<DsSessionPool>, String> {
        // Fast path — pool already exists.
        {
            let m = self
                .ds_sessions
                .lock()
                .map_err(|e| format!("ds map: {e}"))?;
            if let Some(p) = m.get(&addr) {
                return Ok(Arc::clone(p));
            }
        }
        // Slow path — first DS observation. Build the pool outside
        // the map lock so concurrent first-time-misses on DIFFERENT
        // addrs don't serialize on it. Last writer wins on duplicate
        // inserts; the loser drops their freshly-built (empty) pool.
        let pool = Arc::new(DsSessionPool::new(addr, self.pool_size));
        let mut m = self
            .ds_sessions
            .lock()
            .map_err(|e| format!("ds map: {e}"))?;
        Ok(Arc::clone(m.entry(addr).or_insert(pool)))
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
        let pool = self.ds_session_pool(addr)?;
        let mut guard = pool.acquire().await?;
        let sess = guard
            .as_mut()
            .expect("DsSessionPool::acquire populated the slot");

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
                idempotency_key: None,

                forwarded_from_node: None,
                comp_id_override: None,
                tier: None,
                surface: kiseki_gateway::ops::WriteSurface::Nfs,
                base_composition_id: None,
                base_bytes: 0,
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
// FUSE → GatewayOps → native TCP-framed wire (ADR-042)
// ---------------------------------------------------------------------------
//
// `KisekiFuse` is a sync POSIX-style API backed by an async
// `GatewayOps` impl. We point it at `NativeRemoteGateway` so every
// `fs.create()` rides a put_object verb on the TCP-framed binding,
// every `fs.read()` rides a get_object verb. The S3-over-HTTP detour
// is gone — the V3 split-bulk wire format ships meta + payload in
// one writev syscall.
//
// **Concurrency model** — same 3-phase RwLock pattern the FUSE
// daemon uses (`fuse_daemon::FuseDaemon::create`):
//
//   Phase 1 (read lock):  build the WriteRequest. Multiple `put`s
//                         can validate + clone payload data in
//                         parallel — only the inode table is read.
//   Phase 2 (no lock):    gateway call. Other `put` / `get` callers
//                         on this FuseDriver proceed concurrently.
//                         The connection pool inside
//                         NativeRemoteGateway fans out across N
//                         server-side reader tasks.
//   Phase 3 (write lock): register the inode + parent→child link
//                         from the gateway's response. Microseconds
//                         of contention vs. milliseconds in the
//                         pre-fix outer Mutex design.
//
// Pre-fix (single `tokio::sync::Mutex<KisekiFuse>` serialized the
// whole call): c=16 GET 12.9 k op/s, PUT 7.0 k op/s — bottlenecked
// on the inode-table mutex held across the gateway call.

struct FuseDriver {
    /// `tokio::sync::RwLock` — read lock for build-request, write
    /// lock for inode-table mutation. The `KisekiFuse` instance holds
    /// a peer-cloned `NativeRemoteGateway` for API symmetry but the
    /// hot path (gateway call) bypasses it via the separate
    /// `gateway` field below.
    fs: tokio::sync::RwLock<
        kiseki_client::fuse_fs::KisekiFuse<kiseki_client::native_remote::NativeRemoteGateway>,
    >,
    /// Direct gateway handle — same connection pool the
    /// `KisekiFuse` instance holds (cheap `Arc`-shared clone). Used
    /// to `.await` the gateway call DIRECTLY from the perf-driver
    /// task instead of round-tripping through `KisekiFuse`'s
    /// `block_gateway_pub` which `block_in_place`s into a separate
    /// dedicated runtime. The detour was a measurable cap on PUT
    /// throughput (the read lock was held for the full
    /// `block_in_place` window; under c=16 the write-lock handoff
    /// then bottlenecked).
    gateway: Arc<kiseki_client::native_remote::NativeRemoteGateway>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
}

impl FuseDriver {
    async fn new(
        addr: &str,
        pool_size: usize,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Self, String> {
        let gateway =
            kiseki_client::native_remote::NativeRemoteGateway::connect_plaintext(addr, pool_size)
                .await
                .map_err(|e| format!("native-remote connect: {e}"))?;
        // Hand KisekiFuse a peer clone so it satisfies the API but
        // never actually drives the connection pool — the FuseDriver
        // hits `gateway` directly.
        let fs = kiseki_client::fuse_fs::KisekiFuse::new(gateway.clone(), tenant_id, namespace_id);
        Ok(Self {
            fs: tokio::sync::RwLock::new(fs),
            gateway: Arc::new(gateway),
            tenant_id,
            namespace_id,
        })
    }
}

#[async_trait]
impl Driver for FuseDriver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        let payload = payload.to_vec();
        let name = format!("fuse-prof-{}", uuid::Uuid::new_v4().simple());

        // Phase 1 — build the WriteRequest under a read lock. Many
        // concurrent puts can do this in parallel.
        let req = {
            let fs = self.fs.read().await;
            fs.create_build_request(1, &name, payload)
                .map_err(|e| format!("fuse create_build_request errno {e}"))?
        };
        let size = req.data.len() as u64;

        // Phase 2 — gateway call. Direct `.await` on the
        // FuseDriver's own gateway handle — NO read lock held, NO
        // `block_in_place` round-trip into KisekiFuse's dedicated
        // runtime. Lets dozens of concurrent puts overlap on the
        // connection pool.
        let resp = self
            .gateway
            .write(kiseki_gateway::ops::WriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data: req.data,
                name: req.name,
                conditional: req.conditional,
                workflow_ref: req.workflow_ref,
                idempotency_key: None,

                forwarded_from_node: None,
                comp_id_override: None,
                tier: None,
                surface: kiseki_gateway::ops::WriteSurface::Fuse,
                base_composition_id: None,
                base_bytes: 0,
            })
            .await
            .map_err(|e| format!("fuse gateway write: {e}"))?;

        // Phase 3 — register the inode under a write lock.
        // Microseconds of contention; gateway is already done.
        let mut fs = self.fs.write().await;
        fs.create_apply_response(1, &name, size, &resp)
            .map_err(|e| format!("fuse create_apply_response errno {e}"))?;

        Ok(Key {
            composition_id: resp.composition_id,
            name: Some(name),
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        // Slow path: synthetic key without a known composition_id —
        // resolve via the FUSE inode-table lookup. Currently unreached
        // in steady-state perf runs (the fast path post-`put` always
        // carries a real composition_id) but kept for backwards compat.
        if key.composition_id.0.is_nil() {
            let name = key
                .name
                .clone()
                .ok_or_else(|| "fuse get: key missing name".to_owned())?;
            let fs = self.fs.read().await;
            let attr = fs
                .lookup(&name)
                .map_err(|e| format!("fuse lookup errno {e}"))?;
            let bytes = fs
                .read(attr.ino, 0, u32::try_from(attr.size).unwrap_or(u32::MAX))
                .map_err(|e| format!("fuse read errno {e}"))?;
            return Ok(bytes.len());
        }

        // Fast path: bypass FUSE — `.await` the gateway directly.
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
            .map_err(|e| format!("fuse gateway read: {e}"))?;
        Ok(resp.data.len())
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
            tier_policy: Vec::new(),

            size_band_pools: kiseki_composition::namespace::NamespaceSizeBandPools::default(),
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
                idempotency_key: None,

                forwarded_from_node: None,
                comp_id_override: None,
                tier: None,
                surface: kiseki_gateway::ops::WriteSurface::Native,
                base_composition_id: None,
                base_bytes: 0,
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
    clients: Vec<
        kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient<
            tonic::transport::Channel,
        >,
    >,
    /// Per-call selector — atomic so workers don't all hash to slot 0.
    next: std::sync::atomic::AtomicUsize,
    namespace_id: NamespaceId,
    tenant_id: OrgId,
}

impl NativeDriver {
    pub async fn new(
        grpc_addr: &str,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Self, String> {
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
            tenant_id,
            namespace_id,
        })
    }

    fn pick_client(
        &self,
    ) -> kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient<
        tonic::transport::Channel,
    > {
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.clients.len();
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
            forwarded_from_node: None,
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
            key: Some(
                kiseki_proto::v1::native::get_object_request::Key::CompositionId(
                    kiseki_proto::v1::CompositionId {
                        value: key.composition_id.0.to_string(),
                    },
                ),
            ),
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

/// Same gateway shape `kiseki-server` runs (fjall-backed
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
/// the listed bits in place a single PUT exercises the same fjall
/// commit + chunk device write + group-commit fsync pipeline the
/// production server pays.
pub struct InProcessPersistentDriver {
    gateway: kiseki_gateway::mem_gateway::InMemoryGateway,
    namespace_id: NamespaceId,
    tenant_id: OrgId,
    /// Tempdir owning the composition keyspace + raw block device.
    /// Drops at the end of the run.
    _data_dir: tempfile::TempDir,
}

impl InProcessPersistentDriver {
    /// Sibling driver constructors are all `async fn` to match the
    /// `Driver` boundary in `build`, which `.await`s every driver's
    /// `new()`. The persistent backend's setup is fully sync (fjall +
    /// tempdir + compositions + spawning a flush task) but we keep
    /// the async signature so the dispatch site stays uniform.
    #[allow(clippy::unused_async)]
    pub async fn new() -> Result<Self, String> {
        use kiseki_common::ids::ShardId;
        use kiseki_common::tenancy::KeyEpoch;
        use kiseki_composition::composition::CompositionStore;
        use kiseki_composition::namespace::Namespace;
        use kiseki_crypto::keys::SystemMasterKey;
        use kiseki_gateway::mem_gateway::InMemoryGateway;

        let data_dir = crate::harness::profile_data_dir("in-process-persistent")?;
        let dir = data_dir.path();

        // 1. Persistent chunk store — same shape as runtime.rs:
        //    raw block device + group-commit fsync (sync_per_write =
        //    false). 4 GiB is plenty for a 30 s run; the spawned
        //    `kiseki-server` uses the same default.
        std::fs::create_dir_all(dir.join("chunks")).map_err(|e| format!("chunks dir: {e}"))?;
        let dev_path = dir.join("chunks").join("data.dev");
        // ADR-022 rev-4: chunk meta is a fjall keyspace (directory).
        let meta_path = dir.join("chunks").join("meta");
        let chunks =
            kiseki_chunk::PersistentChunkStore::init(&dev_path, &meta_path, 4 * 1024 * 1024 * 1024)
                .map_err(|e| format!("chunk store init: {e}"))?;
        chunks.set_sync_per_write(false);
        let chunks_async = kiseki_chunk::arc_async(chunks);

        // 2. Persistent composition store — fjall-backed, with the
        //    same eventual-durability + periodic flush model the
        //    runtime uses by default (KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100).
        std::fs::create_dir_all(dir.join("metadata")).map_err(|e| format!("metadata dir: {e}"))?;
        let comp_path = dir.join("metadata").join("compositions");
        let interval_ms = std::env::var("KISEKI_COMPOSITION_FLUSH_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(100);
        let comp_store_fjall = kiseki_composition::persistent::FjallStorage::open(&comp_path)
            .map_err(|e| format!("composition fjall open: {e}"))?
            .with_eventual_durability(true);
        let flusher = comp_store_fjall.flusher();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if flusher.flush().is_err() {
                    // Profile harness — log-and-continue, not load-bearing.
                    break;
                }
            }
        });
        let comp_storage: Box<dyn kiseki_composition::persistent::CompositionStorage> =
            Box::new(comp_store_fjall);
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
            tier_policy: Vec::new(),

            size_band_pools: kiseki_composition::namespace::NamespaceSizeBandPools::default(),
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
                idempotency_key: None,

                forwarded_from_node: None,
                comp_id_override: None,
                tier: None,
                surface: kiseki_gateway::ops::WriteSurface::Native,
                base_composition_id: None,
                base_bytes: 0,
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

// ---------------------------------------------------------------------------
// Native — TCP-framed-postcard binding (ADR-042 §2.2)
// ---------------------------------------------------------------------------

/// Drives the kiseki-server's TCP-framed-postcard binding through
/// [`kiseki_client::native::TcpFramedClient`]. Same handler as the
/// gRPC `NativeDriver`; different wire — no h2 framing tax,
/// length-prefixed postcard envelopes with `request_id`-multiplexed
/// pipelining.
///
/// Pool shape mirrors `NativeDriver`: N independent TCP-framed
/// clients (one TCP connection each) load-balanced round-robin so
/// per-connection reader-task contention doesn't pin throughput to
/// a single core.
pub struct TcpFramedNativeDriver {
    clients: Vec<Arc<kiseki_client::native::TcpFramedClient>>,
    next: std::sync::atomic::AtomicUsize,
    namespace_id: NamespaceId,
    tenant_id: OrgId,
}

impl TcpFramedNativeDriver {
    pub async fn new(
        addr: &str,
        pool_size: usize,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Self, String> {
        let env_pool: usize = std::env::var("KISEKI_NATIVE_DRIVER_POOL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(pool_size.max(1));
        let mut clients = Vec::with_capacity(env_pool);
        for _ in 0..env_pool {
            let client = kiseki_client::native::TcpFramedClient::connect_plaintext(addr)
                .await
                .map_err(|e| format!("tcp-framed connect: {e}"))?;
            clients.push(client);
        }
        Ok(Self {
            clients,
            next: std::sync::atomic::AtomicUsize::new(0),
            tenant_id,
            namespace_id,
        })
    }

    fn pick(&self) -> Arc<kiseki_client::native::TcpFramedClient> {
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.clients.len();
        Arc::clone(&self.clients[idx])
    }

    fn ctrl(&self) -> kiseki_proto::v1::native::ControlFields {
        kiseki_proto::v1::native::ControlFields {
            tenant_id: Some(kiseki_proto::v1::OrgId {
                value: self.tenant_id.0.to_string(),
            }),
            idempotency_key: uuid::Uuid::new_v4().as_bytes().to_vec(),
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
            forwarded_from_node: None,
        }
    }
}

#[async_trait]
impl Driver for TcpFramedNativeDriver {
    async fn put(&self, payload: &[u8]) -> Result<Key, String> {
        // V3: meta = postcard(PutObjectRequest with empty .data),
        // bulk = the actual payload bytes. The server attaches bulk
        // onto req.data before calling the handler.
        let req = kiseki_proto::v1::native::PutObjectRequest {
            control: Some(self.ctrl()),
            namespace_id: Some(kiseki_proto::v1::NamespaceId {
                value: self.namespace_id.0.to_string(),
            }),
            name: format!("perf-{}", uuid::Uuid::new_v4().simple()),
            data: Vec::new(), // bulk rides separately
        };
        let req_meta =
            postcard::to_allocvec(&req).map_err(|e| format!("tcp-framed put encode: {e}"))?;
        let req_bulk = payload.to_vec();
        let (resp_meta, _resp_bulk) = self
            .pick()
            .call_ok("put_object", req_meta, req_bulk)
            .await
            .map_err(|e| format!("tcp-framed put: {e}"))?;
        let resp: kiseki_proto::v1::native::PutObjectResponse =
            postcard::from_bytes(&resp_meta).map_err(|e| format!("tcp-framed put decode: {e}"))?;
        let comp = resp
            .composition_id
            .ok_or_else(|| "tcp-framed put: response missing composition_id".to_string())?;
        let uuid = uuid::Uuid::parse_str(&comp.value)
            .map_err(|e| format!("tcp-framed put: composition_id parse: {e}"))?;
        Ok(Key {
            composition_id: kiseki_common::ids::CompositionId(uuid),
            name: None,
        })
    }

    async fn get(&self, key: &Key) -> Result<usize, String> {
        // V3: GET request has no bulk; response carries bulk = data.
        // We don't need to postcard-decode the bulk bytes — the
        // length is what the perf driver measures, so just count
        // resp_bulk.len().
        let req = kiseki_proto::v1::native::GetObjectRequest {
            control: Some(self.ctrl()),
            namespace_id: Some(kiseki_proto::v1::NamespaceId {
                value: self.namespace_id.0.to_string(),
            }),
            range_start: 0,
            range_end: 0,
            key: Some(
                kiseki_proto::v1::native::get_object_request::Key::CompositionId(
                    kiseki_proto::v1::CompositionId {
                        value: key.composition_id.0.to_string(),
                    },
                ),
            ),
        };
        let req_meta =
            postcard::to_allocvec(&req).map_err(|e| format!("tcp-framed get encode: {e}"))?;
        let (_resp_meta, resp_bulk) = self
            .pick()
            .call_ok("get_object", req_meta, Vec::new())
            .await
            .map_err(|e| format!("tcp-framed get: {e}"))?;
        // resp_bulk IS the object data. No postcard decode of the
        // bulk bytes — that's the V3 win.
        Ok(resp_bulk.len())
    }
}
