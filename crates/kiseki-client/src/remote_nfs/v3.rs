//! NFSv3 client (RFC 1813) — stateless RPC procedures over TCP.
//!
//! Each GatewayOps call maps to one or more NFSv3 procedures:
//!   write → CREATE + WRITE
//!   read  → LOOKUP + READ
//!   delete → REMOVE

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_gateway::error::GatewayError;
use kiseki_gateway::nfs_xdr::{XdrReader, XdrWriter};
use kiseki_gateway::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};

use super::transport::RpcTransport;

const NFS_PROGRAM: u32 = 100003;
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

/// NFSv3 client. Stateless — each operation is a single RPC.
pub struct Nfs3Client {
    addr: SocketAddr,
    transport: Mutex<Option<RpcTransport>>,
    /// Root file handle — obtained from FSINFO or MOUNT protocol.
    /// Kiseki's FSINFO returns the root handle in `post_op_attr`.
    root_fh: Mutex<Option<Vec<u8>>>,
    /// Client-side multipart upload buffers keyed by upload ID.
    /// Each value is a list of (part_number, data) pairs assembled
    /// into a single CREATE+WRITE on `complete_multipart`.
    multipart_buffers: Mutex<HashMap<String, Vec<(u32, Vec<u8>)>>>,
}

impl Nfs3Client {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            transport: Mutex::new(None),
            root_fh: Mutex::new(None),
            multipart_buffers: Mutex::new(HashMap::new()),
        }
    }

    fn ensure_transport(&self) -> Result<std::sync::MutexGuard<'_, Option<RpcTransport>>, GatewayError> {
        let mut guard = self.transport.lock().map_err(|e| {
            GatewayError::ProtocolError(format!("lock: {e}"))
        })?;
        if guard.is_none() {
            *guard = Some(RpcTransport::connect(self.addr)?);
        }
        Ok(guard)
    }

    fn ensure_root_fh(&self) -> Result<Vec<u8>, GatewayError> {
        {
            let fh = self.root_fh.lock().map_err(|e| {
                GatewayError::ProtocolError(format!("lock: {e}"))
            })?;
            if let Some(ref h) = *fh {
                return Ok(h.clone());
            }
        }
        // Get root handle via FSINFO with a synthetic root handle.
        // Kiseki's NFSv3 uses a well-known root handle format.
        let mut guard = self.ensure_transport()?;
        let t = guard.as_mut().unwrap();

        // NULL first — verify server is alive
        let _ = t.call(NFS_PROGRAM, NFS3_VERSION, NFSPROC3_NULL, &[])?;

        // FSINFO with empty handle to get root
        let mut args = XdrWriter::new();
        // The root handle is a 16-byte handle that Kiseki recognizes.
        // Use the bootstrap namespace/tenant UUID format.
        let bootstrap_handle = vec![0u8; 16]; // Kiseki maps all-zero to root
        args.write_opaque(&bootstrap_handle);
        let reply = t.call(NFS_PROGRAM, NFS3_VERSION, NFSPROC3_FSINFO, &args.into_bytes())?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(xdr_err)?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "FSINFO failed: status={status}"
            )));
        }
        // post_op_attr follows — we need the handle from it.
        // For now, store the bootstrap handle as root.
        let mut fh = self.root_fh.lock().map_err(|e| {
            GatewayError::ProtocolError(format!("lock: {e}"))
        })?;
        *fh = Some(bootstrap_handle.clone());
        Ok(bootstrap_handle)
    }
}

fn xdr_err(e: std::io::Error) -> GatewayError {
    GatewayError::ProtocolError(format!("XDR: {e}"))
}

#[async_trait::async_trait]
impl GatewayOps for Nfs3Client {
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        let root_fh = self.ensure_root_fh()?;
        let mut guard = self.ensure_transport()?;
        let t = guard.as_mut().unwrap();

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
        let reply = t.call(NFS_PROGRAM, NFS3_VERSION, NFSPROC3_CREATE, &args.into_bytes())?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(xdr_err)?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 CREATE failed: status={status}"
            )));
        }
        // post_op_fh3: follows(bool) + handle
        let has_handle = r.read_u32().map_err(xdr_err)?;
        let file_fh = if has_handle != 0 {
            r.read_opaque().map_err(xdr_err)?
        } else {
            return Err(GatewayError::ProtocolError("CREATE returned no handle".into()));
        };

        // WRITE
        let mut args = XdrWriter::new();
        args.write_opaque(&file_fh);
        args.write_u64(0); // offset
        args.write_u32(req.data.len() as u32); // count
        args.write_u32(2); // stable = FILE_SYNC
        args.write_opaque(&req.data);
        let reply = t.call(NFS_PROGRAM, NFS3_VERSION, NFSPROC3_WRITE, &args.into_bytes())?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(xdr_err)?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 WRITE failed: status={status}"
            )));
        }

        let composition_id = CompositionId(
            uuid::Uuid::parse_str(&filename).unwrap_or_else(|_| uuid::Uuid::new_v4()),
        );

        Ok(WriteResponse {
            composition_id,
            bytes_written: req.data.len() as u64,
        })
    }

    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        let root_fh = self.ensure_root_fh()?;
        let mut guard = self.ensure_transport()?;
        let t = guard.as_mut().unwrap();

        // LOOKUP to get the file handle
        let mut args = XdrWriter::new();
        args.write_opaque(&root_fh);
        args.write_string(&req.composition_id.0.to_string());
        let reply = t.call(NFS_PROGRAM, NFS3_VERSION, NFSPROC3_LOOKUP, &args.into_bytes())?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(xdr_err)?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 LOOKUP failed: status={status}"
            )));
        }
        let file_fh = r.read_opaque().map_err(xdr_err)?;

        // READ
        let mut args = XdrWriter::new();
        args.write_opaque(&file_fh);
        args.write_u64(req.offset);
        args.write_u32(req.length.min(u32::MAX as u64) as u32);
        let reply = t.call(NFS_PROGRAM, NFS3_VERSION, NFSPROC3_READ, &args.into_bytes())?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(xdr_err)?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 READ failed: status={status}"
            )));
        }
        // post_op_attr
        let has_attr = r.read_u32().map_err(xdr_err)?;
        if has_attr != 0 {
            // Skip fattr3 (84 bytes)
            for _ in 0..21 {
                let _ = r.read_u32().map_err(xdr_err)?;
            }
        }
        let count = r.read_u32().map_err(xdr_err)?;
        let eof = r.read_u32().map_err(xdr_err)? != 0;
        let data = r.read_opaque().map_err(xdr_err)?;

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
        let root_fh = self.ensure_root_fh()?;
        let mut guard = self.ensure_transport()?;
        let t = guard.as_mut().unwrap();

        let mut args = XdrWriter::new();
        args.write_opaque(&root_fh);
        args.write_string(&composition_id.0.to_string());
        let reply = t.call(NFS_PROGRAM, NFS3_VERSION, NFSPROC3_REMOVE, &args.into_bytes())?;

        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(xdr_err)?;
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

    async fn complete_multipart(&self, upload_id: &str) -> Result<CompositionId, GatewayError> {
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
