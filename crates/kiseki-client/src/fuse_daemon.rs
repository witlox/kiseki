//! FUSE daemon — bridges [`KisekiFuse`] to libfuse 3.x via the
//! `kiseki-fuse` safe wrapper.
//!
//! Feature-gated behind `fuse`. Provides [`mount`] to bind a
//! `KisekiFuse<G>` instance to a mount point through libfuse's low-
//! level API on the dedicated `kiseki-fuse-session` `std::thread`.
//!
//! Usage:
//! ```ignore
//! let fs = KisekiFuse::new(gateway, tenant_id, namespace_id);
//! kiseki_client::fuse_daemon::mount(fs, "/mnt/kiseki", false)?;
//! ```
//!
//! # History
//!
//! This module was the `fuser` 0.17 wrapper through commit `2b7fa0c`.
//! On 2026-05-09 the FUSE backend swapped to libfuse 3.x via
//! `kiseki-fuse` (ADR-043 rev 4 + `specs/implementation/libfuse-swap.md`).
//! The 3-phase write-lock pattern (Bug 8 / Bug 9 GCP 2026-05-04 fix)
//! and the `FOPEN_KEEP_CACHE` / 16 MiB readahead tunings are preserved.

#[cfg(feature = "fuse")]
use std::ffi::OsStr;
#[cfg(feature = "fuse")]
use std::path::Path;
#[cfg(feature = "fuse")]
use std::sync::Arc;
#[cfg(feature = "fuse")]
use std::sync::RwLock;
#[cfg(feature = "fuse")]
use std::time::{Duration, SystemTime};

#[cfg(feature = "fuse")]
use kiseki_common::locks::LockOrDie;
#[cfg(feature = "fuse")]
use kiseki_fuse::filesystem::OpContext;
#[cfg(feature = "fuse")]
use kiseki_fuse::reply::{
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
};
#[cfg(feature = "fuse")]
use kiseki_fuse::types::{FileAttr as KFuseAttr, FileType as KFuseFileType, SetAttrRequest};
#[cfg(feature = "fuse")]
use kiseki_fuse::{caps, ConnectionInfo, FuseError, KisekiFuseConfig, OpenOptions};

#[cfg(feature = "fuse")]
use crate::fuse_fs::{FileKind, KisekiFuse};
#[cfg(feature = "fuse")]
use kiseki_gateway::ops::GatewayOps;

#[cfg(feature = "fuse")]
const TTL: Duration = Duration::from_secs(1);

/// Maximum readahead bytes we negotiate on FUSE init.
///
/// The kernel default on Linux is typically 128 KiB; for a 16 MiB
/// sequential read that means 128 separate FUSE READ requests instead
/// of one. Bumping to 16 MiB lets the kernel issue 1–2 large requests
/// per chunk, dramatically reducing per-call overhead on cold reads.
/// Pinned by `read_perf_caps_tests::max_readahead_is_at_least_one_mib`.
#[cfg(feature = "fuse")]
pub(crate) const MAX_READAHEAD_BYTES: u32 = 16 * 1024 * 1024;

/// Per-file-handle cache hint flags returned on every `open` and
/// `create` reply.
///
/// `keep_cache = true` is safe to set unconditionally because kiseki's
/// chunks are content-addressed (`chunk_id = HMAC(plaintext)`) and
/// therefore immutable — the kernel can keep cached pages across
/// opens without ever showing stale data. Without this, every
/// `open(2)` invalidates the page cache and the next read goes back
/// through the gateway, defeating the kernel's repeat-read win that
/// dominates HPC/AI training workloads. Pinned by
/// `read_perf_caps_tests::open_flags_set_keep_cache_so_repeat_reads_hit_page_cache`.
#[cfg(feature = "fuse")]
pub(crate) const FILE_OPEN_OPTIONS: OpenOptions = OpenOptions {
    fh: 0,
    keep_cache: true,
    direct_io: false,
    nonseekable: false,
    cache_readdir: false,
};

/// Per-open cache hint derivation — examines the kernel-supplied
/// open `flags` (POSIX `O_*` bits) and returns the
/// [`OpenOptions`] reply for `FUSE_OPEN` / `FUSE_CREATE`.
///
/// Default (no `O_DIRECT`): the long-standing
/// `FOPEN_KEEP_CACHE` behavior — kiseki chunks are content-
/// addressed so the kernel page cache stays coherent across
/// opens and a repeat read need not refetch through the gateway.
///
/// With `O_DIRECT`: surface `FOPEN_DIRECT_IO` so the kernel
/// routes IO through the direct path (no page cache, the user
/// MUST supply aligned buffers). Without this reply bit the
/// kernel either rejects the `open(2)` with EINVAL or silently
/// services the IO from the page cache — both surface as
/// `fio --direct=1` reporting 0 MB/s. See GH issue #37.
///
/// `direct_io` and `keep_cache` are mutually exclusive — when
/// direct-IO is on, the kernel must not cache pages for this fd
/// or stale reads would slip out.
#[cfg(feature = "fuse")]
#[must_use]
pub fn open_options_for_flags(flags: i32) -> OpenOptions {
    // GH#37: when the caller opened the file with O_DIRECT, the
    // kernel only honors direct-IO semantics if the FUSE backend
    // returns FOPEN_DIRECT_IO from FUSE_OPEN / FUSE_CREATE.
    // Without that reply bit the kernel either rejects the open
    // with EINVAL or silently routes through the page cache.
    //
    // direct_io and keep_cache are mutually exclusive: a fd with
    // direct-IO MUST bypass the page cache (or stale pre-direct
    // pages would surface on subsequent reads).
    if (flags & libc::O_DIRECT) != 0 {
        OpenOptions {
            fh: 0,
            keep_cache: false,
            direct_io: true,
            nonseekable: false,
            cache_readdir: false,
        }
    } else {
        FILE_OPEN_OPTIONS
    }
}

#[cfg(feature = "fuse")]
fn to_kiseki_fuse_attr(ino: u64, attr: &crate::fuse_fs::FileAttr) -> KFuseAttr {
    // Bug 7 (GCP 2026-05-04): the prior implementation hard-coded
    // `UNIX_EPOCH` for atime/mtime/ctime, so every FUSE `getattr`
    // reported "Jan 1 1970." Source from the wall clock as a
    // placeholder, same shape as the Bug 3 fix in
    // `nfs4_server::op_getattr`. Per-inode mtime that tracks the last
    // write is a follow-on; the wall-clock placeholder removes the
    // user-visible 1970 bug and keeps mtime monotonic so the kernel
    // doesn't believe stale cached data.
    let now = SystemTime::now();
    KFuseAttr {
        ino,
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: now,
        mtime: now,
        ctime: now,
        kind: match attr.kind {
            FileKind::Directory => KFuseFileType::Directory,
            FileKind::Regular => KFuseFileType::RegularFile,
        },
        perm: attr.mode as u16,
        nlink: attr.nlink,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        ttl: TTL,
    }
}

/// FUSE daemon wrapping a `KisekiFuse` for libfuse-driven dispatch.
///
/// Wraps `KisekiFuse` in an `RwLock` so concurrent FUSE read-path
/// callbacks (read, getattr, lookup, readdir) can run in parallel.
/// Mutating ops (write, create, unlink, mkdir, rename, flush, fsync,
/// release) take the write-lock and remain serialized.
///
/// Bug 8 (GCP 2026-05-04): the previous wrapper used a plain `Mutex`,
/// which serialized every FUSE op behind a single lock. Concurrent
/// kernel-side reads ran one-at-a-time even though `KisekiFuse::read`
/// is `&self` — capping FUSE READ at ~3% of wire throughput on the
/// 38 Gbps GCP perf cluster.
#[cfg(feature = "fuse")]
pub struct FuseDaemon<G: GatewayOps> {
    inner: RwLock<KisekiFuse<G>>,
}

#[cfg(feature = "fuse")]
impl<G: GatewayOps> FuseDaemon<G> {
    /// Create a new FUSE daemon wrapping a `KisekiFuse` instance.
    pub fn new(fs: KisekiFuse<G>) -> Self {
        Self {
            inner: RwLock::new(fs),
        }
    }

    /// Three-phase flush so the `RwLock` write is dropped across the
    /// gateway call (FUSE p99 fix). Pre-fix, every other FUSE op
    /// queued behind the flush's exclusive lock for the full
    /// gateway latency (160 ms p99 in the local single-node matrix
    /// — root cause: composition redb fsync stall).
    ///
    /// Phase 1: write lock — pop the dirty buffer + build request.
    /// Phase 2: read lock — issue the gateway call. Read lock
    ///          allows concurrent gateway calls (other flushes,
    ///          `create_build_request`) to run in parallel.
    /// Phase 3: write lock — record the new `composition_id`.
    /// Phase 4 (when `force_fsync`): read lock — call
    ///          `gateway.fsync_pending()` to force durability of
    ///          the just-written composition + any other pending
    ///          writes. Honors POSIX `fsync(2)` semantics under
    ///          the eventual-durability optimization.
    ///
    /// `force_fsync = false` for `FUSE_FLUSH` (POSIX says close
    /// has no durability guarantee), `true` for `FUSE_FSYNC`.
    fn flush_dirty_buffer(&self, ino: u64, force_fsync: bool) -> Result<(), i32> {
        // Phase 1: pop dirty buffer.
        let req = self
            .inner
            .write()
            .lock_or_die("fuse_daemon.inner.write.flush_take_buffer")
            .flush_take_buffer(ino);
        if let Some(req) = req {
            // Phase 2: gateway call (no exclusive lock).
            let resp = {
                let fs = self
                    .inner
                    .read()
                    .lock_or_die("fuse_daemon.inner.read.gateway_write");
                fs.block_gateway_pub(fs.gateway().write(req))
                    .map_err(|_| crate::fuse_fs::libc_eio())?
            };
            // Phase 3: persist composition_id.
            self.inner
                .write()
                .lock_or_die("fuse_daemon.inner.write.flush_apply_response")
                .flush_apply_response(ino, &resp);
        }
        // Phase 4: durability barrier for `fsync(2)` callers. Even
        // when there was no dirty buffer (no-op flush) we still
        // honor the durability call — the user's `fsync(2)` may
        // be sequencing prior writes by other handles on the
        // same file.
        if force_fsync {
            let fs = self
                .inner
                .read()
                .lock_or_die("fuse_daemon.inner.read.fsync_pending");
            fs.block_gateway_pub(fs.gateway().fsync_pending())
                .map_err(|_| crate::fuse_fs::libc_eio())?;
        }
        Ok(())
    }

    /// Test-only: invoke the read path through the same lock the
    /// `Filesystem::read` callback uses. Lets concurrency tests
    /// exercise the lock without the FUSE kernel surface.
    #[cfg(test)]
    pub(crate) fn read_through_lock(
        &self,
        ino: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, i32> {
        let fs = self
            .inner
            .read()
            .lock_or_die("fuse_daemon.inner.read.test_read");
        fs.read(ino, offset, size)
    }
}

#[cfg(feature = "fuse")]
impl<G: GatewayOps + Send + Sync + 'static> kiseki_fuse::Filesystem for FuseDaemon<G> {
    /// Negotiate kernel-side caching + readahead. Called once on mount
    /// before any other op.
    ///
    /// 16 MiB readahead (`MAX_READAHEAD_BYTES`) addresses the
    /// 2026-05-04 GCP perf-cluster issue where FUSE reads hit
    /// ~5 MB/s aggregate because the kernel's ~128 KiB default
    /// readahead fanned each 16 MiB userspace read into ~128 FUSE
    /// READ requests, each making a full gRPC round trip.
    ///
    /// `EXPORT_SUPPORT` is enabled so the mount can be re-exported
    /// over NFS — matches the `init_declares_export_support_flag`
    /// pin in `tests/fuse_linux.rs`.
    fn init(&self, conn: &mut ConnectionInfo) {
        conn.max_readahead = MAX_READAHEAD_BYTES;
        // Only opt-in to capabilities the kernel offers.
        conn.want |= conn.capable & caps::EXPORT_SUPPORT;
    }

    fn open(&self, _ctx: &OpContext, _ino: u64, flags: i32, reply: ReplyOpen) {
        // GH#37: honor caller-requested O_DIRECT by surfacing
        // FOPEN_DIRECT_IO so the kernel actually bypasses the
        // page cache for this fd. Otherwise `fio --direct=1`
        // either errors with EINVAL or silently routes through
        // the cache and reports 0 MB/s.
        reply.opened(open_options_for_flags(flags));
    }

    fn opendir(&self, _ctx: &OpContext, _ino: u64, _flags: i32, reply: ReplyOpen) {
        // Directories never carry O_DIRECT semantics — keep_cache
        // lets the kernel cache entries between opendir + readdir.
        reply.opened(FILE_OPEN_OPTIONS);
    }

    fn getattr(&self, _ctx: &OpContext, ino: u64, reply: ReplyAttr) {
        let fs = self
            .inner
            .read()
            .lock_or_die("fuse_daemon.inner.read.getattr");
        match fs.getattr(ino) {
            Ok(attr) => reply.attr(&to_kiseki_fuse_attr(ino, &attr)),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn lookup(&self, _ctx: &OpContext, parent: u64, name: &OsStr, reply: ReplyEntry) {
        if parent != 1 {
            reply.error(FuseError::NotFound);
            return;
        }
        let fs = self
            .inner
            .read()
            .lock_or_die("fuse_daemon.inner.read.lookup");
        match fs.lookup(name.to_str().unwrap_or("")) {
            Ok(attr) => reply.entry(&to_kiseki_fuse_attr(attr.ino, &attr), 0),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn read(&self, _ctx: &OpContext, ino: u64, _fh: u64, offset: i64, size: u32, reply: ReplyData) {
        let fs = self.inner.read().lock_or_die("fuse_daemon.inner.read.read");
        // libfuse passes `offset` as i64 (POSIX off_t); kernel never
        // sends a negative value here, so saturating to 0 is safe.
        #[allow(clippy::cast_sign_loss)]
        let off = if offset < 0 { 0 } else { offset as u64 };
        match fs.read(ino, off, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn write(
        &self,
        _ctx: &OpContext,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _lock_owner: Option<kiseki_fuse::LockOwner>,
        reply: kiseki_fuse::reply::ReplyWrite,
    ) {
        let mut fs = self
            .inner
            .write()
            .lock_or_die("fuse_daemon.inner.write.write");
        #[allow(clippy::cast_sign_loss)]
        let off = if offset < 0 { 0 } else { offset as u64 };
        match fs.write(ino, off, data) {
            Ok(written) => reply.written(written as usize),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn readdir(
        &self,
        _ctx: &OpContext,
        _ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let fs = self
            .inner
            .read()
            .lock_or_die("fuse_daemon.inner.read.readdir");
        let entries = fs.readdir();
        #[allow(clippy::cast_sign_loss)]
        let start = if offset < 0 { 0 } else { offset as usize };
        for (i, entry) in entries.iter().enumerate().skip(start) {
            let kind = match entry.kind {
                FileKind::Directory => KFuseFileType::Directory,
                FileKind::Regular => KFuseFileType::RegularFile,
            };
            #[allow(clippy::cast_possible_wrap)]
            let next = (i + 1) as i64;
            if reply.add(entry.ino, next, kind, entry.name.as_bytes()) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        _ctx: &OpContext,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        if parent != 1 {
            reply.error(FuseError::NotFound);
            return;
        }
        let file_name = name.to_str().unwrap_or("").to_owned();

        // Phase 1: build the WriteRequest under a SHARED read lock —
        // multiple `create` calls validate + clone request data in
        // parallel; only `flush_take_buffer` and `*_apply_response`
        // need exclusive access.
        let req = match self
            .inner
            .read()
            .lock_or_die("fuse_daemon.inner.read.create_build_request")
            .create_build_request(1, &file_name, Vec::new())
        {
            Ok(req) => req,
            Err(e) => {
                reply.error(FuseError::Errno(e));
                return;
            }
        };
        let size = req.data.len() as u64;

        // Phase 2: gateway call — NO LOCK held. Other FUSE ops
        // (write, read, unlink) proceed concurrently. This is the
        // FUSE p99 fix: pre-fix, every other op queued behind this
        // call's exclusive lock for the full gateway latency.
        let fut_resp = {
            let fs = self
                .inner
                .read()
                .lock_or_die("fuse_daemon.inner.read.create_gateway_write");
            fs.block_gateway_pub(fs.gateway().write(req))
        };
        let resp = match fut_resp {
            Ok(r) => r,
            Err(e) => {
                reply.error(FuseError::Errno(crate::fuse_fs::gateway_err_to_errno(&e)));
                return;
            }
        };

        // Phase 3: register the inode under exclusive write lock.
        let mut fs = self
            .inner
            .write()
            .lock_or_die("fuse_daemon.inner.write.create_apply_response");
        match fs.create_apply_response(1, &file_name, size, &resp) {
            Ok(ino) => {
                let attr = match fs.getattr(ino) {
                    Ok(a) => a,
                    Err(e) => {
                        reply.error(FuseError::Errno(e));
                        return;
                    }
                };
                // GH#37: a CREATE that opens with O_DIRECT (e.g.
                // `open(path, O_CREAT | O_DIRECT | O_RDWR)`) must
                // get FOPEN_DIRECT_IO on the open-half of the
                // combined CREATE+OPEN op, same as the standalone
                // open() callback above.
                reply.created(
                    &to_kiseki_fuse_attr(ino, &attr),
                    0,
                    open_options_for_flags(flags),
                );
            }
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn flush(
        &self,
        _ctx: &OpContext,
        ino: u64,
        _fh: u64,
        _lock_owner: kiseki_fuse::LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.flush_dirty_buffer(ino, false) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn fsync(&self, _ctx: &OpContext, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        match self.flush_dirty_buffer(ino, true) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn release(
        &self,
        _ctx: &OpContext,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<kiseki_fuse::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Best-effort flush: kernel ignores errors from release.
        let _ = self.flush_dirty_buffer(ino, false);
        reply.ok();
    }

    fn unlink(&self, _ctx: &OpContext, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if parent != 1 {
            reply.error(FuseError::NotFound);
            return;
        }
        let mut fs = self
            .inner
            .write()
            .lock_or_die("fuse_daemon.inner.write.unlink");
        match fs.unlink(name.to_str().unwrap_or("")) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn mkdir(
        &self,
        _ctx: &OpContext,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if parent != 1 {
            reply.error(FuseError::NotFound);
            return;
        }
        let mut fs = self
            .inner
            .write()
            .lock_or_die("fuse_daemon.inner.write.mkdir");
        match fs.mkdir(name.to_str().unwrap_or("")) {
            Ok(ino) => match fs.getattr(ino) {
                Ok(attr) => reply.entry(&to_kiseki_fuse_attr(ino, &attr), 0),
                Err(e) => reply.error(FuseError::Errno(e)),
            },
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn rmdir(&self, _ctx: &OpContext, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if parent != 1 {
            reply.error(FuseError::NotFound);
            return;
        }
        let mut fs = self
            .inner
            .write()
            .lock_or_die("fuse_daemon.inner.write.rmdir");
        match fs.rmdir(name.to_str().unwrap_or("")) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn rename(
        &self,
        _ctx: &OpContext,
        parent: u64,
        name: &OsStr,
        _newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        if parent != 1 {
            reply.error(FuseError::NotFound);
            return;
        }
        let mut fs = self
            .inner
            .write()
            .lock_or_die("fuse_daemon.inner.write.rename");
        match fs.rename(name.to_str().unwrap_or(""), newname.to_str().unwrap_or("")) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn symlink(
        &self,
        _ctx: &OpContext,
        parent: u64,
        name: &OsStr,
        link: &std::path::Path,
        reply: ReplyEntry,
    ) {
        if parent != 1 {
            reply.error(FuseError::NotFound);
            return;
        }
        let mut fs = self
            .inner
            .write()
            .lock_or_die("fuse_daemon.inner.write.symlink");
        let target = link.to_str().unwrap_or("");
        match fs.symlink(name.to_str().unwrap_or(""), target) {
            Ok(ino) => match fs.getattr(ino) {
                Ok(attr) => reply.entry(&to_kiseki_fuse_attr(ino, &attr), 0),
                Err(e) => reply.error(FuseError::Errno(e)),
            },
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn readlink(&self, _ctx: &OpContext, ino: u64, reply: ReplyData) {
        let fs = self
            .inner
            .read()
            .lock_or_die("fuse_daemon.inner.read.readlink");
        match fs.readlink(ino) {
            Ok(target) => reply.readlink(std::path::Path::new(&target)),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }

    fn setattr(&self, _ctx: &OpContext, ino: u64, attr: SetAttrRequest, reply: ReplyAttr) {
        // Only the mode-update path is supported by KisekiFuse today
        // (the data-plane lacks chown / truncate / utime backing).
        // Other valid bits return EOPNOTSUPP rather than silently
        // succeeding — the kernel surfaces this to userspace
        // truthfully.
        if attr.valid.uid
            || attr.valid.gid
            || attr.valid.size
            || attr.valid.atime
            || attr.valid.mtime
            || attr.valid.ctime
        {
            reply.error(FuseError::Errno(libc::EOPNOTSUPP));
            return;
        }
        let mode = if attr.valid.mode {
            Some(attr.mode)
        } else {
            None
        };
        let mut fs = self
            .inner
            .write()
            .lock_or_die("fuse_daemon.inner.write.setattr");
        match fs.setattr(ino, mode) {
            Ok(updated) => reply.attr(&to_kiseki_fuse_attr(ino, &updated)),
            Err(e) => reply.error(FuseError::Errno(e)),
        }
    }
}

/// Mount a `KisekiFuse` instance at the given path.
///
/// Blocks until the filesystem is unmounted. Feature-gated behind
/// `fuse`. `read_write = true` flips the mount to RW (default RO
/// matches the HPC compute-node use case where writes go via S3).
///
/// Internally:
/// 1. Wraps `fs` in a [`FuseDaemon`] (3-phase `RwLock`).
/// 2. Builds (or reuses) a tokio multi-thread runtime that the bridge
///    spawns its async finalize tasks on.
/// 3. Calls `kiseki_fuse::mount` which spins up the dedicated
///    `kiseki-fuse-session` thread (I-FUSE-5) under a watchdog
///    (I-FUSE-8 default = abort on session-thread crash).
/// 4. Joins the watchdog — returns when the kernel unmounts the FS.
#[cfg(feature = "fuse")]
pub fn mount<G: GatewayOps + Send + Sync + 'static>(
    fs: KisekiFuse<G>,
    mountpoint: &Path,
    read_write: bool,
) -> Result<(), std::io::Error> {
    let daemon: Arc<dyn kiseki_fuse::Filesystem> = Arc::new(FuseDaemon::new(fs));
    let mut mount_options = vec!["-o".to_owned(), "fsname=kiseki".to_owned()];
    if read_write {
        tracing::info!(
            mountpoint = %mountpoint.display(),
            "FUSE mount posture: read-write (default). Use --read-only for RO datasets.",
        );
    } else {
        // Loud log on the RO posture — F-2 (2026-05-15): operators
        // missed this on GCP perf runs because the prior default was
        // silently RO and the first POSIX write returned EROFS with
        // no daemon-side context.
        tracing::warn!(
            mountpoint = %mountpoint.display(),
            "FUSE mount posture: READ-ONLY (--read-only). All writes \
             through this mount will fail with EROFS.",
        );
        mount_options.push("-o".to_owned());
        mount_options.push("ro".to_owned());
    }
    let config = KisekiFuseConfig {
        mount_options,
        ..KisekiFuseConfig::default()
    };
    // kiseki_fuse::mount needs an ambient tokio runtime (the Bridge's
    // async finalize tasks spawn there). The CLI's main is sync, and
    // KisekiFuse uses its own runtime internally via the same `spawn
    // + mem::forget` pattern as `bin/kiseki_client.rs`. We use that
    // pattern here so the runtime survives the mount-call boundary.
    let mount_result = if tokio::runtime::Handle::try_current().is_ok() {
        kiseki_fuse::mount(daemon, mountpoint, config)
    } else {
        let rt_handle = std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("kiseki-fuse-rt")
                .build()
                .expect("failed to build kiseki-fuse tokio runtime");
            let handle = runtime.handle().clone();
            std::mem::forget(runtime);
            handle
        })
        .join()
        .expect("kiseki-fuse runtime spawn thread panicked");
        let _enter = rt_handle.enter();
        kiseki_fuse::mount(daemon, mountpoint, config)
    };
    let watchdog =
        mount_result.map_err(|e| std::io::Error::other(format!("FUSE mount failed: {e}")))?;
    watchdog
        .join()
        .map_err(|e| std::io::Error::other(format!("FUSE session exited with error: {e}")))
}

#[cfg(all(test, feature = "fuse"))]
mod read_perf_caps_tests {
    use super::*;

    /// FUSE 2026-05-04 GCP perf disaster: parallel reads measured
    /// 5.78 MB/s aggregate. Root cause was the kernel-default caps —
    /// ~128 KiB readahead (forces N round trips for an N×128 KiB
    /// read) and `keep_cache = false` on open (defeats the page cache
    /// between opens). Pin both fixes here.
    #[test]
    fn open_flags_set_keep_cache_so_repeat_reads_hit_page_cache() {
        assert!(
            FILE_OPEN_OPTIONS.keep_cache,
            "FILE_OPEN_OPTIONS.keep_cache must be true — without \
             it the kernel invalidates the page cache on every open(2), \
             so a workload that repeats reads on the same file refetches \
             through the gateway every time. Kiseki chunks are content-\
             addressed and immutable so KEEP_CACHE is always safe.",
        );
    }

    #[test]
    fn max_readahead_is_at_least_one_mib_so_large_reads_dont_chunk_to_default() {
        const MIN: u32 = 1024 * 1024;
        assert!(
            MAX_READAHEAD_BYTES >= MIN,
            "FUSE MAX_READAHEAD_BYTES = {MAX_READAHEAD_BYTES}; must be \
             >= {MIN} so the kernel doesn't fall back to its 128 KiB \
             default readahead and fan a single 16 MiB read out into \
             ~128 separate FUSE READ requests through the gRPC gateway.",
        );
    }
}

#[cfg(all(test, feature = "fuse"))]
mod attr_time_tests {
    use super::*;
    use crate::fuse_fs::{FileAttr, FileKind};

    /// Bug 7 (GCP 2026-05-04): FUSE getattr returned `mtime = Jan 1 1970`
    /// because `to_*_attr` hard-coded UNIX_EPOCH for every time field.
    /// The fix uses `SystemTime::now()` as a placeholder.
    #[test]
    fn to_kiseki_fuse_attr_does_not_return_unix_epoch() {
        let attr = FileAttr {
            ino: 42,
            size: 1024,
            kind: FileKind::Regular,
            mode: 0o644,
            nlink: 1,
        };
        let f = to_kiseki_fuse_attr(attr.ino, &attr);
        assert_ne!(
            f.mtime,
            SystemTime::UNIX_EPOCH,
            "FUSE mtime must not be epoch 0",
        );
        assert_ne!(f.ctime, SystemTime::UNIX_EPOCH);
        assert_ne!(f.atime, SystemTime::UNIX_EPOCH);
    }
}

#[cfg(all(test, feature = "fuse"))]
mod concurrency_tests {
    use super::*;
    use crate::fuse_fs::KisekiFuse;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_composition::namespace::Namespace;
    use kiseki_crypto::keys::SystemMasterKey;
    use kiseki_gateway::error::GatewayError;
    use kiseki_gateway::mem_gateway::InMemoryGateway;
    use kiseki_gateway::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Spy gateway that gates `read` on a 2-thread `Barrier`. Both
    /// readers must arrive before either is released. If the daemon
    /// serializes reads behind a `Mutex`, only one reader ever
    /// reaches the barrier and both threads deadlock.
    struct BarrierGateway {
        inner: InMemoryGateway,
        barrier: Arc<tokio::sync::Barrier>,
        max_in_flight: Arc<AtomicUsize>,
        cur_in_flight: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl GatewayOps for BarrierGateway {
        async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
            let n = self.cur_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, Ordering::SeqCst);
            self.barrier.wait().await;
            let r = self.inner.read(req).await;
            self.cur_in_flight.fetch_sub(1, Ordering::SeqCst);
            r
        }
        async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
            self.inner.write(req).await
        }
    }

    /// Bug 8 (GCP 2026-05-04): two concurrent FUSE reads must run
    /// in parallel. Pre-fix the daemon held a `Mutex<KisekiFuse>` for
    /// every op so the second reader blocked behind the first; the
    /// barrier in the spy gateway never released and both threads
    /// timed out. Post-fix (`RwLock<KisekiFuse>` + `read()` lock on
    /// the read path), both readers reach the barrier together and
    /// the test completes.
    #[test]
    fn concurrent_reads_do_not_serialize_behind_one_lock() {
        let tenant = OrgId(uuid::Uuid::from_u128(700));
        let ns = NamespaceId(uuid::Uuid::from_u128(701));
        let compositions = CompositionStore::new();
        compositions.add_namespace(Namespace {
            id: ns,
            tenant_id: tenant,
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        });
        let chunks = ChunkStore::new();
        let master_key = SystemMasterKey::new([0xCC; 32], KeyEpoch(1));
        let backing =
            InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let cur_in_flight = Arc::new(AtomicUsize::new(0));
        let spy = BarrierGateway {
            inner: backing,
            barrier: Arc::clone(&barrier),
            max_in_flight: Arc::clone(&max_in_flight),
            cur_in_flight: Arc::clone(&cur_in_flight),
        };

        let mut fs = KisekiFuse::new(spy, tenant, ns);
        let ino_a = fs.create("file-a", b"AAAAAAAAAA".to_vec()).unwrap();
        let ino_b = fs.create("file-b", b"BBBBBBBBBB".to_vec()).unwrap();
        let daemon = Arc::new(FuseDaemon::new(fs));

        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, i32>>();
        let d_a = Arc::clone(&daemon);
        let tx_a = tx.clone();
        std::thread::spawn(move || {
            let r = d_a.read_through_lock(ino_a, 0, 10);
            let _ = tx_a.send(r);
        });
        let d_b = Arc::clone(&daemon);
        let tx_b = tx;
        std::thread::spawn(move || {
            let r = d_b.read_through_lock(ino_b, 0, 10);
            let _ = tx_b.send(r);
        });

        // Both should land on the spy's barrier together. Generous 5 s
        // ceiling; serialization causes a deadlock so the recv times out.
        let r1 = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first reader timed out — daemon serialized reads behind one lock");
        let r2 = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("second reader timed out — daemon serialized reads behind one lock");
        assert!(
            r1.is_ok() && r2.is_ok(),
            "reads must succeed: {r1:?}, {r2:?}"
        );
        assert!(
            max_in_flight.load(Ordering::SeqCst) >= 2,
            "max in-flight = {}; expected >= 2 for parallel reads",
            max_in_flight.load(Ordering::SeqCst),
        );
    }
}
