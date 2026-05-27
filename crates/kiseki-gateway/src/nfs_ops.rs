//! Shared NFS operations — used by both NFSv3 and NFSv4.2 dispatchers.
//!
//! Maps NFS file handles to compositions, provides stat/readdir, and
//! delegates read/write to `NfsGateway<GatewayOps>`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

use crate::error::GatewayError;
use crate::nfs::{NfsGateway, NfsReadRequest, NfsReadResponse, NfsWriteRequest, NfsWriteResponse};
use crate::nfs4_server::SessionManager;
use crate::nfs_dir::DirectoryIndex;
use crate::nfs_lock::LockManager;
use crate::ops::GatewayOps;
use kiseki_common::locks::LockOrDie;

/// NFS file handle — 32-byte opaque identifier.
pub type FileHandle = [u8; 32];

/// File type for NFS attributes.
///
/// Maps to NFSv3 ftype3 / NFSv4 nfs_ftype4 wire encodings:
///   Regular   → NF3REG (1) / NF4REG (1)
///   Directory → NF3DIR (2) / NF4DIR (2)
///   Symlink   → NF3LNK (5) / NF4LNK (5)
///
/// The Symlink variant (#53) lets `getattr` report the correct
/// type for fhs registered as `HandleEntry::Symlink`. Without it,
/// `ln -s` over a kernel NFSv4 mount creates the link entry but
/// the kernel sees the file as a regular file and calls READ on
/// it instead of READLINK, returning the raw target-path bytes
/// (Group V #1 saved us from outright EIO but the link still
/// doesn't behave like a symlink to userspace tools).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
}

/// NFS file attributes (subset shared by v3 and v4).
#[derive(Debug, Clone)]
pub struct NfsAttrs {
    pub file_type: FileType,
    pub size: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub fileid: u64,
}

/// File handle registry — maps handles to namespace/composition IDs.
pub struct HandleRegistry {
    handles: Mutex<HashMap<FileHandle, HandleEntry>>,
    root_handles: Mutex<HashMap<NamespaceId, FileHandle>>,
}

#[derive(Clone, Debug)]
enum HandleEntry {
    /// Pseudo-root: the empty parent directory the kernel sees from
    /// `PUTROOTFH`. Has a single virtual child entry pointing at the
    /// namespace root via `LOOKUP("default")`. Distinct fileid from
    /// the namespace root so the kernel doesn't see a path-resolution
    /// loop (Phase 15c.2 mount fix).
    PseudoRoot,
    Root {
        namespace_id: NamespaceId,
        tenant_id: OrgId,
    },
    /// Subdirectory created via `mkdir` (NFSv3 MKDIR / NFSv4 CREATE
    /// type=NF4DIR). Behaves like `Root` for handle-lookup purposes
    /// (no composition_id, namespace+tenant ownership) but is
    /// distinct so `getattr` reports `FileType::Directory` and
    /// readdir routes correctly. Pre-2026-05-07 the mkdir path
    /// minted the 32-byte handle (`0xFE` marker byte) but never
    /// registered it in `HandleRegistry::handles`, so every
    /// follow-up op (kernel does GETATTR on the freshly-created
    /// handle to verify) returned `NFS3ERR_BADHANDLE` → errno 521.
    Directory {
        namespace_id: NamespaceId,
        tenant_id: OrgId,
    },
    File {
        namespace_id: NamespaceId,
        tenant_id: OrgId,
        composition_id: CompositionId,
    },
    /// Symbolic link — composition_id points at storage holding the
    /// target path bytes. Distinct from `File` so READLINK can reject
    /// non-symlink handles (RFC 1813 §3.3.5 + RFC 7530 §16.11.6 both
    /// mandate NFS3ERR_INVAL / NFS4ERR_INVAL for READLINK on a
    /// non-symlink target). Pre-2026-05-15 symlinks were stored as
    /// `File`, so READLINK on any composition handle succeeded with
    /// the file's raw contents — silent type-confusion.
    Symlink {
        namespace_id: NamespaceId,
        tenant_id: OrgId,
        composition_id: CompositionId,
    },
}

impl HandleRegistry {
    pub fn new() -> Self {
        Self {
            handles: Mutex::new(HashMap::new()),
            root_handles: Mutex::new(HashMap::new()),
        }
    }

    /// The pseudo-root file handle — what `PUTROOTFH` returns. This
    /// is a stable 32-byte sentinel distinct from any namespace root,
    /// so the kernel sees a different fileid for "/" vs "/default"
    /// (Phase 15c.2: avoids `mount(2): Too many levels of symbolic
    /// links` from kernel loop detection).
    pub fn pseudo_root_handle(&self) -> FileHandle {
        // 0xFD-stuffed handle ⇒ fileid bytes differ from any
        // namespace_id-derived root_handle. Trailing 0xFD also
        // marks "pseudo-root" for the dispatcher.
        let mut fh = [0xFDu8; 32];
        fh[31] = 0xFD;
        // Idempotent insert.
        self.handles
            .lock()
            .lock_or_die("nfs_ops.unknown")
            .entry(fh)
            .or_insert(HandleEntry::PseudoRoot);
        fh
    }

    /// Whether this handle is the pseudo-root (the parent of the
    /// namespace alias). Distinguished from `is_root()` (namespace
    /// root) so dispatch logic can route LOOKUP("default") correctly.
    pub fn is_pseudo_root(&self, fh: &FileHandle) -> bool {
        let handles = self.handles.lock().lock_or_die("nfs_ops.unknown");
        matches!(handles.get(fh), Some(HandleEntry::PseudoRoot))
    }

    /// Get or create a root directory handle for a namespace.
    pub fn root_handle(&self, namespace_id: NamespaceId, tenant_id: OrgId) -> FileHandle {
        let mut roots = self.root_handles.lock().lock_or_die("nfs_ops.unknown");
        if let Some(&fh) = roots.get(&namespace_id) {
            return fh;
        }
        let mut fh = [0u8; 32];
        fh[..16].copy_from_slice(namespace_id.0.as_bytes());
        fh[16] = 0xFF; // marker for root handle

        roots.insert(namespace_id, fh);
        self.handles.lock().lock_or_die("nfs_ops.unknown").insert(
            fh,
            HandleEntry::Root {
                namespace_id,
                tenant_id,
            },
        );
        fh
    }

    /// Create a file handle for a composition.
    pub fn file_handle(
        &self,
        namespace_id: NamespaceId,
        tenant_id: OrgId,
        composition_id: CompositionId,
    ) -> FileHandle {
        let mut fh = [0u8; 32];
        fh[..16].copy_from_slice(composition_id.0.as_bytes());
        self.handles.lock().lock_or_die("nfs_ops.unknown").insert(
            fh,
            HandleEntry::File {
                namespace_id,
                tenant_id,
                composition_id,
            },
        );
        fh
    }

    /// Look up a handle. Returns `None` if not found.
    pub fn lookup(&self, fh: &FileHandle) -> Option<(NamespaceId, OrgId, Option<CompositionId>)> {
        let handles = self.handles.lock().lock_or_die("nfs_ops.unknown");
        handles.get(fh).and_then(|entry| match entry {
            HandleEntry::PseudoRoot => None, // pseudo-root has no namespace
            HandleEntry::Root {
                namespace_id,
                tenant_id,
            }
            | HandleEntry::Directory {
                namespace_id,
                tenant_id,
            } => Some((*namespace_id, *tenant_id, None)),
            HandleEntry::File {
                namespace_id,
                tenant_id,
                composition_id,
            }
            | HandleEntry::Symlink {
                namespace_id,
                tenant_id,
                composition_id,
            } => Some((*namespace_id, *tenant_id, Some(*composition_id))),
        })
    }

    /// Register a fh as a symbolic link. Mirrors `file_handle` but
    /// the entry is `Symlink`, so `readlink` accepts it and other
    /// callers can distinguish via `is_symlink`.
    pub fn symlink_handle(
        &self,
        namespace_id: NamespaceId,
        tenant_id: OrgId,
        composition_id: CompositionId,
    ) -> FileHandle {
        let mut fh = [0u8; 32];
        fh[..16].copy_from_slice(composition_id.0.as_bytes());
        self.handles.lock().lock_or_die("nfs_ops.unknown").insert(
            fh,
            HandleEntry::Symlink {
                namespace_id,
                tenant_id,
                composition_id,
            },
        );
        fh
    }

    /// Is this handle a symbolic link?
    pub fn is_symlink(&self, fh: &FileHandle) -> bool {
        let handles = self.handles.lock().lock_or_die("nfs_ops.unknown");
        matches!(handles.get(fh), Some(HandleEntry::Symlink { .. }))
    }

    /// Check if a handle is a root directory.
    pub fn is_root(&self, fh: &FileHandle) -> bool {
        let handles = self.handles.lock().lock_or_die("nfs_ops.unknown");
        matches!(handles.get(fh), Some(HandleEntry::Root { .. }))
    }

    /// Check if a handle is a directory (root *or* subdirectory).
    /// Used by `lookup_by_name` to set the right `FileType` on
    /// the returned attrs and by NFSv4 `getattr` to decide
    /// `FATTR4_TYPE`.
    pub fn is_directory(&self, fh: &FileHandle) -> bool {
        let handles = self.handles.lock().lock_or_die("nfs_ops.unknown");
        matches!(
            handles.get(fh),
            Some(
                HandleEntry::Root { .. } | HandleEntry::Directory { .. } | HandleEntry::PseudoRoot
            ),
        )
    }

    /// Register a subdirectory handle minted by `mkdir`. The handle
    /// bytes are produced by the caller (deterministic UUIDv5 with a
    /// `0xFE` marker so the byte pattern is distinguishable from
    /// `Root` handles' `0xFF` marker), but only landing it in the
    /// registry makes follow-up ops resolve.
    pub fn register_dir_handle(&self, fh: FileHandle, namespace_id: NamespaceId, tenant_id: OrgId) {
        self.handles.lock().lock_or_die("nfs_ops.unknown").insert(
            fh,
            HandleEntry::Directory {
                namespace_id,
                tenant_id,
            },
        );
    }

    /// Repoint a `File` handle entry at a new `composition_id`.
    /// Used when the pending-fh CREATE path's flush writes data —
    /// the placeholder `comp_id` minted by `create_pending_named`
    /// is replaced with the real one returned by `gateway.write()`,
    /// so any client still holding the original fh keeps reading
    /// the correct composition. No-op if the entry isn't a `File`.
    pub fn repoint_file(&self, fh: &FileHandle, new_comp_id: CompositionId) {
        let mut handles = self.handles.lock().lock_or_die("nfs_ops.unknown");
        if let Some(HandleEntry::File { composition_id, .. }) = handles.get_mut(fh) {
            *composition_id = new_comp_id;
        }
    }
}

/// NFS operations context — wraps gateway + handle registry + lock manager.
///
/// Methods are async-native: they `.await` gateway ops directly
/// instead of `block_on`-ing into a sub-runtime. Lifts NFS PUT from
/// the per-request `block_on` coordination tax (~6 k op/s ceiling)
/// to native-equivalent throughput.
pub struct NfsContext<G: GatewayOps> {
    pub gateway: NfsGateway<G>,
    pub handles: HandleRegistry,
    pub dir_index: DirectoryIndex,
    pub locks: LockManager,
    /// NFSv4 session/state tracker (OPEN, CLOSE, lock stateids).
    pub sessions: Arc<SessionManager>,
    /// Legacy in-memory layout manager (Phase 14). Kept until fully
    /// retired once Phase 15c completes. Phase 15b prefers
    /// `mds_layout_manager` when set.
    pub layouts: Mutex<crate::pnfs::LayoutManager>,
    /// Production MDS layout manager (ADR-038 Phase 15b). When `Some`,
    /// `op_layoutget` and `op_getdeviceinfo` route through this.
    pub mds_layout_manager: Option<Arc<crate::pnfs::MdsLayoutManager>>,
    pub tenant_id: OrgId,
    pub namespace_id: NamespaceId,
    /// Per-file write buffer. NFS clients write sequentially at
    /// increasing offsets; the buffer accumulates all writes and
    /// flushes to a single composition on CLOSE or COMMIT.
    pub write_buffers: Mutex<HashMap<FileHandle, Vec<u8>>>,
    /// Per-fh "last flushed buffer length". Used by [`Self::flush_writes`]
    /// to detect "no new data since last flush" (idempotent — skip the
    /// gateway write entirely) vs "buffer grew" (allocate a FRESH
    /// composition id and write the full cumulative buffer).
    ///
    /// Closes #50: pre-fix `flush_writes` always reused the placeholder
    /// id from the fh, hitting `mem_gateway::write`'s `create_at`
    /// idempotency no-op on the 2nd+ flush and silently dropping the
    /// new data. The 2026-05-16 GCP wedge was that no-op +
    /// useless-Raft-entry compounding. Tracking last-flushed length
    /// lets us (a) skip no-growth flushes outright (no Raft entry), and
    /// (b) mint a fresh composition id when we DO need to flush
    /// growth, so the new data actually lands.
    pub last_flushed_len: Mutex<HashMap<FileHandle, usize>>,
}

impl<G: GatewayOps> NfsContext<G> {
    /// Create a new NFS context.
    /// Create a new NFS context with default (empty) pNFS layout manager.
    pub fn new(gateway: NfsGateway<G>, tenant_id: OrgId, namespace_id: NamespaceId) -> Self {
        Self::with_storage_nodes(gateway, tenant_id, namespace_id, Vec::new())
    }

    /// Create a new NFS context with pNFS storage node addresses.
    pub fn with_storage_nodes(
        gateway: NfsGateway<G>,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        storage_nodes: Vec<String>,
    ) -> Self {
        Self::with_storage_nodes_and_mgr(gateway, tenant_id, namespace_id, storage_nodes, None)
    }

    /// Phase 15c.4 — same as `with_storage_nodes` plus an optional
    /// production `MdsLayoutManager`. When `Some`, `op_layoutget`
    /// routes through the proper Flex Files encoder
    /// (`op_layoutget_ff`) instead of the legacy FILES-layout stub
    /// fallback. Required for kernel pNFS dispatch to work.
    pub fn with_storage_nodes_and_mgr(
        gateway: NfsGateway<G>,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        storage_nodes: Vec<String>,
        mds_layout_manager: Option<Arc<crate::pnfs::MdsLayoutManager>>,
    ) -> Self {
        let handles = HandleRegistry::new();
        // Register root handle.
        handles.root_handle(namespace_id, tenant_id);

        Self {
            gateway,
            handles,
            dir_index: DirectoryIndex::new(),
            locks: LockManager::default(),
            sessions: Arc::new(SessionManager::new()),
            layouts: Mutex::new(crate::pnfs::LayoutManager::new(storage_nodes)),
            mds_layout_manager,
            tenant_id,
            namespace_id,
            write_buffers: Mutex::new(HashMap::new()),
            last_flushed_len: Mutex::new(HashMap::new()),
        }
    }

    /// Get attributes for a file handle.
    pub async fn getattr(&self, fh: &FileHandle) -> Result<NfsAttrs, GatewayError> {
        // Both `Root` (namespace root, `0xFF` marker) and `Directory`
        // (mkdir-created subdir, `0xFE` marker) share the same attrs
        // shape. `is_directory` matches both, so a single early return
        // covers both cases. Pre-2026-05-07 only `Root` had this
        // branch; `Directory` fell through into the `Some(comp_id)`
        // let-else, returned an error, and the dispatcher mapped it
        // to NFS3ERR_IO — kernel ext4 dentry cache marked the new
        // entry as `??????????` and `mkdir(1)` returned non-zero.
        if self.handles.is_directory(fh) {
            return Ok(NfsAttrs {
                file_type: FileType::Directory,
                size: 4096,
                mode: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                fileid: u64::from_le_bytes(fh[..8].try_into().unwrap_or([0; 8])),
            });
        }

        // #53: symlinks share the regular-file size/composition shape
        // (target path is stored as inline composition data) but the
        // file_type MUST be Symlink so the kernel calls READLINK
        // instead of READ when resolving `ln -s` output. Pre-fix
        // `getattr` returned Regular for every non-directory fh and
        // the kernel saw symlinks as regular files (mode -rw- + size
        // matching the target-path length, with READ returning the
        // target-path bytes — Group V #1 saved us from EIO but the
        // link was unusable).
        let is_symlink = self.handles.is_symlink(fh);

        let (ns_id, tenant_id, Some(comp_id)) = self
            .handles
            .lookup(fh)
            .ok_or_else(|| GatewayError::ProtocolError("stale file handle".into()))?
        else {
            return Err(GatewayError::ProtocolError("expected file handle".into()));
        };

        // Phase 15c.3 (B1): kernel `cat /mnt/pnfs/<uuid>` short-
        // circuits to ENOENT-equivalent when GETATTR reports size=0
        // for a non-empty composition (it skips OPEN+READ on a 0-byte
        // regular file). Resolve the actual size via gateway.list,
        // which already filters by tenant.
        let size = self
            .gateway
            .list(tenant_id, ns_id)
            .await
            .ok()
            .and_then(|entries| {
                entries
                    .into_iter()
                    .find(|(cid, _)| *cid == comp_id)
                    .map(|(_, sz)| sz)
            })
            .unwrap_or(0);

        Ok(NfsAttrs {
            file_type: if is_symlink {
                FileType::Symlink
            } else {
                FileType::Regular
            },
            size,
            mode: if is_symlink { 0o777 } else { 0o644 },
            nlink: 1,
            uid: 0,
            gid: 0,
            fileid: u64::from_le_bytes(comp_id.0.as_bytes()[..8].try_into().unwrap_or([0; 8])),
        })
    }

    /// Read from a file handle.
    pub async fn read(
        &self,
        fh: &FileHandle,
        offset: u64,
        count: u32,
    ) -> Result<NfsReadResponse, GatewayError> {
        // Buffer-served path: a CREATE-via-`create_pending_named`
        // pre-populates write_buffers, and a WRITE before the
        // post-COMMIT flush leaves data sitting there. Serve from
        // the buffer in either case rather than going to the
        // gateway — for CREATE-only the composition doesn't exist
        // yet; for WRITE-pending-COMMIT the in-buffer data is
        // newer than what the gateway holds.
        {
            let buffers = self
                .write_buffers
                .lock()
                .lock_or_die("nfs_ops.write_buffers");
            if let Some(buf) = buffers.get(fh) {
                let off = usize::try_from(offset).unwrap_or(usize::MAX);
                let end = off.saturating_add(count as usize).min(buf.len());
                let slice = if off < buf.len() {
                    buf[off..end].to_vec()
                } else {
                    Vec::new()
                };
                let eof = end >= buf.len();
                return Ok(NfsReadResponse { data: slice, eof });
            }
        }

        // Group II partial (2026-05-15): an NFS client may PUTFH a
        // synthetic `[comp_id 16][zeros 16]` handle that this server's
        // HandleRegistry never registered — e.g. a composition created
        // via the S3 gateway. The client's deterministic-fh contract
        // (kiseki-client/src/remote_nfs/v4.rs:594) is "the first 16
        // bytes ARE the composition_id, the rest is zeros". Honor it:
        // if registry lookup misses AND the fh has that exact shape,
        // resolve the comp_id directly from the bytes. The composition
        // still has to exist (otherwise `gateway.read` returns
        // CompositionNotFound and the caller maps it to NFS4ERR_IO).
        let resolved = match self.handles.lookup(fh) {
            Some((ns_id, tenant_id, Some(comp_id))) => (ns_id, tenant_id, comp_id),
            Some((_, _, None)) => {
                return Err(GatewayError::ProtocolError(
                    "cannot read a directory".into(),
                ));
            }
            None => {
                // Registry miss — try the synthetic-fh fallback.
                if fh.len() == 32 && fh[16..32].iter().all(|&b| b == 0) {
                    let mut id_bytes = [0u8; 16];
                    id_bytes.copy_from_slice(&fh[..16]);
                    let comp_id = CompositionId(uuid::Uuid::from_bytes(id_bytes));
                    (self.namespace_id, self.tenant_id, comp_id)
                } else {
                    return Err(GatewayError::ProtocolError("stale file handle".into()));
                }
            }
        };
        let (ns_id, tenant_id, comp_id) = resolved;

        self.gateway
            .read(NfsReadRequest {
                tenant_id,
                namespace_id: ns_id,
                composition_id: comp_id,
                offset,
                count,
            })
            .await
    }

    /// Buffer a write at the given offset for the given file handle.
    /// Data is accumulated and flushed on `flush_writes`.
    pub fn buffer_write(&self, fh: &FileHandle, offset: u64, data: &[u8]) {
        let mut buffers = self.write_buffers.lock().lock_or_die("nfs_ops.unknown");
        let buf = buffers.entry(*fh).or_default();
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = off.saturating_add(data.len());
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[off..end].copy_from_slice(data);
    }

    /// Flush buffered writes for a file handle. Creates a new
    /// composition with the accumulated data, updates the file handle
    /// and directory index. Returns the new file handle.
    ///
    /// Closes #50: pre-fix this function took (removed) the buffer and
    /// wrote it under the placeholder id from `fh`. The second call
    /// would build a fresh buffer (zero-padded prefix for the already-
    /// flushed range + new data at the tail) and re-submit under the
    /// SAME placeholder id — which hit `mem_gateway`'s `create_at`
    /// idempotency no-op and silently dropped the new data. Tracked
    /// by `nfs_ops::tests::sustained_writes_with_per_write_flush_concatenate`.
    ///
    /// New shape:
    /// 1. Read (don't remove) the buffer. If empty or unchanged since
    ///    the previous flush, return `Ok(None)` — no work, no Raft
    ///    entry. This is the per-COMMIT idempotence that collapses the
    ///    2026-05-16 GCP "hydrator backlog from useless deltas" load.
    /// 2. On growth: allocate a FRESH composition id (not the
    ///    placeholder) for the 2nd+ flush. The first flush still uses
    ///    the placeholder so cross-protocol GET-by-placeholder-UUID
    ///    (Group V #1) keeps working.
    /// 3. Write the FULL cumulative buffer to gateway under that id.
    /// 4. Update dir_index + handles.repoint_file at the new id.
    /// 5. Record the new flushed length so subsequent equal-length
    ///    calls go through the early-skip path.
    pub async fn flush_writes(
        &self,
        fh: &FileHandle,
    ) -> Result<Option<(FileHandle, NfsWriteResponse)>, GatewayError> {
        let (data, last_len) = {
            let buffers = self.write_buffers.lock().lock_or_die("nfs_ops.unknown");
            let last_flushed = self
                .last_flushed_len
                .lock()
                .lock_or_die("nfs_ops.last_flushed_len");
            let Some(buf) = buffers.get(fh) else {
                return Ok(None);
            };
            let last_len = last_flushed.get(fh).copied().unwrap_or(0);
            if buf.is_empty() || buf.len() <= last_len {
                // Empty (pending-CREATE marker), or no growth since
                // last flush — skip the gateway call entirely. The
                // buffer-served read path still serves stale-free
                // because it reads from the live buffer.
                return Ok(None);
            }
            // Clone the buffer so we release the lock before the
            // (potentially long) gateway.write.
            (buf.clone(), last_len)
        };

        // Choose the composition id:
        // - First flush (last_len == 0): use the placeholder from the
        //   fh's first 16 bytes. Preserves Group V #1's cross-protocol
        //   GET-by-placeholder-UUID semantics — the NFS client received
        //   that UUID at CREATE time; an S3 client doing
        //   GET /<bucket>/<uuid> must still find it.
        // - Subsequent flushes: mint a fresh UUID. `create_at`'s
        //   idempotency would otherwise drop the new data.
        let target_id = if last_len == 0 {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&fh[..16]);
            CompositionId(uuid::Uuid::from_bytes(bytes))
        } else {
            CompositionId(uuid::Uuid::new_v4())
        };

        let new_len = data.len();
        let (new_fh, resp) = self.write_with_optional_id(data, Some(target_id)).await?;

        // Update dir_index to point at the latest composition. Keeps
        // cross-protocol GET-by-name (S3) reading the freshest data;
        // older compositions become orphans and get GC'd by the chunk-
        // store cleaner. This is the explicit "compositions are
        // mutable until close" semantic for NFS-buffered writes.
        if let Some(name) = self.dir_index.name_for(self.namespace_id, fh) {
            self.dir_index.insert(
                self.namespace_id,
                name,
                new_fh,
                resp.composition_id,
                u64::from(resp.count),
            );
        }
        self.handles.repoint_file(fh, resp.composition_id);

        self.last_flushed_len
            .lock()
            .lock_or_die("nfs_ops.last_flushed_len")
            .insert(*fh, new_len);

        Ok(Some((new_fh, resp)))
    }

    /// Release per-fh write state after the final flush.
    ///
    /// Called from NFSv4 `op_close` and NFSv3 `reply_commit` (the
    /// "I'm done writing" signal) — both paths run `flush_writes`
    /// first, then this. Frees the buffered Vec so the in-memory
    /// footprint of long-running NFS clients doesn't grow unbounded
    /// with the number of files they've ever opened.
    ///
    /// Safe to call when `fh` was never written to (idempotent).
    pub fn release_buffer(&self, fh: &FileHandle) {
        self.write_buffers
            .lock()
            .lock_or_die("nfs_ops.write_buffers")
            .remove(fh);
        self.last_flushed_len
            .lock()
            .lock_or_die("nfs_ops.last_flushed_len")
            .remove(fh);
    }

    /// Write to create a new named file (NFS CREATE).
    pub async fn write_named(
        &self,
        name: &str,
        data: Vec<u8>,
    ) -> Result<(FileHandle, NfsWriteResponse), GatewayError> {
        let (fh, resp) = self.write(data).await?;
        self.dir_index.insert(
            self.namespace_id,
            name.to_owned(),
            fh,
            resp.composition_id,
            u64::from(resp.count),
        );
        Ok((fh, resp))
    }

    /// Reserve a "pending" file handle for a freshly created NFS file
    /// without actually creating a composition in the gateway.
    ///
    /// Why: NFSv3 CREATE issues an empty file. The previous design
    /// called `gateway.write([])` to materialise an empty
    /// composition, which paid the full PUT cost (Raft delta + fjall
    /// write + chunk_id derive) for ~zero data. Flame at 64 KiB PUT
    /// showed this was ~22% of CPU on the PUT critical path; the
    /// actual user data WRITE then created a *second* composition
    /// and the first one was dead weight.
    ///
    /// New design — pending fh: synthesize the handle from a fresh
    /// UUID, register it in the handle registry, insert in
    /// `dir_index` with size 0, and pre-populate `write_buffers`
    /// with an empty `Vec` so [`Self::read`] knows to serve from
    /// the buffer (returning 0 bytes on an unwritten file) rather
    /// than calling `gateway.read()` on a composition that doesn't
    /// exist yet. When the first WRITE arrives, [`Self::buffer_write`]
    /// extends that Vec; [`Self::flush_writes`] then does the
    /// single `gateway.write(data)` and replaces the dir_index
    /// entry with the real composition.
    ///
    /// Crash semantics: identical to eventual-durability mode for
    /// the composition store — an empty file CREATE that survives
    /// for less than the flush interval may not persist across a
    /// crash. NFSv3 has no fsync hook on CREATE itself (only on
    /// WRITE FILE_SYNC / COMMIT), so this matches POSIX expectations
    /// for `creat(2)` durability before the first write.
    pub fn create_pending_named(
        &self,
        name: &str,
    ) -> Result<(FileHandle, NfsWriteResponse), GatewayError> {
        // Mint a fresh composition_id locally — never bound to a
        // real composition row. The `[comp_id 16][zeros 16]` layout
        // matches what `HandleRegistry::file_handle` produces for
        // post-write handles; clients can't tell the difference.
        let placeholder_id = CompositionId(uuid::Uuid::new_v4());
        let fh = self
            .handles
            .file_handle(self.namespace_id, self.tenant_id, placeholder_id);
        // Pre-populate write_buffers with an empty Vec — both
        // signals "this fh is buffer-served" to `read` AND gives
        // `buffer_write` a starting point so the entry().or_default()
        // fast path skips a HashMap insertion on the first write.
        self.write_buffers
            .lock()
            .lock_or_die("nfs_ops.write_buffers")
            .insert(fh, Vec::new());
        self.dir_index
            .insert(self.namespace_id, name.to_owned(), fh, placeholder_id, 0);
        Ok((
            fh,
            NfsWriteResponse {
                count: 0,
                composition_id: placeholder_id,
            },
        ))
    }

    /// Write to create a new file (NFS CREATE + WRITE).
    pub async fn write(
        &self,
        data: Vec<u8>,
    ) -> Result<(FileHandle, NfsWriteResponse), GatewayError> {
        self.write_with_optional_id(data, None).await
    }

    /// Same as [`Self::write`] but pins the resulting composition to
    /// `comp_id_override` when `Some`. Used by [`Self::flush_writes`]
    /// to materialise the placeholder UUID minted at CREATE time as
    /// the actual stored composition_id — see Group V #1: without this,
    /// the NFS client's first response gives the caller a UUID that
    /// `comps.get(uuid)` doesn't resolve, so an S3 GET on the same
    /// UUID fails with 404.
    pub async fn write_with_optional_id(
        &self,
        data: Vec<u8>,
        comp_id_override: Option<CompositionId>,
    ) -> Result<(FileHandle, NfsWriteResponse), GatewayError> {
        let resp = self
            .gateway
            .write(NfsWriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data,
                comp_id_override,
            })
            .await?;

        let fh = self
            .handles
            .file_handle(self.namespace_id, self.tenant_id, resp.composition_id);

        Ok((fh, resp))
    }

    /// Look up a file by name in the namespace. Returns handle + attrs.
    pub async fn lookup_by_name(&self, name: &str) -> Option<(FileHandle, NfsAttrs)> {
        // 1) NFS-CREATE'd files and `mkdir`'d subdirs are tracked in
        // `dir_index`. The index alone doesn't carry type info — we
        // ask the handle registry whether the resolved handle is a
        // directory and pick the right mode + nlink + size shape.
        if let Some(entry) = self.dir_index.lookup(self.namespace_id, name) {
            let fileid = u64::from_le_bytes(entry.file_handle[..8].try_into().unwrap_or([0; 8]));
            let attrs = if self.handles.is_directory(&entry.file_handle) {
                NfsAttrs {
                    file_type: FileType::Directory,
                    size: 4096,
                    mode: 0o755,
                    nlink: 2,
                    uid: 0,
                    gid: 0,
                    fileid,
                }
            } else if self.handles.is_symlink(&entry.file_handle) {
                // #53: LOOKUP("lnk") after `ln -s target lnk` resolves
                // here. Report Symlink so the kernel's subsequent
                // GETATTR sees ftype=NF4LNK and calls READLINK.
                NfsAttrs {
                    file_type: FileType::Symlink,
                    size: entry.size,
                    mode: 0o777,
                    nlink: 1,
                    uid: 0,
                    gid: 0,
                    fileid,
                }
            } else {
                NfsAttrs {
                    file_type: FileType::Regular,
                    size: entry.size,
                    mode: 0o644,
                    nlink: 1,
                    uid: 0,
                    gid: 0,
                    fileid,
                }
            };
            return Some((entry.file_handle, attrs));
        }

        // 2) Phase 15c.3: composition-by-UUID lookup. S3 PUT'd
        // objects are named by their composition UUID; the kernel
        // sends `LOOKUP("<uuid>")` for `dd /mnt/pnfs/<uuid>`. Parse
        // the name as a UUID and consult the gateway list.
        let uuid = uuid::Uuid::parse_str(name).ok()?;
        let comp_id = CompositionId(uuid);
        let entries = self
            .gateway
            .list(self.tenant_id, self.namespace_id)
            .await
            .ok()?;
        let (_, size) = entries.iter().find(|(cid, _)| *cid == comp_id).copied()?;
        // Materialize a file handle for the composition; future
        // PUTFH/READ requests will resolve through the registry.
        let fh = self
            .handles
            .file_handle(self.namespace_id, self.tenant_id, comp_id);
        let attrs = NfsAttrs {
            file_type: FileType::Regular,
            size,
            mode: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            fileid: u64::from_le_bytes(comp_id.0.as_bytes()[..8].try_into().unwrap_or([0; 8])),
        };
        Some((fh, attrs))
    }

    /// List directory entries for READDIR.
    ///
    /// Does NOT emit `.` and `..` — the Linux NFSv4 kernel client
    /// synthesizes them locally for every directory (POSIX
    /// abstraction) using the mountpoint's local inode. Emitting
    /// them server-side as well surfaces them twice in the user-
    /// visible listing because their fileids differ (server returns
    /// fileid=1 with no real attrs; kernel synthesizes from the
    /// real local inode). 2026-05-04 GCP transport-profile run
    /// captured the duplicate-entry symptom in
    /// `.gcp-build/findings/2026-05-04-fabric-256mib-cap/client-1-nfs-state.txt`.
    /// RFC 8881 §18.26.4 says the server SHOULD include them, but
    /// every production NFSv4 server (NFS-Ganesha, knfsd, EFS)
    /// omits them for the same reason. Wire-test guard:
    /// `nfs4_server::tests::readdir_response_omits_dot_and_dotdot`.
    pub async fn readdir(&self) -> Vec<ReadDirEntry> {
        let mut entries: Vec<ReadDirEntry> = Vec::new();

        // NFS-CREATE'd files (named via dir_index). Their backing
        // compositions are tracked here so the Phase 15c.3 enumeration
        // below can skip them — otherwise `a.txt` (named) and the
        // composition's UUID would both surface in `ls`.
        let mut named_comp_ids: std::collections::HashSet<CompositionId> =
            std::collections::HashSet::new();
        for dir_entry in self.dir_index.list(self.namespace_id) {
            named_comp_ids.insert(dir_entry.composition_id);
            entries.push(ReadDirEntry {
                fileid: u64::from_le_bytes(dir_entry.file_handle[..8].try_into().unwrap_or([0; 8])),
                name: dir_entry.name,
            });
        }

        // Phase 15c.3: also enumerate compositions stored in the
        // namespace (S3-PUT'd objects have no dir_index entry but
        // are visible to NFS as files named by their UUID). Skip
        // any composition that's already surfaced via a named entry.
        if let Ok(comps) = self.gateway.list(self.tenant_id, self.namespace_id).await {
            for (comp_id, _size) in comps {
                if named_comp_ids.contains(&comp_id) {
                    continue;
                }
                entries.push(ReadDirEntry {
                    fileid: u64::from_le_bytes(
                        comp_id.0.as_bytes()[..8].try_into().unwrap_or([0; 8]),
                    ),
                    name: comp_id.0.to_string(),
                });
            }
        }

        entries
    }

    /// Remove a file by name.
    ///
    /// GH #36: this must (a) drop the dir-index binding and (b) call
    /// the gateway's `delete` path so the composition is removed and
    /// every referenced chunk's refcount is decremented. Without (b)
    /// the chunk-store's bitmap allocator never sees the space come
    /// back — on the GCP 2026-05-15 perf cluster this manifested as
    /// `device full: largest free extent is 256 KiB` after ~200 GB
    /// of cumulative writes, despite 2.8 TB of raw NVMe untouched.
    /// Decrementing chunk refcounts is necessary but not sufficient —
    /// the runtime's periodic GC task (see `runtime.rs`) is what
    /// actually frees the device extents once `refcount == 0`.
    pub async fn remove_file(&self, name: &str) -> Result<(), GatewayError> {
        // Look up before removing so we still know the composition_id
        // after the dir entry is gone.
        let Some(entry) = self.dir_index.lookup(self.namespace_id, name) else {
            return Err(GatewayError::ProtocolError("file not found".into()));
        };
        let composition_id = entry.composition_id;

        // Drop the dir-index binding first — kernel sees the file
        // gone the instant REMOVE returns. The composition delete
        // (which decrements chunk refcounts) follows so a partial
        // failure can't leave a name pointing at a half-deleted
        // composition.
        let removed = self.dir_index.remove(self.namespace_id, name);
        debug_assert!(removed, "lookup succeeded above, remove must too");

        // Skip the gateway delete for placeholder/nil compositions —
        // directory entries minted by `mkdir` use `CompositionId::nil`
        // and the gateway delete path would just error out. Real
        // files always carry a concrete UUID.
        if composition_id.0.is_nil() {
            return Ok(());
        }

        // Best-effort: gateway.delete decrements chunk refcounts and
        // emits the cluster-wide Delete delta. If the call fails (e.g.
        // the composition was already deleted out-of-band, or the
        // cluster transport is degraded) we still return Ok — the dir
        // entry is gone, the kernel is satisfied, and the next GC pass
        // is harmless because the composition's chunks remain
        // ref-held by any surviving references.
        if let Err(e) = self
            .gateway
            .delete(self.tenant_id, self.namespace_id, composition_id)
            .await
        {
            tracing::warn!(
                error = %e,
                name = %name,
                composition_id = %composition_id.0,
                "NFS REMOVE: gateway.delete failed; dir entry dropped but \
                 chunk refcounts not decremented",
            );
        }

        Ok(())
    }

    /// Rename a file within the namespace.
    pub fn rename_file(&self, old_name: &str, new_name: &str) -> Result<(), GatewayError> {
        if self.dir_index.rename(self.namespace_id, old_name, new_name) {
            Ok(())
        } else {
            Err(GatewayError::ProtocolError("source file not found".into()))
        }
    }

    /// Set file attributes (mode, size). Returns updated attrs.
    pub async fn setattr(
        &self,
        fh: &FileHandle,
        _mode: Option<u32>,
    ) -> Result<NfsAttrs, GatewayError> {
        // In-memory store: attrs are computed, not stored.
        // Return current attrs (mode update is advisory for now).
        self.getattr(fh).await
    }

    /// Create a directory. Returns handle + attrs.
    pub fn mkdir(&self, name: &str) -> Result<(FileHandle, NfsAttrs), GatewayError> {
        // Use UUID v5 (deterministic hash of namespace + name) to avoid collisions.
        let dir_uuid = uuid::Uuid::new_v5(&self.namespace_id.0, name.as_bytes());
        let mut fh = [0u8; 32];
        fh[..16].copy_from_slice(dir_uuid.as_bytes());
        fh[16] = 0xFE; // marker for subdirectory

        self.dir_index.insert(
            self.namespace_id,
            name.to_owned(),
            fh,
            CompositionId(uuid::Uuid::nil()), // dirs have no composition
            0,
        );

        // Register the new fh in the handle registry. Pre-2026-05-07
        // the dir_index insert above was the only effect — any
        // follow-up op (kernel does GETATTR on the freshly-returned
        // handle to verify) would `handles.lookup(&fh)` and miss,
        // returning `NFS3ERR_BADHANDLE` → kernel errno 521.
        self.handles
            .register_dir_handle(fh, self.namespace_id, self.tenant_id);

        Ok((
            fh,
            NfsAttrs {
                file_type: FileType::Directory,
                size: 4096,
                mode: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                fileid: u64::from_le_bytes(fh[..8].try_into().unwrap_or([0; 8])),
            },
        ))
    }

    /// Remove a directory by name.
    pub fn rmdir(&self, name: &str) -> Result<(), GatewayError> {
        if self.dir_index.remove(self.namespace_id, name) {
            Ok(())
        } else {
            Err(GatewayError::ProtocolError("directory not found".into()))
        }
    }

    /// Check access permissions. Returns allowed access bits.
    /// Single-tenant in-memory: all access granted.
    pub fn access(&self, fh: &FileHandle) -> Result<u32, GatewayError> {
        let _ = self
            .handles
            .lookup(fh)
            .ok_or_else(|| GatewayError::ProtocolError("stale handle".into()))?;
        // ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_MODIFY | ACCESS4_EXTEND | ACCESS4_DELETE | ACCESS4_EXECUTE
        Ok(0x3F)
    }

    /// Create a symbolic link. Stores target as inline data and
    /// registers the resulting fh as `HandleEntry::Symlink` so
    /// `readlink` can later distinguish it from a regular file
    /// (RFC 1813 §3.3.5 / RFC 7530 §16.11.6 require READLINK on a
    /// non-symlink to return INVAL).
    pub async fn symlink(
        &self,
        name: &str,
        target: &str,
    ) -> Result<(FileHandle, NfsAttrs), GatewayError> {
        // `self.write` calls `handles.file_handle(...)` which registers
        // the new fh as `File`. Re-register the same fh as `Symlink`
        // so the entry reflects the actual file type.
        let (fh, resp) = self.write(target.as_bytes().to_vec()).await?;
        let _ = self
            .handles
            .symlink_handle(self.namespace_id, self.tenant_id, resp.composition_id);
        self.dir_index.insert(
            self.namespace_id,
            name.to_owned(),
            fh,
            resp.composition_id,
            target.len() as u64,
        );
        Ok((
            fh,
            NfsAttrs {
                file_type: FileType::Symlink,
                size: target.len() as u64,
                mode: 0o777,
                nlink: 1,
                uid: 0,
                gid: 0,
                fileid: u64::from_le_bytes(fh[..8].try_into().unwrap_or([0; 8])),
            },
        ))
    }

    /// Read a symbolic link target. Capped at 4096 bytes (NFS3 MAXPATHLEN).
    ///
    /// RFC 1813 §3.3.5 + RFC 7530 §16.11.6: READLINK on a non-symlink
    /// target MUST return NFS3ERR_INVAL / NFS4ERR_INVAL. Pre-2026-05-15
    /// any `File` handle returned `Ok(file_contents)`, silently
    /// type-confusing a regular file as a symlink. The handle-registry
    /// `is_symlink` gate enforces the RFC contract; callers surface the
    /// `InvalidArgument` error in the wire response.
    pub async fn readlink(&self, fh: &FileHandle) -> Result<String, GatewayError> {
        if !self.handles.is_symlink(fh) {
            return Err(GatewayError::InvalidArgument(
                "READLINK on a non-symlink".into(),
            ));
        }
        let resp = self.read(fh, 0, 4096).await?;
        String::from_utf8(resp.data)
            .map_err(|_| GatewayError::ProtocolError("invalid symlink target".into()))
    }

    /// Create a hard link (within same namespace).
    pub fn link(&self, target_fh: &FileHandle, new_name: &str) -> Result<(), GatewayError> {
        let (ns, _tenant, comp_id) = self
            .handles
            .lookup(target_fh)
            .ok_or_else(|| GatewayError::ProtocolError("stale handle".into()))?;
        if ns != self.namespace_id {
            return Err(GatewayError::ProtocolError(
                "cross-namespace link (EXDEV)".into(),
            ));
        }
        self.dir_index.insert(
            self.namespace_id,
            new_name.to_owned(),
            *target_fh,
            comp_id.unwrap_or(CompositionId(uuid::Uuid::nil())),
            0,
        );
        Ok(())
    }

    /// Commit (fsync). No-op for in-memory; would flush redb for persistent.
    pub fn commit(&self) -> Result<(), GatewayError> {
        Ok(())
    }
}

/// Directory entry for READDIR response.
pub struct ReadDirEntry {
    pub fileid: u64,
    pub name: String,
}

#[cfg(test)]
mod tests {
    //! Unit tests for the NFS write-buffer + flush_writes path.
    //!
    //! These pin the sustained-write contract per #50 (F-1): multiple
    //! NFS WRITE ops to the same fh, with intervening per-write
    //! flushes (the NFSv3 stable>=1 path, and the NFSv4 COMMIT-after-
    //! each-WRITE path that Linux sends under fio --direct=1), MUST
    //! yield a final composition containing the CONCATENATION of all
    //! writes. Pre-fix the second + subsequent flushes hit the
    //! `create_at` idempotent no-op in mem_gateway.rs:2146 and the
    //! data was silently dropped.
    use super::*;
    use crate::mem_gateway::InMemoryGateway;
    use crate::nfs::NfsGateway;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_crypto::keys::SystemMasterKey;

    fn ctx() -> NfsContext<InMemoryGateway> {
        let master_key = SystemMasterKey::new([0u8; 32], KeyEpoch(1));
        let tenant = OrgId(uuid::Uuid::nil());
        let ns = NamespaceId(uuid::Uuid::from_u128(1));
        let store = CompositionStore::new();
        store.add_namespace(kiseki_composition::namespace::Namespace {
            id: ns,
            tenant_id: tenant,
            shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
            tier_policy: Vec::new(),
        });
        let gw = InMemoryGateway::new(
            store,
            kiseki_chunk::arc_async(ChunkStore::new()),
            master_key,
        );
        let nfs_gw = NfsGateway::new(gw);
        NfsContext::new(nfs_gw, tenant, ns)
    }

    /// Three sequential writes to the same fh, each followed by a
    /// flush (mirrors NFSv4.2 + fio `--direct=1`: COMMIT after every
    /// WRITE). The final file content MUST be the concatenation of
    /// all three writes.
    ///
    /// Pre-#50 the second flush hits `create_at`'s idempotent path
    /// and the data is silently dropped — read returned only the
    /// first 1 KB of 'A' with zeros in [1024, 3072) instead of
    /// AAAA…BBBB…CCCC… Fix (in the same commit / branch as this
    /// test) tracks `last_flushed_len` per fh and mints a fresh
    /// composition id on the 2nd+ flush so growth lands instead
    /// of bouncing off `create_at`'s idempotency check.
    const CHUNK: usize = 1024;

    #[tokio::test(flavor = "multi_thread")]
    async fn sustained_writes_with_per_write_flush_concatenate() {
        let ctx = ctx();
        let (fh, _) = ctx.create_pending_named("sustained-fio").expect("create");
        let a = vec![b'A'; CHUNK];
        let b = vec![b'B'; CHUNK];
        let c = vec![b'C'; CHUNK];

        ctx.buffer_write(&fh, 0, &a);
        ctx.flush_writes(&fh).await.expect("flush 1");

        ctx.buffer_write(&fh, CHUNK as u64, &b);
        ctx.flush_writes(&fh).await.expect("flush 2");

        ctx.buffer_write(&fh, (CHUNK as u64) * 2, &c);
        ctx.flush_writes(&fh).await.expect("flush 3");

        // Read the entire file back.
        let total = u32::try_from(CHUNK * 3).expect("CHUNK * 3 fits in u32");
        let resp = ctx
            .read(&fh, 0, total)
            .await
            .expect("read after sustained writes");

        let expected: Vec<u8> = a.iter().chain(b.iter()).chain(c.iter()).copied().collect();
        assert_eq!(
            resp.data.len(),
            expected.len(),
            "read returned {} bytes, expected {} — sustained-write flushes silently dropped data (#50)",
            resp.data.len(),
            expected.len(),
        );
        assert_eq!(
            resp.data, expected,
            "read content does not match concatenated writes — flush_writes dropped data (#50)"
        );
    }
}
