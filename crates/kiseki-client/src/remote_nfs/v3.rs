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
const NFSPROC3_COMMIT: u32 = 21;
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
    #[allow(clippy::too_many_lines)] // CREATE+WRITE+COMMIT+LOOKUP wire choreography, single-purpose
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

        // COMMIT (RFC 1813 §3.3.21) — required after WRITE so the
        // server's per-fh buffer is flushed and the placeholder
        // composition is materialized. Without this, a follow-up GET
        // (NFS or cross-protocol via S3) on the returned
        // composition_id sees 404 until the next periodic flush.
        //
        // Real Linux NFS clients issue COMMIT before close /
        // cross-mount visibility for the same reason. PR #50 removed
        // inline flush on `stable >= 1` to prevent F-1
        // (per-WRITE Raft hydrator saturation under `fio --direct=1`),
        // so COMMIT is now the only flush trigger.
        let mut args = XdrWriter::new();
        args.write_opaque(&file_fh);
        args.write_u64(0); // offset
        args.write_u32(0); // count = 0 → flush all
        let reply = t.call(
            NFS_PROGRAM,
            NFS3_VERSION,
            NFSPROC3_COMMIT,
            &args.into_bytes(),
        )?;
        let mut r = XdrReader::new(&reply);
        let status = r.read_u32().map_err(|e| xdr_err(&e))?;
        if status != NFS3_OK {
            return Err(GatewayError::ProtocolError(format!(
                "NFSv3 COMMIT failed: status={status}"
            )));
        }

        // Post-COMMIT LOOKUP to recover the **real** composition_id.
        //
        // The CREATE-returned fh's first 16 bytes are the *placeholder*
        // UUID minted by `nfs_ops::create_pending_named`. After
        // COMMIT triggers `flush_writes`, the server's `HandleRegistry`
        // repoints the placeholder fh's `HandleEntry` to the real
        // composition_id, but the original fh BYTES are unchanged
        // (the placeholder lives in `fh[..16]` forever). Same-handle
        // NFS reads still work via the registry indirection, but
        // **cross-protocol GET-by-UUID** (S3 / native) needs the
        // real composition_id since they bypass the fh registry and
        // look the composition up directly in the composition store.
        //
        // The fix is one extra LOOKUP RPC against the parent directory
        // for the filename used at CREATE. `flush_writes` updates
        // `dir_index` (`nfs_ops::flush_writes` §"Update dir_index to
        // point at the latest composition") so the post-COMMIT
        // LOOKUP returns a fresh fh whose first 16 bytes ARE the
        // real composition_id. Cost: 1 extra RPC per PUT (3 → 4) on
        // a path that's traditionally followed by a GET anyway —
        // bringing us back to the natural NFS round-trip count and
        // making cross-protocol PUT-then-GET correct (#127 follow-up).
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
        let lookup_status = r.read_u32().map_err(|e| xdr_err(&e))?;
        let composition_id = if lookup_status == NFS3_OK {
            let fh_real = r.read_opaque().map_err(|e| xdr_err(&e))?;
            if fh_real.len() >= 16 {
                let mut bytes = [0u8; 16];
                bytes.copy_from_slice(&fh_real[..16]);
                CompositionId(uuid::Uuid::from_bytes(bytes))
            } else {
                // Defensive: malformed reply → fall back to the
                // placeholder. Cross-protocol GET will still 404 in
                // this rare case, but the NFS round-trip stays
                // useful so the caller can at least retry.
                let mut bytes = [0u8; 16];
                bytes.copy_from_slice(&file_fh[..16]);
                CompositionId(uuid::Uuid::from_bytes(bytes))
            }
        } else if file_fh.len() >= 16 {
            // LOOKUP NOENT or any other failure — fall back to the
            // placeholder so single-protocol clients (NFS-only) still
            // behave as before. Cross-protocol callers see the
            // pre-fix behavior in that degenerate case.
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&file_fh[..16]);
            CompositionId(uuid::Uuid::from_bytes(bytes))
        } else {
            CompositionId(uuid::Uuid::parse_str(&filename).unwrap_or_else(|_| uuid::Uuid::new_v4()))
        };

        Ok(WriteResponse {
            composition_id,
            bytes_written: req.data.len() as u64,
        })
    }

    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        // The kiseki NFSv3 server's file-handle layout is
        // `[composition_id 16][zeros 16]` (see
        // `nfs_ops::HandleRegistry::file_handle`). The fh registry
        // is populated when the composition is created and the
        // pending-fh path repoints the original CREATE fh to the
        // real composition during `flush_writes`. Either way, a
        // freshly-constructed `[composition_id 16][zeros 16]` fh
        // resolves to the right composition server-side — no
        // LOOKUP RPC required, no client-side fh cache.
        let mut fh = vec![0u8; 32];
        fh[..16].copy_from_slice(req.composition_id.0.as_bytes());

        let mut guard = self.ensure_transport().await?;
        let t = guard
            .as_mut()
            .expect("transport not initialized — call connect() first");
        read_with_fh(t, &fh, &req)
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
                idempotency_key: None,

                forwarded_from_node: None,
                comp_id_override: None,
                tier: None,
                surface: kiseki_gateway::ops::WriteSurface::Nfs,
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

/// Issue an `NFSv3` READ for `fh` against `req.offset`/`req.length`.
/// Shared between the cached-handle fast path and the `LOOKUP+READ`
/// slow path. The transport guard is held by the caller — this
/// helper just drives the wire format and parses the reply.
fn read_with_fh(
    t: &mut RpcTransport,
    fh: &[u8],
    req: &ReadRequest,
) -> Result<ReadResponse, GatewayError> {
    let mut args = XdrWriter::new();
    args.write_opaque(fh);
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

#[cfg(test)]
mod tests {
    //! Server-free contract tests for the `NFSv3` client adapter.
    //!
    //! These cover only the paths that never touch the network — the
    //! in-memory multipart buffer state machine and the explicit
    //! no-op `GatewayOps` stubs (`list`, `set_object_content_type`,
    //! `ensure_namespace`). The wire-level paths (`read`, `write`,
    //! `delete`) need a running gateway and are exercised by the
    //! e2e Python suite + BDD acceptance crate.
    use super::*;
    use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
    use std::net::SocketAddr;
    use std::str::FromStr;

    fn client() -> Nfs3Client {
        // Unbound port — these tests never actually connect.
        Nfs3Client::new(SocketAddr::from_str("127.0.0.1:0").expect("addr"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_returns_empty_until_readdirplus_is_wired() {
        // NFSv3 client deliberately does NOT implement READDIRPLUS (RFC
        // 1813 §3.3.17) — S3 LIST is the canonical listing path. The
        // contract is "always Ok(empty)" so callers that ask both
        // paths see consistent semantics rather than a Protocol error.
        let c = client();
        let got = c
            .list(OrgId(uuid::Uuid::nil()), NamespaceId(uuid::Uuid::nil()))
            .await
            .expect("list must succeed as a no-op");
        assert!(
            got.is_empty(),
            "NFSv3 list is intentionally empty; got {} entries",
            got.len()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_object_content_type_is_a_noop_returning_ok() {
        // NFSv3 has no per-file Content-Type metadata. Callers that
        // unconditionally set one (S3 PUT translation) must not error
        // out — the contract is silent acceptance.
        let c = client();
        c.set_object_content_type(
            CompositionId(uuid::Uuid::from_u128(7)),
            Some("application/octet-stream".into()),
        )
        .await
        .expect("set_object_content_type must succeed");
        c.set_object_content_type(CompositionId(uuid::Uuid::from_u128(8)), None)
            .await
            .expect("set_object_content_type(None) must succeed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_namespace_is_a_noop_returning_ok() {
        // NFSv3's directory tree is implicit; namespaces aren't
        // declared via a protocol op. The stub MUST return Ok so the
        // generic mount path doesn't degrade to an error.
        let c = client();
        c.ensure_namespace(OrgId(uuid::Uuid::nil()), NamespaceId(uuid::Uuid::nil()))
            .await
            .expect("ensure_namespace must succeed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multipart_buffer_lifecycle_start_upload_abort() {
        // Cover the buffer-only side of S3-style multipart upload.
        // After abort_multipart, the upload_id must be forgotten so
        // a later upload_part against the same id is a hard error
        // (otherwise we'd silently leak buffers).
        let c = client();
        let uid = c
            .start_multipart(NamespaceId(uuid::Uuid::nil()))
            .await
            .expect("start_multipart");
        assert!(!uid.is_empty(), "upload id must be non-empty");

        let etag = c.upload_part(&uid, 1, b"hello").await.expect("upload_part");
        // ETag shape per impl — synthetic, derived from part number.
        assert_eq!(etag, "nfs3-part-1");

        c.abort_multipart(&uid).await.expect("abort_multipart");

        let err = c
            .upload_part(&uid, 2, b"world")
            .await
            .expect_err("upload_part after abort must fail");
        // The buffer must be gone, not silently re-created.
        match err {
            GatewayError::ProtocolError(m) => {
                assert!(
                    m.contains("unknown upload_id"),
                    "abort+upload error must say 'unknown upload_id': {m}"
                );
            }
            other => panic!("expected ProtocolError; got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upload_part_on_unknown_upload_id_fails() {
        let c = client();
        let err = c
            .upload_part("does-not-exist", 1, b"x")
            .await
            .expect_err("unknown upload_id must error");
        assert!(matches!(err, GatewayError::ProtocolError(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn complete_multipart_on_unknown_upload_id_fails() {
        // complete_multipart's first step is buffer lookup — that
        // happens before any wire op, so this test stays server-free.
        let c = client();
        let err = c
            .complete_multipart("does-not-exist", None)
            .await
            .expect_err("unknown upload_id must error before reaching the wire");
        assert!(matches!(err, GatewayError::ProtocolError(_)));
    }
}
