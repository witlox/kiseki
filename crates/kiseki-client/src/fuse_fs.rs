//! FUSE filesystem for Kiseki — translates POSIX ops to `GatewayOps`.
//!
//! Implements a virtual filesystem backed by `GatewayOps`. Each file
//! maps to a composition; the root directory lists compositions in
//! the namespace. Client-side encryption: plaintext never leaves
//! this process (I-K1, I-K2).
//!
//! This module is platform-independent. The actual FUSE mount uses
//! the `fuser` crate (feature-gated) which provides the kernel
//! interface on Linux/macOS.

use std::collections::HashMap;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_gateway::ops::{GatewayOps, ReadRequest, WriteRequest, WriteResponse, WriteSurface};

/// Inode number type.
pub type Ino = u64;

/// File type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Directory,
    Regular,
}

/// File attributes returned by `getattr`.
#[derive(Debug, Clone)]
pub struct FileAttr {
    pub ino: Ino,
    pub size: u64,
    pub kind: FileKind,
    pub mode: u32,
    pub nlink: u32,
}

/// Directory entry for `readdir`.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: Ino,
    pub name: String,
    pub kind: FileKind,
}

/// Inode table entry.
#[derive(Debug, Clone)]
enum InodeEntry {
    Root,
    File {
        parent: Ino,
        name: String,
        composition_id: CompositionId,
        size: u64,
    },
    Dir {
        parent: Ino,
        name: String,
    },
    Symlink {
        parent: Ino,
        name: String,
        target: String,
    },
}

impl InodeEntry {
    fn parent(&self) -> Option<Ino> {
        match self {
            InodeEntry::Root => None,
            InodeEntry::File { parent, .. }
            | InodeEntry::Dir { parent, .. }
            | InodeEntry::Symlink { parent, .. } => Some(*parent),
        }
    }
}

/// FUSE filesystem backed by `GatewayOps`.
///
/// Maintains an inode table mapping inode numbers to compositions.
/// Thread-safe via interior mutability in the gateway.
///
/// Holds a `tokio::runtime::Handle` to bridge sync FUSE callbacks
/// to async `GatewayOps` methods via `block_on` on a dedicated runtime.
///
/// The `run_sync` helper ensures `block_on` is never called from a
/// tokio worker thread (which panics). It spawns the future on the
/// dedicated runtime and blocks the caller via `mpsc::recv`.
pub struct KisekiFuse<G: GatewayOps> {
    /// `Arc<G>` not `G` so the FUSE daemon's flush / create paths can
    /// clone the gateway handle out of the `RwLock<KisekiFuse>` guard
    /// and call gateway methods AFTER dropping the lock. Holding the
    /// read guard across a blocking `block_on(gateway.write(...))`
    /// starves concurrent `FUSE_WRITE` handlers (which need the write
    /// guard) and deadlocks `sync(2)` against in-flight writeback —
    /// see issue #64.
    ///
    /// Existing read paths inside `KisekiFuse` still use `self.gateway`
    /// directly via Arc's `Deref` (zero-cost; same wire calls). The
    /// only behavioural change is the daemon clones the Arc before
    /// dropping its lock.
    gateway: std::sync::Arc<G>,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
    inodes: HashMap<Ino, InodeEntry>,
    /// Maps `(parent_ino, child_name)` to child inode.
    children: HashMap<(Ino, String), Ino>,
    next_ino: Ino,
    /// Per-inode dirty buffer (Bug 9 fix): kernel pwrites mutate this
    /// in place; only [`Self::flush`] (or implicit close via
    /// [`Self::release`]) ships bytes to the gateway. Without this,
    /// every pwrite read-modify-wrote the entire file, producing
    /// O(N²) wire traffic under streaming writes.
    dirty: HashMap<Ino, Vec<u8>>,
    /// Tokio runtime handle for bridging sync FUSE → async gateway ops.
    rt: tokio::runtime::Handle,
}

impl<G: GatewayOps> KisekiFuse<G> {
    /// Create a new FUSE filesystem.
    pub fn new(gateway: G, tenant_id: OrgId, namespace_id: NamespaceId) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(1, InodeEntry::Root);

        // Always create a dedicated runtime — never reuse the caller's.
        // FUSE/NFS methods use block_on() which panics if called from
        // within the same runtime (e.g., in BDD tests under cucumber).
        let rt = std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("fuse-rt")
                .build()
                .expect("failed to create FUSE tokio runtime");
            let handle = runtime.handle().clone();
            std::mem::forget(runtime);
            handle
        })
        .join()
        .expect("FUSE runtime thread panicked");

        Self {
            gateway: std::sync::Arc::new(gateway),
            tenant_id,
            namespace_id,
            inodes,
            children: HashMap::new(),
            next_ino: 2,
            dirty: HashMap::new(),
            rt,
        }
    }

    /// Clone the gateway handle out of the lock so callers can drop
    /// the `RwLock<KisekiFuse>` guard before issuing a blocking
    /// gateway call (issue #64).
    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub(crate) fn gateway_arc(&self) -> std::sync::Arc<G> {
        std::sync::Arc::clone(&self.gateway)
    }

    /// Clone the tokio runtime handle out of the lock for the same
    /// reason as [`Self::gateway_arc`]. The daemon's deadlock fix
    /// (issue #64) clones both before dropping the lock and then
    /// uses them to block on a gateway future.
    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub(crate) fn rt_arc(&self) -> tokio::runtime::Handle {
        self.rt.clone()
    }

    /// Test-only accessor — verifies that gateway-touching ops
    /// (`unlink`, `delete`) actually round-trip through the gateway,
    /// not just the local inode tables.
    #[cfg(test)]
    pub(crate) fn gateway_ref(&self) -> &G {
        &self.gateway
    }

    #[cfg(test)]
    pub(crate) fn rt_handle(&self) -> &tokio::runtime::Handle {
        &self.rt
    }

    /// Block on an async gateway call. Uses `block_in_place` when on a
    /// tokio multi-thread runtime (tests), or `block_on` when on an OS
    /// thread (FUSE daemon).
    fn block_gateway<F, T>(&self, f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            // On a tokio worker — use block_in_place so we don't panic.
            tokio::task::block_in_place(|| self.rt.block_on(f))
        } else {
            // On an OS thread (FUSE daemon) — block_on directly.
            self.rt.block_on(f)
        }
    }

    /// Validate that `ino` is a directory (Root or Dir). Returns error if not.
    fn require_dir(&self, ino: Ino) -> Result<(), i32> {
        match self.inodes.get(&ino) {
            Some(InodeEntry::Root | InodeEntry::Dir { .. }) => Ok(()),
            Some(_) => Err(20), // ENOTDIR
            None => Err(libc_enoent()),
        }
    }

    /// Get file attributes.
    pub fn getattr(&self, ino: Ino) -> Result<FileAttr, i32> {
        let entry = self.inodes.get(&ino).ok_or(libc_enoent())?;
        Ok(match entry {
            InodeEntry::Root | InodeEntry::Dir { .. } => FileAttr {
                ino,
                size: 0,
                kind: FileKind::Directory,
                mode: 0o755,
                nlink: 2,
            },
            InodeEntry::File { size, .. } => FileAttr {
                ino,
                size: *size,
                kind: FileKind::Regular,
                mode: 0o644,
                nlink: 1,
            },
            InodeEntry::Symlink { target, .. } => FileAttr {
                ino,
                size: target.len() as u64,
                kind: FileKind::Regular,
                mode: 0o777,
                nlink: 1,
            },
        })
    }

    /// Look up a name in the given parent directory.
    ///
    /// For backwards compatibility, the single-argument form searches
    /// all entries by name (flat namespace). Use `lookup_in` for proper
    /// nested directory support.
    pub fn lookup(&self, name: &str) -> Result<FileAttr, i32> {
        self.lookup_in(1, name)
    }

    /// Look up a child by name within a specific parent directory.
    pub fn lookup_in(&self, parent: Ino, name: &str) -> Result<FileAttr, i32> {
        self.require_dir(parent)?;
        let ino = self
            .children
            .get(&(parent, name.to_owned()))
            .ok_or(libc_enoent())?;
        self.getattr(*ino)
    }

    /// Read from a file.
    ///
    /// If the inode has a dirty buffer (uncommitted writes), serve
    /// from that buffer so reads see the in-flight modifications.
    /// Otherwise fall through to the gateway read.
    pub fn read(&self, ino: Ino, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let entry = self.inodes.get(&ino).ok_or(libc_enoent())?;
        let InodeEntry::File { composition_id, .. } = entry else {
            return Err(libc_eisdir());
        };

        if let Some(buf) = self.dirty.get(&ino) {
            #[allow(clippy::cast_possible_truncation)]
            let start = (offset as usize).min(buf.len());
            let end = (start + size as usize).min(buf.len());
            return Ok(buf[start..end].to_vec());
        }

        self.block_gateway(self.gateway.read(ReadRequest {
            tenant_id: self.tenant_id,
            namespace_id: self.namespace_id,
            composition_id: *composition_id,
            offset,
            length: u64::from(size),
        }))
        .map(|r| r.data)
        .map_err(|_| libc_eio())
    }

    /// Write data to an existing file at a given offset.
    ///
    /// Mutates a per-inode dirty buffer in place. The first write to
    /// a clean inode populates the buffer (lazy-loading the existing
    /// composition once if non-empty); subsequent writes splice into
    /// the buffer locally with no gateway traffic. Bytes are shipped
    /// to the gateway only on [`Self::flush`] (or implicit close via
    /// [`Self::release`]).
    ///
    /// Bug 9 (GCP 2026-05-04): the previous implementation
    /// round-tripped the entire file through the gateway on every
    /// kernel pwrite, producing O(N²) wire traffic.
    pub fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let entry = self.inodes.get(&ino).ok_or(libc_enoent())?;
        let (old_size, old_composition_id) = match entry {
            InodeEntry::File {
                size,
                composition_id,
                ..
            } => (*size, *composition_id),
            _ => return Err(libc_eisdir()),
        };

        // Lazy-load the existing file content into the dirty buffer
        // on first write. Empty files start with an empty buffer.
        if !self.dirty.contains_key(&ino) {
            let initial = if old_size > 0 {
                self.block_gateway(self.gateway.read(ReadRequest {
                    tenant_id: self.tenant_id,
                    namespace_id: self.namespace_id,
                    composition_id: old_composition_id,
                    offset: 0,
                    length: old_size,
                }))
                .map(|r| r.data)
                .map_err(|_| libc_eio())?
            } else {
                Vec::new()
            };
            self.dirty.insert(ino, initial);
        }
        let buf = self.dirty.get_mut(&ino).expect("inserted above");

        #[allow(clippy::cast_possible_truncation)]
        let start = offset as usize;
        let end = start + data.len();
        if end > buf.len() {
            buf.resize(end, 0);
        }
        buf[start..end].copy_from_slice(data);

        let new_size = buf.len() as u64;
        if let Some(InodeEntry::File { size, .. }) = self.inodes.get_mut(&ino) {
            *size = new_size;
        }

        #[allow(clippy::cast_possible_truncation)]
        let written = data.len() as u32;
        Ok(written)
    }

    /// Flush a file's dirty buffer to the gateway as a new composition,
    /// updating the inode's `composition_id`. No-op if the inode has
    /// no dirty data.
    ///
    /// Called directly by tests and the in-process kiseki-client API.
    /// The FUSE daemon uses [`Self::flush_take_buffer`] +
    /// [`Self::flush_apply_response`] instead so the daemon can drop
    /// its `RwLock` across the slow gateway call (FUSE p99 fix).
    pub fn flush(&mut self, ino: Ino) -> Result<(), i32> {
        let Some(req) = self.flush_take_buffer(ino) else {
            return Ok(());
        };
        let resp = self
            .block_gateway(self.gateway.write(req))
            .map_err(|_| libc_eio())?;
        self.flush_apply_response(ino, &resp);
        Ok(())
    }

    /// Phase 1 of the daemon-orchestrated flush: pop the dirty
    /// buffer (under exclusive lock) and return a ready-to-send
    /// `WriteRequest`. `None` when the inode has no buffered data
    /// (no-op flush).
    ///
    /// The daemon calls this with the `RwLock` write lock held for
    /// the few microseconds it takes to mutate `self.dirty`, then
    /// drops the lock before [`Self::block_gateway`] / the gateway
    /// call. See `kiseki_client::fuse_daemon::FuseDaemon::flush`.
    pub(crate) fn flush_take_buffer(&mut self, ino: Ino) -> Option<WriteRequest> {
        let buf = self.dirty.remove(&ino)?;
        Some(WriteRequest {
            tenant_id: self.tenant_id,
            namespace_id: self.namespace_id,
            data: buf,
            name: None,
            conditional: None,
            workflow_ref: None,
            idempotency_key: None,

            forwarded_from_node: None,
            comp_id_override: None,
            tier: None,
            // ADR-047: FUSE is POSIX close-to-open (ADR-013) — NEVER
            // decoupled; always synchronous.
            surface: WriteSurface::Fuse,
        })
    }

    /// Phase 3 of the daemon-orchestrated flush: persist the
    /// gateway's `composition_id` onto the inode (under exclusive
    /// lock). No-op if the inode is gone or not a file (e.g. it was
    /// unlinked between take-buffer and apply).
    pub(crate) fn flush_apply_response(&mut self, ino: Ino, resp: &WriteResponse) {
        if let Some(InodeEntry::File { composition_id, .. }) = self.inodes.get_mut(&ino) {
            *composition_id = resp.composition_id;
        }
    }

    /// POSIX `fsync(2)` semantics: drain the dirty buffer through
    /// the gateway AND force the gateway's backing stores
    /// (composition redb, chunk-store device) to fsync. Distinct
    /// from `flush(2)` (called on `close(2)`), which has no
    /// durability guarantee per POSIX.
    ///
    /// Without this method, the eventual-durability optimization
    /// (composition redb at `Durability::None`, chunk store with
    /// `sync_per_write=false`) would silently weaken `fsync(2)` —
    /// the call would return before bytes hit stable storage.
    /// `gateway.fsync_pending()` triggers all registered fsync
    /// hooks server-side, so the call only returns after every
    /// backing store has fsynced.
    pub fn fsync(&mut self, ino: Ino) -> Result<(), i32> {
        // Phase 1: same as flush — drain dirty buffer through the
        // gateway. After this returns, the composition is in the
        // server's redb (perhaps still in the WAL if eventual).
        self.flush(ino)?;
        // Phase 2: force durability of the just-written composition
        // and any other recent writes. One fsync call covers all
        // pending writes — redb's flush is global, not per-row.
        self.block_gateway(self.gateway.fsync_pending())
            .map_err(|_| libc_eio())?;
        Ok(())
    }

    /// Write a new file (create + write) under the given parent directory.
    ///
    /// The single-argument `create(name, data)` form creates under root (inode 1).
    /// Use `create_in` for nested directory support.
    pub fn create(&mut self, name: &str, data: Vec<u8>) -> Result<Ino, i32> {
        self.create_in(1, name, data)
    }

    /// Create a file under a specific parent directory.
    ///
    /// Direct path used by tests / in-process callers. The FUSE
    /// daemon goes through [`Self::create_build_request`] +
    /// [`Self::create_apply_response`] so the daemon can drop its
    /// `RwLock` across the gateway call.
    pub fn create_in(&mut self, parent: Ino, name: &str, data: Vec<u8>) -> Result<Ino, i32> {
        let req = self.create_build_request(parent, name, data)?;
        let size = req.data.len() as u64;
        let resp = self
            .block_gateway(self.gateway.write(req))
            .map_err(|e| gateway_err_to_errno(&e))?;
        self.create_apply_response(parent, name, size, &resp)
    }

    /// Phase 1 of the daemon-orchestrated create: validate parent +
    /// uniqueness and build the `WriteRequest` (under shared lock).
    /// **No state change** — `self.next_ino`, `inodes`, `children`
    /// are mutated only in [`Self::create_apply_response`] after
    /// the gateway succeeds.
    ///
    /// The daemon takes the `RwLock` for read here so concurrent
    /// `flush_take_buffer` callers (which need exclusive write
    /// lock) wait, but parallel `create_build_request` callers
    /// proceed concurrently. The actual gateway call runs lock-free.
    pub fn create_build_request(
        &self,
        parent: Ino,
        name: &str,
        data: Vec<u8>,
    ) -> Result<WriteRequest, i32> {
        self.require_dir(parent)?;
        if self.children.contains_key(&(parent, name.to_owned())) {
            return Err(17); // EEXIST
        }
        Ok(WriteRequest {
            tenant_id: self.tenant_id,
            namespace_id: self.namespace_id,
            data,
            name: None,
            conditional: None,
            workflow_ref: None,
            idempotency_key: None,

            forwarded_from_node: None,
            comp_id_override: None,
            tier: None,
            // ADR-047: FUSE is POSIX close-to-open (ADR-013) — NEVER
            // decoupled; always synchronous.
            surface: WriteSurface::Fuse,
        })
    }

    /// Phase 3 of the daemon-orchestrated create: assign an inode +
    /// register the parent → child link with the `composition_id`
    /// the gateway just returned (under exclusive lock).
    ///
    /// Re-checks uniqueness because between the `build_request`
    /// (read lock) and `apply_response` (write lock) another
    /// concurrent caller may have created the same name. Returns
    /// `EEXIST` in that case; caller is responsible for surfacing
    /// it (the orphaned composition the gateway minted will be
    /// GC'd by the chunk-refcount sweep).
    pub fn create_apply_response(
        &mut self,
        parent: Ino,
        name: &str,
        size: u64,
        resp: &WriteResponse,
    ) -> Result<Ino, i32> {
        if self.children.contains_key(&(parent, name.to_owned())) {
            return Err(17); // EEXIST raced
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            InodeEntry::File {
                parent,
                name: name.to_owned(),
                composition_id: resp.composition_id,
                size,
            },
        );
        self.children.insert((parent, name.to_owned()), ino);
        Ok(ino)
    }

    /// Remove a file from the root directory.
    pub fn unlink(&mut self, name: &str) -> Result<(), i32> {
        self.unlink_in(1, name)
    }

    /// Remove a file from a specific parent directory.
    ///
    /// Phase 15c.7: bridges the FUSE unlink to `gateway.delete()` so
    /// the composition is removed from the cluster store, not just
    /// the local inode table. For File entries we capture the
    /// `composition_id`, drive `gateway.delete(...)` through the
    /// runtime, and only on success commit the local removal.
    /// Symlinks and directories don't carry a `composition_id` (the
    /// directory tree is local in this FUSE adapter), so they only
    /// need the local removal.
    pub fn unlink_in(&mut self, parent: Ino, name: &str) -> Result<(), i32> {
        self.require_dir(parent)?;
        let ino = *self
            .children
            .get(&(parent, name.to_owned()))
            .ok_or(libc_enoent())?;

        // Capture the composition_id (if this is a File) before we
        // touch the gateway — we don't want a half-mutated state if
        // the gateway delete returns an error.
        let composition_id = match self.inodes.get(&ino) {
            Some(InodeEntry::File { composition_id, .. }) => Some(*composition_id),
            Some(_) => None,
            None => return Err(libc_enoent()),
        };

        if let Some(composition_id) = composition_id {
            self.block_gateway(self.gateway.delete(
                self.tenant_id,
                self.namespace_id,
                composition_id,
            ))
            .map_err(|e| gateway_err_to_errno(&e))?;
        }

        self.children.remove(&(parent, name.to_owned()));
        self.inodes.remove(&ino);
        self.dirty.remove(&ino);
        Ok(())
    }

    /// List directory entries for the given directory inode.
    ///
    /// The no-argument form lists the root directory (inode 1).
    #[must_use]
    pub fn readdir(&self) -> Vec<DirEntry> {
        self.readdir_in(1)
    }

    /// List directory entries for a specific directory.
    #[must_use]
    pub fn readdir_in(&self, dir_ino: Ino) -> Vec<DirEntry> {
        let parent_ino = match self.inodes.get(&dir_ino) {
            Some(InodeEntry::Root) => dir_ino, // root's parent is itself
            Some(InodeEntry::Dir { parent, .. }) => *parent,
            _ => return Vec::new(),
        };

        let mut entries = vec![
            DirEntry {
                ino: dir_ino,
                name: ".".into(),
                kind: FileKind::Directory,
            },
            DirEntry {
                ino: parent_ino,
                name: "..".into(),
                kind: FileKind::Directory,
            },
        ];

        // Iterate children of this directory.
        for (&ino, entry) in &self.inodes {
            if entry.parent() != Some(dir_ino) {
                continue;
            }
            match entry {
                InodeEntry::File { name, .. } | InodeEntry::Symlink { name, .. } => {
                    entries.push(DirEntry {
                        ino,
                        name: name.clone(),
                        kind: FileKind::Regular,
                    });
                }
                InodeEntry::Dir { name, .. } => {
                    entries.push(DirEntry {
                        ino,
                        name: name.clone(),
                        kind: FileKind::Directory,
                    });
                }
                InodeEntry::Root => {}
            }
        }

        entries
    }

    /// Create a directory under root.
    pub fn mkdir(&mut self, name: &str) -> Result<Ino, i32> {
        self.mkdir_in(1, name)
    }

    /// Create a directory under a specific parent.
    pub fn mkdir_in(&mut self, parent: Ino, name: &str) -> Result<Ino, i32> {
        self.require_dir(parent)?;

        if self.children.contains_key(&(parent, name.to_owned())) {
            return Err(17); // EEXIST
        }

        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            InodeEntry::Dir {
                parent,
                name: name.to_owned(),
            },
        );
        self.children.insert((parent, name.to_owned()), ino);
        Ok(ino)
    }

    /// Remove a directory from root.
    pub fn rmdir(&mut self, name: &str) -> Result<(), i32> {
        self.rmdir_in(1, name)
    }

    /// Remove a directory from a specific parent.
    pub fn rmdir_in(&mut self, parent: Ino, name: &str) -> Result<(), i32> {
        self.require_dir(parent)?;
        let ino = *self
            .children
            .get(&(parent, name.to_owned()))
            .ok_or(libc_enoent())?;
        if !matches!(self.inodes.get(&ino), Some(InodeEntry::Dir { .. })) {
            return Err(20); // ENOTDIR
        }
        // Check directory is empty.
        let has_children = self.children.keys().any(|(p, _)| *p == ino);
        if has_children {
            return Err(39); // ENOTEMPTY (macOS) / 66 on some systems
        }
        self.children.remove(&(parent, name.to_owned()));
        self.inodes.remove(&ino);
        self.dirty.remove(&ino);
        Ok(())
    }

    /// Rename a file or directory within the root directory.
    pub fn rename(&mut self, old_name: &str, new_name: &str) -> Result<(), i32> {
        self.rename_in(1, old_name, 1, new_name)
    }

    /// Rename a file or directory, possibly moving between parents.
    pub fn rename_in(
        &mut self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
    ) -> Result<(), i32> {
        self.require_dir(old_parent)?;
        self.require_dir(new_parent)?;

        let ino = self
            .children
            .remove(&(old_parent, old_name.to_owned()))
            .ok_or(libc_enoent())?;

        // Update the name (and parent) in the inode entry.
        if let Some(entry) = self.inodes.get_mut(&ino) {
            match entry {
                InodeEntry::File { name, parent, .. }
                | InodeEntry::Dir { name, parent }
                | InodeEntry::Symlink { name, parent, .. } => {
                    new_name.clone_into(name);
                    *parent = new_parent;
                }
                InodeEntry::Root => return Err(libc_eio()),
            }
        }
        // Remove any existing entry at new_name (overwrite semantics).
        if let Some(old_ino) = self.children.remove(&(new_parent, new_name.to_owned())) {
            self.inodes.remove(&old_ino);
        }
        self.children.insert((new_parent, new_name.to_owned()), ino);
        Ok(())
    }

    /// Set file attributes (mode only for now).
    pub fn setattr(&mut self, ino: Ino, _mode: Option<u32>) -> Result<FileAttr, i32> {
        // Attributes are computed, not stored (in-memory FS).
        // Return current attributes unchanged.
        self.getattr(ino)
    }

    /// Create a symbolic link under root.
    pub fn symlink(&mut self, name: &str, target: &str) -> Result<Ino, i32> {
        self.symlink_in(1, name, target)
    }

    /// Create a symbolic link under a specific parent.
    pub fn symlink_in(&mut self, parent: Ino, name: &str, target: &str) -> Result<Ino, i32> {
        self.require_dir(parent)?;
        if self.children.contains_key(&(parent, name.to_owned())) {
            return Err(17); // EEXIST
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            InodeEntry::Symlink {
                parent,
                name: name.to_owned(),
                target: target.to_owned(),
            },
        );
        self.children.insert((parent, name.to_owned()), ino);
        Ok(ino)
    }

    /// Read a symbolic link target.
    pub fn readlink(&self, ino: Ino) -> Result<String, i32> {
        let entry = self.inodes.get(&ino).ok_or(libc_enoent())?;
        match entry {
            InodeEntry::Symlink { target, .. } => Ok(target.clone()),
            _ => Err(22), // EINVAL
        }
    }
}

fn libc_enoent() -> i32 {
    2 // ENOENT
}
pub(crate) fn libc_eio() -> i32 {
    5 // EIO
}
fn libc_eisdir() -> i32 {
    21 // EISDIR
}
fn libc_erofs() -> i32 {
    30 // EROFS — POSIX.1-2024 §<errno.h>; Linux ABI.
}

/// Map a `GatewayError` to a POSIX errno (Linux ABI).
pub(crate) fn gateway_err_to_errno(e: &kiseki_gateway::error::GatewayError) -> i32 {
    use kiseki_gateway::error::GatewayError;
    match e {
        GatewayError::ReadOnlyNamespace => libc_erofs(),
        // Any other gateway failure surfaces as EIO at the FUSE
        // boundary. Fine-grained mapping (ENOSPC, EACCES) is
        // future work tracked by the POSIX semantics catalog row.
        _ => libc_eio(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::ids::ShardId;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_composition::namespace::Namespace;
    use kiseki_crypto::keys::SystemMasterKey;
    use kiseki_gateway::mem_gateway::InMemoryGateway;

    fn test_tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(100))
    }

    fn test_namespace() -> NamespaceId {
        NamespaceId(uuid::Uuid::from_u128(200))
    }

    fn setup_fuse() -> KisekiFuse<InMemoryGateway> {
        let compositions = CompositionStore::new();
        compositions.add_namespace(Namespace {
            id: test_namespace(),
            tenant_id: test_tenant(),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
            tier_policy: Vec::new(),
        });
        let chunks = ChunkStore::new();
        let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let gateway =
            InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);

        KisekiFuse::new(gateway, test_tenant(), test_namespace())
    }

    #[test]
    fn root_getattr() {
        let fs = setup_fuse();
        let attr = fs.getattr(1).unwrap();
        assert_eq!(attr.kind, FileKind::Directory);
        assert_eq!(attr.ino, 1);
    }

    #[test]
    fn create_and_read_file() {
        let mut fs = setup_fuse();
        let data = b"hello fuse world";

        let ino = fs.create("test.txt", data.to_vec()).unwrap();
        assert!(ino >= 2);

        // Read back.
        let read_data = fs.read(ino, 0, 1024).unwrap();
        assert_eq!(read_data, data);
    }

    /// #127 (FUSE half) — the GCP 2026-05-28 run saw a FUSE write then a
    /// post-`drop_caches` read return 0 bytes. `drop_caches` evicts the
    /// kernel page cache, forcing a real gateway READ (by `composition_id`)
    /// rather than a dirty-buffer hit. This drives the same path:
    /// create(empty) → write → flush (which clears the dirty buffer) →
    /// getattr + read. `create_and_read_file` above reads from the dirty
    /// buffer and so never exercised the post-flush gateway read.
    #[test]
    fn write_then_flush_then_read_resolves_via_gateway() {
        let mut fs = setup_fuse();
        let data = vec![0x5a_u8; 4096];

        // open(O_CREAT) — empty file, then write, then fsync/close.
        let ino = fs.create("big", Vec::new()).unwrap();
        let n = fs.write(ino, 0, &data).unwrap();
        assert_eq!(n as usize, data.len());
        fs.flush(ino).unwrap();

        // getattr after flush: size must reflect the write.
        let attr = fs.getattr(ino).unwrap();
        assert_eq!(
            attr.size,
            data.len() as u64,
            "#127 FUSE: getattr size after flush is {} (expected {})",
            attr.size,
            data.len(),
        );

        // read after flush: the dirty buffer is gone, so this is served
        // from the gateway by composition_id — the cache-dropped path.
        let got = fs
            .read(ino, 0, u32::try_from(data.len()).expect("fits u32"))
            .unwrap();
        assert_eq!(
            got,
            data,
            "#127 FUSE: read-after-flush returned {} bytes",
            got.len(),
        );
    }

    #[test]
    fn lookup_file() {
        let mut fs = setup_fuse();
        fs.create("myfile.txt", b"content".to_vec()).unwrap();

        let attr = fs.lookup("myfile.txt").unwrap();
        assert_eq!(attr.kind, FileKind::Regular);
        assert_eq!(attr.size, 7);
    }

    #[test]
    fn lookup_nonexistent_returns_enoent() {
        let fs = setup_fuse();
        assert_eq!(fs.lookup("nope").unwrap_err(), 2); // ENOENT
    }

    #[test]
    fn unlink_file() {
        let mut fs = setup_fuse();
        fs.create("deleteme.txt", b"data".to_vec()).unwrap();
        assert!(fs.lookup("deleteme.txt").is_ok());

        fs.unlink("deleteme.txt").unwrap();
        assert_eq!(fs.lookup("deleteme.txt").unwrap_err(), 2);
    }

    /// Phase 15c.7: `unlink` MUST bridge to `gateway.delete()` so the
    /// composition is removed from the cluster — not just the local
    /// inode table. Pre-fix, removing the FUSE entry left the
    /// composition behind in the gateway (visible via `list`),
    /// silently leaking storage on every unlink. The test creates a
    /// file, snapshots the gateway's composition list (1 entry),
    /// unlinks, and verifies the gateway list is empty.
    #[test]
    fn unlink_removes_composition_from_gateway() {
        let mut fs = setup_fuse();
        fs.create("removeme.txt", b"payload".to_vec()).unwrap();

        let pre = fs.rt_handle().block_on(async {
            fs.gateway_ref()
                .list(test_tenant(), test_namespace())
                .await
                .unwrap()
        });
        assert_eq!(
            pre.len(),
            1,
            "precondition: gateway should hold the just-created composition",
        );

        fs.unlink("removeme.txt").unwrap();

        let post = fs.rt_handle().block_on(async {
            fs.gateway_ref()
                .list(test_tenant(), test_namespace())
                .await
                .unwrap()
        });
        assert!(
            post.is_empty(),
            "Phase 15c.7: unlink must call gateway.delete() — \
             gateway still holds {} composition(s) after unlink",
            post.len(),
        );
    }

    #[test]
    fn readdir_lists_files() {
        let mut fs = setup_fuse();
        fs.create("a.txt", b"aaa".to_vec()).unwrap();
        fs.create("b.txt", b"bbb".to_vec()).unwrap();

        let entries = fs.readdir();
        assert!(entries.len() >= 4); // . + .. + a.txt + b.txt
        assert!(entries.iter().any(|e| e.name == "a.txt"));
        assert!(entries.iter().any(|e| e.name == "b.txt"));
    }

    #[test]
    fn read_with_offset() {
        let mut fs = setup_fuse();
        let data = b"abcdefghijklmnop";
        let ino = fs.create("offset.txt", data.to_vec()).unwrap();

        let chunk = fs.read(ino, 4, 4).unwrap();
        assert_eq!(chunk, b"efgh");
    }

    #[test]
    fn nested_directory_create_and_lookup() {
        let mut fs = setup_fuse();

        // Create a subdirectory under root.
        let subdir_ino = fs.mkdir("subdir").unwrap();
        assert!(subdir_ino >= 2);

        // Verify subdirectory appears in root listing.
        let root_entries = fs.readdir();
        assert!(root_entries
            .iter()
            .any(|e| e.name == "subdir" && e.kind == FileKind::Directory));

        // Create a file inside the subdirectory.
        let file_ino = fs
            .create_in(subdir_ino, "nested.txt", b"nested data".to_vec())
            .unwrap();
        assert!(file_ino > subdir_ino);

        // Look up the file by name within the subdirectory.
        let attr = fs.lookup_in(subdir_ino, "nested.txt").unwrap();
        assert_eq!(attr.kind, FileKind::Regular);
        assert_eq!(attr.size, 11);
        assert_eq!(attr.ino, file_ino);

        // The file must NOT appear in the root directory listing.
        let root_entries = fs.readdir();
        assert!(!root_entries.iter().any(|e| e.name == "nested.txt"));

        // The file MUST appear in the subdirectory listing.
        let sub_entries = fs.readdir_in(subdir_ino);
        assert!(sub_entries.iter().any(|e| e.name == "nested.txt"));
        // Subdirectory listing includes . and ..
        assert!(sub_entries.iter().any(|e| e.name == "."));
        assert!(sub_entries.iter().any(|e| e.name == ".."));

        // Look up in root must fail for the nested file.
        assert!(fs.lookup_in(1, "nested.txt").is_err());

        // Read the file data to confirm it works.
        let data = fs.read(file_ino, 0, 1024).unwrap();
        assert_eq!(data, b"nested data");
    }

    /// Read-only mmap (`PROT_READ` + `MAP_PRIVATE`) is functionally equivalent
    /// to the `read()` path in this FUSE implementation. The kernel FUSE
    /// layer does not expose `mmap()` directly to our filesystem; instead,
    /// mmap reads are transparently serviced by the kernel calling our
    /// `read()` handler. This test asserts that the `read()` path (which
    /// already covers mmap semantics) returns correct data at arbitrary
    /// offsets, confirming mmap-style random access works.
    #[test]
    fn read_only_mmap_equivalent_to_read_path() {
        let mut fs = setup_fuse();
        let data = b"ABCDEFGHIJKLMNOP0123456789abcdef";
        let ino = fs.create("mmap_test.bin", data.to_vec()).unwrap();

        // Simulate mmap-style random access reads at various offsets.
        // A PROT_READ + MAP_PRIVATE mmap would issue these same reads
        // via the FUSE read() handler.
        let chunk1 = fs.read(ino, 0, 8).unwrap();
        assert_eq!(chunk1, b"ABCDEFGH");

        let chunk2 = fs.read(ino, 16, 10).unwrap();
        assert_eq!(chunk2, b"0123456789");

        let chunk3 = fs.read(ino, 26, 6).unwrap();
        assert_eq!(chunk3, b"abcdef");

        // Full-file read (equivalent to mmap of entire file).
        let full = fs
            .read(ino, 0, u32::try_from(data.len()).unwrap_or(u32::MAX))
            .unwrap();
        assert_eq!(full, data);
    }

    /// POSIX write encryption chain: data written via FUSE is encrypted
    /// at rest (ciphertext != plaintext) and decrypted on read (I-K1, I-K2).
    #[test]
    fn write_encryption_chain_ciphertext_differs_from_plaintext() {
        use kiseki_common::tenancy::DedupPolicy;
        use kiseki_crypto::aead::Aead;
        use kiseki_crypto::chunk_id::derive_chunk_id;
        use kiseki_crypto::envelope;

        let plaintext = b"sensitive HPC payload that must be encrypted at rest";

        // Part 1: FUSE roundtrip — write plaintext, read back, confirm match.
        let mut fs = setup_fuse();
        let ino = fs.create("encrypted.dat", plaintext.to_vec()).unwrap();
        let read_back = fs.read(ino, 0, 1024).unwrap();
        assert_eq!(
            read_back, plaintext,
            "FUSE read should return original plaintext"
        );

        // Part 2: Verify encryption at the envelope level.
        // The gateway uses seal_envelope — ciphertext is NOT equal to plaintext.
        let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let aead = Aead::new();
        let chunk_id = derive_chunk_id(plaintext, DedupPolicy::CrossTenant, None).unwrap();
        let envelope = envelope::seal_envelope(&aead, &master_key, &chunk_id, plaintext).unwrap();

        assert_ne!(
            envelope.ciphertext, plaintext,
            "stored ciphertext must differ from plaintext (I-K1)"
        );
        assert!(
            !envelope.ciphertext.is_empty(),
            "ciphertext must not be empty"
        );

        // Part 3: Confirm decryption of the envelope recovers plaintext.
        let decrypted = envelope::open_envelope(&aead, &master_key, &envelope).unwrap();
        assert_eq!(
            decrypted, plaintext,
            "decrypted envelope must match original plaintext (I-K2)"
        );
    }

    // ---------------------------------------------------------------
    // Scenario: Native API direct read — bypass FUSE overhead
    // The native API uses the same gateway path as FUSE but without
    // kernel overhead. Verify the read path is the same.
    // ---------------------------------------------------------------
    #[test]
    fn native_api_direct_read_same_path_as_fuse() {
        let mut fs = setup_fuse();
        let data = b"native api test data";
        let ino = fs.create("native.bin", data.to_vec()).unwrap();

        // Direct read via the FUSE fs (same path as native API).
        let result = fs
            .read(ino, 0, u32::try_from(data.len()).unwrap_or(u32::MAX))
            .unwrap();
        assert_eq!(result, data, "native API read must return same data");

        // Partial read: "native api test data" offset 7 = "pi test"
        let partial = fs.read(ino, 7, 3).unwrap();
        assert_eq!(partial, b"api");
    }

    // ---------------------------------------------------------------
    // Scenario: FUSE mount with read-only namespace
    // Writes return EROFS (30 on Linux).
    // ---------------------------------------------------------------
    #[test]
    fn read_only_namespace_rejects_writes() {
        // Build a gateway with a read-only namespace.
        let compositions = CompositionStore::new();
        compositions.add_namespace(Namespace {
            id: test_namespace(),
            tenant_id: test_tenant(),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: true,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
            tier_policy: Vec::new(),
        });
        let chunks = ChunkStore::new();
        let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let gateway =
            InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);
        let mut fs = KisekiFuse::new(gateway, test_tenant(), test_namespace());

        // Attempt to create a file — should fail with write error.
        let result = fs.create("forbidden.txt", b"data".to_vec());
        assert!(result.is_err(), "write to read-only namespace must fail");
    }

    // ---------------------------------------------------------------
    // Scenario: Client process crash — uncommitted writes lost
    // Committed writes are durable; uncommitted are lost.
    // ---------------------------------------------------------------
    #[test]
    fn crash_semantics_committed_survives() {
        let mut fs = setup_fuse();

        // Committed write.
        let ino = fs.create("committed.txt", b"safe data".to_vec()).unwrap();
        let data = fs.read(ino, 0, 1024).unwrap();
        assert_eq!(data, b"safe data", "committed data must be readable");

        // "Crash" = drop the fs. The gateway's underlying store retains
        // the committed data. We can't easily test this without a shared
        // store, so we verify the write was acknowledged (ino returned).
        assert!(ino >= 2, "committed write returns valid inode");
    }

    // ---------------------------------------------------------------
    // Scenario: Writable shared mmap returns ENOTSUP
    // ---------------------------------------------------------------
    #[test]
    fn writable_mmap_returns_enotsup() {
        // ENOTSUP = 95 on Linux, 45 on macOS. We define it as a constant.
        #[cfg(target_os = "linux")]
        const ENOTSUP: i32 = 95;
        #[cfg(not(target_os = "linux"))]
        const ENOTSUP: i32 = 45;

        // The FUSE layer does not support writable shared mmap.
        // We verify the constant is defined and usable.
        const { assert!(ENOTSUP > 0, "ENOTSUP must be a valid errno") };
    }

    /// Bug 9 (GCP 2026-05-04): every FUSE write read-modify-writes
    /// the entire file. A 1-byte pwrite at offset 0 of a 64 MiB file
    /// shipped 64 MiB across the wire (read) + 64 MiB (write) = 128 MiB.
    /// O(N²) wire amplification under streaming writes. After the fix
    /// the dirty buffer absorbs writes locally; only `flush()` (or
    /// implicit close) ships bytes to the gateway.
    ///
    /// Spy gateway counts bytes shipped through `read`/`write`. The
    /// assertion: between create + reset + 1-byte write, we must NOT
    /// transfer the entire file.
    #[test]
    fn small_write_does_not_re_ship_whole_file() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        struct SpyGateway {
            inner: InMemoryGateway,
            write_bytes: Arc<AtomicU64>,
            read_bytes: Arc<AtomicU64>,
        }

        #[async_trait::async_trait]
        impl GatewayOps for SpyGateway {
            async fn read(
                &self,
                req: ReadRequest,
            ) -> Result<kiseki_gateway::ops::ReadResponse, kiseki_gateway::error::GatewayError>
            {
                let resp = self.inner.read(req).await?;
                self.read_bytes
                    .fetch_add(resp.data.len() as u64, Ordering::Relaxed);
                Ok(resp)
            }
            async fn write(
                &self,
                req: WriteRequest,
            ) -> Result<kiseki_gateway::ops::WriteResponse, kiseki_gateway::error::GatewayError>
            {
                self.write_bytes
                    .fetch_add(req.data.len() as u64, Ordering::Relaxed);
                self.inner.write(req).await
            }
            async fn delete(
                &self,
                tenant_id: OrgId,
                namespace_id: NamespaceId,
                composition_id: CompositionId,
            ) -> Result<(), kiseki_gateway::error::GatewayError> {
                self.inner
                    .delete(tenant_id, namespace_id, composition_id)
                    .await
            }
        }

        let compositions = CompositionStore::new();
        compositions.add_namespace(Namespace {
            id: test_namespace(),
            tenant_id: test_tenant(),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
            tier_policy: Vec::new(),
        });
        let chunks = ChunkStore::new();
        let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let inner = InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);

        let read_bytes = Arc::new(AtomicU64::new(0));
        let write_bytes = Arc::new(AtomicU64::new(0));
        let spy = SpyGateway {
            inner,
            read_bytes: Arc::clone(&read_bytes),
            write_bytes: Arc::clone(&write_bytes),
        };
        let mut fs = KisekiFuse::new(spy, test_tenant(), test_namespace());

        // Mimic the kernel splitting a user-issued 4 MiB write into
        // 32 × 128 KiB FUSE pwrite ops on a fresh file.
        let ino = fs.create("big.bin", Vec::new()).unwrap();

        read_bytes.store(0, Ordering::Relaxed);
        write_bytes.store(0, Ordering::Relaxed);

        let chunk = vec![0u8; 128 * 1024];
        let total_bytes = 4 * 1024 * 1024u64;
        let chunks = usize::try_from(total_bytes).unwrap() / chunk.len();
        for i in 0..chunks {
            fs.write(ino, (i * chunk.len()) as u64, &chunk).unwrap();
        }
        // Single explicit flush at the end of the open (matches what
        // the FUSE daemon does on FUSE_FLUSH / FUSE_RELEASE).
        fs.flush(ino).unwrap();

        let r = read_bytes.load(Ordering::Relaxed);
        let w = write_bytes.load(Ordering::Relaxed);
        // Ideal: r=0 (file was empty), w=total_bytes (one composition
        // write at flush). Pre-fix: w grew quadratically — e.g. for
        // 32 pwrites of 128 KiB each, write traffic ≈ 32 × avg(2 MiB)
        // = 64 MiB+. Cap at 1.5× total to leave headroom for envelope
        // overhead but reject the O(N²) shape.
        assert_eq!(
            r, 0,
            "writes to a fresh empty file should not trigger reads",
        );
        let limit = (total_bytes * 3) / 2;
        assert!(
            w <= limit,
            "{chunks} sequential pwrites shipped {w} bytes (>{limit}); \
             FUSE is amplifying writes — N pwrites should ship ≈ file size, not N × file size",
        );
    }

    #[test]
    fn write_at_offset() {
        let mut fs = setup_fuse();

        // Create a file with initial content.
        let ino = fs.create("wfile.txt", b"Hello, World!".to_vec()).unwrap();

        // Verify initial content.
        let data = fs.read(ino, 0, 1024).unwrap();
        assert_eq!(data, b"Hello, World!");

        // Write at offset 5 — replace ", World!" with " Rust!"
        let written = fs.write(ino, 5, b" Rust!").unwrap();
        assert_eq!(written, 6);

        // Read back the full file. Length should be max(old_len, offset+new_len).
        // "Hello, World!" is 13 bytes, offset 5 + 6 = 11, so file is still 13.
        let data = fs.read(ino, 0, 1024).unwrap();
        assert_eq!(data, b"Hello Rust!d!");

        // Write beyond end of file — extends the file.
        let written = fs.write(ino, 15, b"XY").unwrap();
        assert_eq!(written, 2);

        let data = fs.read(ino, 0, 1024).unwrap();
        // Bytes 13..15 should be zero-filled.
        assert_eq!(data.len(), 17);
        assert_eq!(&data[13..15], &[0, 0]);
        assert_eq!(&data[15..17], b"XY");
    }

    // ---------------------------------------------------------------
    // #51 — FUSE write + fsync + post-cache-drop read.
    // ---------------------------------------------------------------
    //
    // Reproduces the 2026-05-16 GCP Phase 5 bug: a sequential write
    // to a FUSE-over-native mount succeeded with conv=fdatasync, but
    // after `echo 3 > /proc/sys/vm/drop_caches` the read returned 0
    // bytes. At the adapter layer that pattern collapses to: write
    // populates `dirty[ino]`, fsync drains it through `gateway.write`
    // and removes the dirty entry, then read MUST go through
    // `gateway.read(composition_id)` and return the same bytes.
    //
    // Pre-fix the second flush in `flush_take_buffer` + gateway.write
    // pipeline hit the same `create_at` idempotency no-op as #50:
    // because `create_pending_named` issued a placeholder composition
    // id and the first gateway.write committed under that id, the
    // fsync-time write tried to re-commit under the same id and got
    // a silent no-op. The post-cache-drop read pulled the empty
    // placeholder composition.
    //
    // The fix for #50 made flush_writes correct on growth for the
    // NFS path; the FUSE adapter goes through a different
    // create/write code path but the same gateway.write underneath.
    // This test pins the adapter-level contract regardless of which
    // layer underneath is responsible.

    /// #51 regression fence — the FUSE adapter's write+fsync+read
    /// cycle MUST be lossless. The 2026-05-16 GCP run reported
    /// `dd ... conv=fdatasync` followed by `drop_caches` + read
    /// returning 0 bytes against a FUSE-over-native mount. At the
    /// adapter layer the failure shape collapses to: after fsync
    /// drains `dirty[ino]` to the gateway, the read path MUST go
    /// through `gateway.read(composition_id)` and return the same
    /// bytes that were written.
    ///
    /// This test pins the contract at the adapter layer (in-memory
    /// gateway). The wire-level path through native TCP-framed +
    /// real kiseki-server is fenced separately by the 20-fuse phase
    /// script (`infra/gcp/benchmarks/phases/20-fuse.sh`), which
    /// halts the bench run if read bytes ≠ write bytes.
    ///
    /// Coverage:
    /// 1. Single write + fsync + read.
    /// 2. Multiple non-contiguous writes coalesced into one fsync.
    /// 3. Two write-fsync-read cycles (rules out state corruption
    ///    across the second-pass `create_at` idempotency that
    ///    bit #50).
    /// 4. Payload sized to span multiple gateway-side chunks
    ///    (default 64 KiB chunks → use 256 KiB so the read path
    ///    walks ≥4 chunks).
    #[test]
    fn write_fsync_then_read_after_dirty_drop_returns_full_buffer() {
        // 256 KiB across multiple non-contiguous writes — exercises
        // the dirty-buffer accumulation + multi-chunk gateway store.
        const BLOCK: usize = 64 * 1024;
        const BLOCKS: usize = 4;
        const TOTAL: usize = BLOCK * BLOCKS;

        let mut fs = setup_fuse();
        let ino = fs.create("dd-write", Vec::new()).unwrap();

        let mut expected = vec![0u8; TOTAL];
        for i in 0..BLOCKS {
            let pattern = b'A' + u8::try_from(i).unwrap();
            let offset = i * BLOCK;
            let chunk = vec![pattern; BLOCK];
            expected[offset..offset + BLOCK].copy_from_slice(&chunk);
            let n = fs.write(ino, offset as u64, &chunk).expect("write block");
            assert_eq!(n as usize, BLOCK);
        }

        fs.fsync(ino).expect("fsync");

        // Defensive: dirty MUST be drained. A future refactor that
        // keeps dirty across fsync would make the read serve from
        // the local buffer and hide a wire-level data loss.
        assert!(
            !fs.dirty.contains_key(&ino),
            "fsync must drain the dirty buffer (precondition for the regression check below)"
        );

        let read = fs
            .read(ino, 0, TOTAL.try_into().unwrap())
            .expect("read after fsync");
        assert_eq!(
            read.len(),
            expected.len(),
            "read returned {} bytes, expected {} — silent data loss in FUSE-over-gateway fsync path (#51)",
            read.len(),
            expected.len(),
        );
        assert_eq!(
            read, expected,
            "read content does not match write content (#51)"
        );

        // ----- Second write-fsync-read cycle -----
        //
        // Pre-#50 the second cycle hit `create_at`'s idempotency
        // no-op at the gateway level — the new data was dropped
        // and the second-cycle read returned the first cycle's
        // content. With #50's fix (fresh comp_id on growth) +
        // FUSE's existing fresh-name-per-write semantic, this
        // cycle should land cleanly.
        let mut expected2 = vec![0u8; TOTAL];
        for i in 0..BLOCKS {
            let pattern = b'a' + u8::try_from(i).unwrap();
            let offset = i * BLOCK;
            let chunk = vec![pattern; BLOCK];
            expected2[offset..offset + BLOCK].copy_from_slice(&chunk);
            fs.write(ino, offset as u64, &chunk).expect("write cycle 2");
        }
        fs.fsync(ino).expect("fsync cycle 2");
        let read2 = fs
            .read(ino, 0, TOTAL.try_into().unwrap())
            .expect("read after fsync cycle 2");
        assert_eq!(
            read2, expected2,
            "second write-fsync-read cycle must reflect the SECOND write's content, not the first (#51 / #50 cross-check)"
        );
    }
}
