//! The [`Filesystem`] trait — every kiseki FUSE backend implements this.
//!
//! The trait is sync (called from the libfuse session thread), takes
//! a [`Reply*`](crate::reply) token by value per op, and is bounded
//! `Send + Sync + 'static` (I-FUSE-6). For async finalization, route
//! through [`OpContext::bridge`].
//!
//! # Mapping to ADR-013 §"Supported (full semantics)"
//!
//! Every op in ADR-013's "supported full" matrix has a method on this
//! trait. The default impls reply `ENOSYS` for ops not yet
//! implemented by a particular backend; the libfuse-default for ops
//! NOT in this trait (e.g., `bmap`, `ioctl`, `poll`, `fallocate`,
//! `copy_file_range`, `lseek`, `tmpfile`, `statx`, `flock`, `link`,
//! `mknod`, `access`) is also `ENOSYS` — the kernel falls back to
//! its default behavior.
//!
//! `syncfs` is **omitted** in this revision pending architect
//! resolution of `specs/escalations/2026-05-09-libfuse-syncfs-not-in-318-release.md`.
//! libfuse 3.18.2 has no `fuse_lowlevel_ops::syncfs` callback to wire
//! into; the kernel's ENOSYS-fallback for FUSE_SYNCFS gives per-inode
//! `FUSE_FSYNC` which our `fsync` method handles.

use std::ffi::OsStr;
use std::path::Path;

use crate::bridge::Bridge;
use crate::reply::{
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
};
use crate::request::Request;
use crate::types::{LockOwner, SetAttrRequest};

/// Per-op context handed to every [`Filesystem`] method.
///
/// Bundles the [`Request`] (caller uid/gid/pid) with the [`Bridge`]
/// for handlers that want to spawn async finalization. Holding both
/// in one struct keeps method signatures stable as new context fields
/// are added (e.g., a tracing span, a tenant id).
#[derive(Clone)]
pub struct OpContext {
    /// The request that triggered this op.
    pub request: Request,
    /// The mount's bridge — pass to [`Bridge::spawn`] for async
    /// finalization.
    pub bridge: Bridge,
}

/// The kiseki FUSE filesystem contract.
///
/// Implementations are dispatched to from libfuse trampolines on the
/// `kiseki-fuse-session` `std::thread`. Each method receives a
/// reply-token wrapper that finalizes the op when consumed; failing
/// to consume is a wrapper-detected bug (I-FUSE-2 → `EIO` + counter).
///
/// Default-impl behavior for every method is "reply ENOSYS"; backends
/// override only the ops they support.
#[allow(unused_variables)]
pub trait Filesystem: Send + Sync + 'static {
    // -------- Inode + lookup --------

    /// Resolve a name in a directory to an inode (`stat` + entry).
    fn lookup(&self, ctx: &OpContext, parent: u64, name: &OsStr, reply: ReplyEntry) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Decrement the kernel's reference count on `ino` by `nlookup`;
    /// the backend may evict the inode from in-memory caches when the
    /// count drops to 0. No reply.
    fn forget(&self, ctx: &OpContext, ino: u64, nlookup: u64) {}

    /// Get inode attributes.
    fn getattr(&self, ctx: &OpContext, ino: u64, reply: ReplyAttr) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Set inode attributes — `chmod`, `chown`, `truncate`,
    /// `utimensat`. The [`SetAttrRequest::valid`](crate::types::SetAttrValid)
    /// bitset says which fields the caller actually wants to change.
    fn setattr(&self, ctx: &OpContext, ino: u64, attr: SetAttrRequest, reply: ReplyAttr) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    // -------- File I/O --------

    /// Open a file — return a backend-side file handle the kernel
    /// will pass back on subsequent ops.
    fn open(&self, ctx: &OpContext, ino: u64, flags: i32, reply: ReplyOpen) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Read up to `size` bytes from `ino` at `offset`.
    fn read(&self, ctx: &OpContext, ino: u64, fh: u64, offset: i64, size: u32, reply: ReplyData) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Write `data` to `ino` at `offset`.
    fn write(
        &self,
        ctx: &OpContext,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        write_flags: u32,
        lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Flush — called on every `close(2)` of a file descriptor.
    /// POSIX does NOT mandate durability; backends typically push
    /// dirty buffers to the gateway without forcing fsync.
    fn flush(&self, ctx: &OpContext, ino: u64, fh: u64, lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Release — last close on the file descriptor.
    fn release(
        &self,
        ctx: &OpContext,
        ino: u64,
        fh: u64,
        flags: i32,
        lock_owner: Option<LockOwner>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Sync the file's data + metadata. POSIX `fsync(2)` /
    /// `fdatasync(2)` semantics — must guarantee durability when this
    /// returns.
    fn fsync(&self, ctx: &OpContext, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    // -------- Directory ops --------

    /// Create + open a regular file (combined `creat(2)` + `open(2)`).
    fn create(
        &self,
        ctx: &OpContext,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Remove a file.
    fn unlink(&self, ctx: &OpContext, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Make a directory.
    fn mkdir(
        &self,
        ctx: &OpContext,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Remove a directory.
    fn rmdir(&self, ctx: &OpContext, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Rename within the namespace.
    fn rename(
        &self,
        ctx: &OpContext,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Open a directory.
    fn opendir(&self, ctx: &OpContext, ino: u64, flags: i32, reply: ReplyOpen) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Read directory entries.
    fn readdir(&self, ctx: &OpContext, ino: u64, fh: u64, offset: i64, reply: ReplyDirectory) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Release a directory.
    fn releasedir(&self, ctx: &OpContext, ino: u64, fh: u64, flags: i32, reply: ReplyEmpty) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    // -------- Symlinks (ADR-013 supported full) --------

    /// Create a symbolic link.
    fn symlink(&self, ctx: &OpContext, parent: u64, name: &OsStr, link: &Path, reply: ReplyEntry) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Read the target of a symbolic link.
    fn readlink(&self, ctx: &OpContext, ino: u64, reply: ReplyData) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    // -------- Extended attributes (ADR-013 supported full) --------

    /// Get the value (or size) of an extended attribute.
    fn getxattr(&self, ctx: &OpContext, ino: u64, name: &OsStr, size: u32, reply: GetXattrReply) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Set or replace an extended attribute.
    fn setxattr(
        &self,
        ctx: &OpContext,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// List extended attribute names (or size).
    fn listxattr(&self, ctx: &OpContext, ino: u64, size: u32, reply: ListXattrReply) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Remove an extended attribute.
    fn removexattr(&self, ctx: &OpContext, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    // -------- POSIX file locks (ADR-013 supported full; fcntl) --------

    /// Get lock status (`fcntl F_GETLK`).
    #[allow(clippy::too_many_arguments)]
    fn getlk(
        &self,
        ctx: &OpContext,
        ino: u64,
        fh: u64,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        lk_type: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    /// Set lock (`fcntl F_SETLK`/`F_SETLKW`).
    #[allow(clippy::too_many_arguments)]
    fn setlk(
        &self,
        ctx: &OpContext,
        ino: u64,
        fh: u64,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        lk_type: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        reply.error(crate::error::FuseError::NotImplemented);
    }

    // -------- Filesystem stats --------

    /// `statfs(2)` — return filesystem-wide free/used counts.
    fn statfs(&self, ctx: &OpContext, ino: u64, reply: ReplyStatfs) {
        reply.error(crate::error::FuseError::NotImplemented);
    }
}

/// Reply shape for `getxattr` — the kernel asks for either the data
/// (`size > 0`) or just the data's size (`size == 0`). The backend
/// chooses with [`GetXattrReply::data`] vs [`GetXattrReply::size`].
pub struct GetXattrReply {
    inner: GetXattrInner,
}

enum GetXattrInner {
    Data(ReplyData),
    Size(ReplyXattr),
}

impl GetXattrReply {
    /// Construct from a data-path reply.
    pub(crate) fn from_data(reply: ReplyData) -> Self {
        Self {
            inner: GetXattrInner::Data(reply),
        }
    }

    /// Construct from a size-query reply.
    pub(crate) fn from_size(reply: ReplyXattr) -> Self {
        Self {
            inner: GetXattrInner::Size(reply),
        }
    }

    /// Reply with the attribute's value bytes (data path).
    ///
    /// Panics in debug mode if the kernel asked for a size query
    /// (`size == 0`) — the backend should call [`GetXattrReply::size`]
    /// in that case.
    pub fn data(self, bytes: &[u8]) {
        match self.inner {
            GetXattrInner::Data(r) => r.data(bytes),
            GetXattrInner::Size(_) => {
                #[cfg(debug_assertions)]
                panic!("kiseki-fuse: getxattr asked for size, backend replied data");
                #[cfg(not(debug_assertions))]
                {
                    if let GetXattrInner::Size(r) = self.inner {
                        r.size(bytes.len());
                    }
                }
            }
        }
    }

    /// Reply with the attribute's size (size-query path).
    ///
    /// Panics in debug mode if the kernel asked for the data — the
    /// backend should call [`GetXattrReply::data`] in that case.
    pub fn size(self, count: usize) {
        match self.inner {
            GetXattrInner::Size(r) => r.size(count),
            GetXattrInner::Data(_) => {
                #[cfg(debug_assertions)]
                panic!("kiseki-fuse: getxattr asked for data, backend replied size");
                #[cfg(not(debug_assertions))]
                {
                    // Best-effort: we have a ReplyData token; reply EIO.
                    if let GetXattrInner::Data(r) = self.inner {
                        r.error(crate::error::FuseError::Io);
                    }
                }
            }
        }
    }

    /// Reply with an error — works on either path.
    pub fn error(self, err: crate::error::FuseError) {
        match self.inner {
            GetXattrInner::Data(r) => r.error(err),
            GetXattrInner::Size(r) => r.error(err),
        }
    }
}

/// Reply shape for `listxattr` — same data-vs-size split as
/// [`GetXattrReply`].
pub struct ListXattrReply {
    inner: GetXattrInner,
}

impl ListXattrReply {
    /// Construct from a data-path reply.
    pub(crate) fn from_data(reply: ReplyData) -> Self {
        Self {
            inner: GetXattrInner::Data(reply),
        }
    }

    /// Construct from a size-query reply.
    pub(crate) fn from_size(reply: ReplyXattr) -> Self {
        Self {
            inner: GetXattrInner::Size(reply),
        }
    }

    /// Reply with the names buffer (NUL-separated, empty for empty).
    pub fn data(self, names: &[u8]) {
        match self.inner {
            GetXattrInner::Data(r) => r.data(names),
            GetXattrInner::Size(_) => {
                #[cfg(debug_assertions)]
                panic!("kiseki-fuse: listxattr asked for size, backend replied data");
                #[cfg(not(debug_assertions))]
                {
                    if let GetXattrInner::Size(r) = self.inner {
                        r.size(names.len());
                    }
                }
            }
        }
    }

    /// Reply with the size only.
    pub fn size(self, count: usize) {
        match self.inner {
            GetXattrInner::Size(r) => r.size(count),
            GetXattrInner::Data(_) => {
                #[cfg(debug_assertions)]
                panic!("kiseki-fuse: listxattr asked for data, backend replied size");
                #[cfg(not(debug_assertions))]
                {
                    if let GetXattrInner::Data(r) = self.inner {
                        r.error(crate::error::FuseError::Io);
                    }
                }
            }
        }
    }

    /// Reply with an error — works on either path.
    pub fn error(self, err: crate::error::FuseError) {
        match self.inner {
            GetXattrInner::Data(r) => r.error(err),
            GetXattrInner::Size(r) => r.error(err),
        }
    }
}
