//! `NFSv3` client (RFC 1813) — stateless RPC procedures over TCP.
//!
//! Each `GatewayOps` call maps to one or more `NFSv3` procedures:
//!   write → CREATE + WRITE
//!   read  → LOOKUP + READ
//!   delete → REMOVE

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_gateway::error::GatewayError;
use kiseki_gateway::nfs_xdr::{XdrReader, XdrWriter};
use kiseki_gateway::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};

use super::transport::RpcTransport;

type MultipartBuffer = std::sync::Mutex<HashMap<String, Vec<(u32, Vec<u8>)>>>;

const NFS_PROGRAM: u32 = 100_003;
const NFS3_VERSION: u32 = 3;

// NFSv3 procedures (RFC 1813 §3)
const NFSPROC3_NULL: u32 = 0;
const NFSPROC3_LOOKUP: u32 = 3;
const NFSPROC3_READ: u32 = 6;
const NFSPROC3_WRITE: u32 = 7;
const NFSPROC3_CREATE: u32 = 8;
const NFSPROC3_REMOVE: u32 = 12;
const NFSPROC3_FSINFO: u32 = 19;

const NFS3_OK: u32 = 0;

/// `NFSv3` client. Stateless — each operation is a single RPC.
pub struct Nfs3Client {
    addr: SocketAddr,
    /// Pool of independent TCP transports. Each slot is an
    /// `AsyncMutex<Option<RpcTransport>>` lazily connected on first
    /// use. Calls pick a slot via the `next` round-robin counter,
    /// so N concurrent operations use N different connections —
    /// throughput scales with `pool_size`. `NFSv3` is wire-stateless,
    /// so any slot can serve any request.
    ///
    /// `tokio::sync::Mutex` (not `std::sync::Mutex`) — same reason
    /// as `Nfs4Client::sessions`: holds across blocking sync TCP IO
    /// inside an `async fn`.
    transports: Vec<AsyncMutex<Option<RpcTransport>>>,
    /// Round-robin slot selector.
    next: AtomicUsize,
    /// Root file handle — obtained from FSINFO or MOUNT protocol.
    /// Kiseki's FSINFO returns the root handle in `post_op_attr`.
    /// Held only across cheap clones, not network IO, so `std::Mutex`
    /// is fine here.
    root_fh: Mutex<Option<Vec<u8>>>,
    /// Client-side multipart upload buffers keyed by upload ID.
    /// Each value is a list of (`part_number`, data) pairs assembled
    /// into a single CREATE+WRITE on `complete_multipart`.
    multipart_buffers: MultipartBuffer,
}

impl Nfs3Client {
    /// Create a `NFSv3` client with a single connection (= prior
    /// behavior). Use [`Self::with_pool`] for concurrent workloads.
    #[must_use]
    pub fn new(addr: SocketAddr) -> Self {
        Self::with_pool(addr, 1)
    }

    /// Create a `NFSv3` client with `pool_size` independent TCP
    /// connections. Throughput scales linearly until either the
    /// server or the wire becomes the bottleneck.
    #[must_use]
    pub fn with_pool(addr: SocketAddr, pool_size: usize) -> Self {
        let pool_size = pool_size.max(1);
        let mut transports = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            transports.push(AsyncMutex::new(None));
        }
        Self {
            addr,
            transports,
            next: AtomicUsize::new(0),
            root_fh: Mutex::new(None),
            multipart_buffers: Mutex::new(HashMap::new()),
        }
    }

    async fn ensure_transport(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<RpcTransport>>, GatewayError> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.transports.len();
        let mut guard = self.transports[idx].lock().await;
        if guard.is_none() {
            *guard = Some(RpcTransport::connect(self.addr)?);
        }
        Ok(guard)
    }

    async fn ensure_root_fh(&self) -> Result<Vec<u8>, GatewayError> {
        {
            let fh = self
                .root_fh
                .lock()
                .map_err(|e| GatewayError::ProtocolError(format!("lock: {e}")))?;
            if let Some(ref h) = *fh {
                return Ok(h.clone());
            }
        }
        // Get root handle via FSINFO with a synthetic root handle.
        // Kiseki's NFSv3 uses a well-known root handle format.
        let mut guard = self.ensure_transport().await?;
        let t = guard
            .as_mut()
            .expect("transport not initialized — call connect() first");

        // NULL first — verify server is alive
        let _ = t.call(NFS_PROGRAM, NFS3_VERSION, NFSPROC3_NULL, &[])?;

        // FSINFO with empty handle to get root
        let mut args = XdrWriter::new();
        // The root handle is a 16-byte handle that Kiseki recognizes.
        // Use the bootstrap namespace/tenant UUID format.
        let bootstrap_handle = vec![0u8; 16]; // Kiseki maps all-zero to root
        args.write_opaque(&bootstrap_handle);
        let reply = t.call(
            NFS_PROGRAM,
            NFS3_VERSION,
            NFSPROC3_FSINFO,
            &args.into_bytes(),
        )?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(|e| xdr_err(&e))?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "FSINFO failed: status={status}"
            )));
        }
        // post_op_attr follows — we need the handle from it.
        // For now, store the bootstrap handle as root.
        let mut fh = self
            .root_fh
            .lock()
            .map_err(|e| GatewayError::ProtocolError(format!("lock: {e}")))?;
        *fh = Some(bootstrap_handle.clone());
        Ok(bootstrap_handle)
    }
}

fn xdr_err(e: &std::io::Error) -> GatewayError {
    GatewayError::ProtocolError(format!("XDR: {e}"))
}

#[async_trait::async_trait]
impl GatewayOps for Nfs3Client {
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        let root_fh = self.ensure_root_fh().await?;
        let mut guard = self.ensure_transport().await?;
        let t = guard
            .as_mut()
            .expect("transport not initialized — call connect() first");

        let filename = uuid::Uuid::new_v4().to_string();

        // CREATE — mode UNCHECKED (0)
        let mut args = XdrWriter::new();
        args.write_opaque(&root_fh); // dir handle
        args.write_string(&filename); // name
        args.write_u32(0); // createmode = UNCHECKED
                           // sattr3 (all unset)
        for _ in 0..6 {
            args.write_u32(0); // mode, uid, gid, size, atime, mtime — all "don't set"
        }
        let reply = t.call(
            NFS_PROGRAM,
            NFS3_VERSION,
            NFSPROC3_CREATE,
            &args.into_bytes(),
        )?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(|e| xdr_err(&e))?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 CREATE failed: status={status}"
            )));
        }
        // post_op_fh3: follows(bool) + handle
        let has_handle = r.read_u32().map_err(|e| xdr_err(&e))?;
        let file_fh = if has_handle != 0 {
            r.read_opaque().map_err(|e| xdr_err(&e))?
        } else {
            return Err(GatewayError::ProtocolError(
                "CREATE returned no handle".into(),
            ));
        };

        // WRITE
        let mut args = XdrWriter::new();
        args.write_opaque(&file_fh);
        args.write_u64(0); // offset
        args.write_u32(req.data.len() as u32); // count
        args.write_u32(2); // stable = FILE_SYNC
        args.write_opaque(&req.data);
        let reply = t.call(
            NFS_PROGRAM,
            NFS3_VERSION,
            NFSPROC3_WRITE,
            &args.into_bytes(),
        )?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(|e| xdr_err(&e))?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 WRITE failed: status={status}"
            )));
        }

        // Re-LOOKUP to discover the server-assigned composition_id.
        // Under FILE_SYNC the server flushes the buffered write into a
        // fresh composition (kiseki compositions are write-once-immutable
        // — see nfs3_server::reply_write) and re-maps the directory
        // entry to the new file handle. The new fh's first 16 bytes
        // ARE the new composition's UUID, so we can decode it directly
        // without a second round trip... except we need the new fh,
        // which means a LOOKUP. One extra RPC for the canonical id.
        let mut args = XdrWriter::new();
        args.write_opaque(&root_fh);
        args.write_string(&filename);
        let reply = t.call(
            NFS_PROGRAM,
            NFS3_VERSION,
            NFSPROC3_LOOKUP,
            &args.into_bytes(),
        )?;
        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(|e| xdr_err(&e))?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 LOOKUP-after-WRITE failed: status={status}"
            )));
        }
        let new_fh = r.read_opaque().map_err(|e| xdr_err(&e))?;
        let composition_id = if new_fh.len() >= 16 {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&new_fh[..16]);
            CompositionId(uuid::Uuid::from_bytes(bytes))
        } else {
            // Defensive fallback — server should always return a
            // 32-byte handle, but if it doesn't, parse the filename.
            CompositionId(uuid::Uuid::parse_str(&filename).unwrap_or_else(|_| uuid::Uuid::new_v4()))
        };

        Ok(WriteResponse {
            composition_id,
            bytes_written: req.data.len() as u64,
        })
    }

    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        let root_fh = self.ensure_root_fh().await?;
        let mut guard = self.ensure_transport().await?;
        let t = guard
            .as_mut()
            .expect("transport not initialized — call connect() first");

        // LOOKUP to get the file handle
        let mut args = XdrWriter::new();
        args.write_opaque(&root_fh);
        args.write_string(&req.composition_id.0.to_string());
        let reply = t.call(
            NFS_PROGRAM,
            NFS3_VERSION,
            NFSPROC3_LOOKUP,
            &args.into_bytes(),
        )?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(|e| xdr_err(&e))?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 LOOKUP failed: status={status}"
            )));
        }
        let file_fh = r.read_opaque().map_err(|e| xdr_err(&e))?;

        // READ
        let mut args = XdrWriter::new();
        args.write_opaque(&file_fh);
        args.write_u64(req.offset);
        args.write_u32(u32::try_from(req.length).unwrap_or(u32::MAX));
        let reply = t.call(NFS_PROGRAM, NFS3_VERSION, NFSPROC3_READ, &args.into_bytes())?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(|e| xdr_err(&e))?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 READ failed: status={status}"
            )));
        }
        // post_op_attr
        let has_attr = r.read_u32().map_err(|e| xdr_err(&e))?;
        if has_attr != 0 {
            // Skip fattr3 (84 bytes)
            for _ in 0..21 {
                let _ = r.read_u32().map_err(|e| xdr_err(&e))?;
            }
        }
        let _count = r.read_u32().map_err(|e| xdr_err(&e))?;
        let eof = r.read_u32().map_err(|e| xdr_err(&e))? != 0;
        let data = r.read_opaque().map_err(|e| xdr_err(&e))?;

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
        Ok(Vec::new()) // READDIRPLUS is complex; S3 list is primary
    }

    async fn delete(
        &self,
        _tenant_id: OrgId,
        _namespace_id: NamespaceId,
        composition_id: CompositionId,
    ) -> Result<(), GatewayError> {
        let root_fh = self.ensure_root_fh().await?;
        let mut guard = self.ensure_transport().await?;
        let t = guard
            .as_mut()
            .expect("transport not initialized — call connect() first");

        let mut args = XdrWriter::new();
        args.write_opaque(&root_fh);
        args.write_string(&composition_id.0.to_string());
        let reply = t.call(
            NFS_PROGRAM,
            NFS3_VERSION,
            NFSPROC3_REMOVE,
            &args.into_bytes(),
        )?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(|e| xdr_err(&e))?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 REMOVE failed: status={status}"
            )));
        }
        Ok(())
    }

    // -- Multipart: client-side buffering, single CREATE+WRITE on complete --

    async fn start_multipart(&self, _namespace_id: NamespaceId) -> Result<String, GatewayError> {
        let upload_id = uuid::Uuid::new_v4().to_string();
        self.multipart_buffers
            .lock()
            .map_err(|e| GatewayError::ProtocolError(format!("lock: {e}")))?
            .insert(upload_id.clone(), Vec::new());
        Ok(upload_id)
    }

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
        // Return a synthetic ETag derived from part number.
        Ok(format!("nfs3-part-{part_number}"))
    }

    async fn complete_multipart(
        &self,
        upload_id: &str,
        _name: Option<&str>,
    ) -> Result<CompositionId, GatewayError> {
        let mut parts = self
            .multipart_buffers
            .lock()
            .map_err(|e| GatewayError::ProtocolError(format!("lock: {e}")))?
            .remove(upload_id)
            .ok_or_else(|| {
                GatewayError::ProtocolError(format!("unknown upload_id: {upload_id}"))
            })?;
        // Sort by part number and concatenate.
        parts.sort_by_key(|(n, _)| *n);
        let full_data: Vec<u8> = parts.into_iter().flat_map(|(_, d)| d).collect();

        // Delegate to the normal write path (CREATE + WRITE).
        let resp = self
            .write(WriteRequest {
                tenant_id: OrgId(uuid::Uuid::nil()),
                namespace_id: NamespaceId(uuid::Uuid::nil()),
                data: full_data,
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await?;
        Ok(resp.composition_id)
    }

    async fn abort_multipart(&self, upload_id: &str) -> Result<(), GatewayError> {
        self.multipart_buffers
            .lock()
            .map_err(|e| GatewayError::ProtocolError(format!("lock: {e}")))?
            .remove(upload_id);
        Ok(())
    }

    // -- No-ops for NFSv3 --

    async fn set_object_content_type(
        &self,
        _composition_id: CompositionId,
        _content_type: Option<String>,
    ) -> Result<(), GatewayError> {
        Ok(()) // NFSv3 has no per-object Content-Type metadata.
    }

    async fn ensure_namespace(
        &self,
        _tenant_id: OrgId,
        _namespace_id: NamespaceId,
    ) -> Result<(), GatewayError> {
        Ok(()) // NFSv3 namespaces are implicit (directory tree).
    }
}
