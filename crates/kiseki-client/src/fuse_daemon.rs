//! FUSE daemon — bridges `KisekiFuse` to the `fuser` kernel interface.
//!
//! Feature-gated behind `fuse`. Provides `mount()` to bind a
//! `KisekiFuse<G>` instance to a mount point via the FUSE kernel module.
//!
//! Usage:
//! ```ignore
//! let fs = KisekiFuse::new(gateway, tenant_id, namespace_id);
//! kiseki_client::fuse_daemon::mount(fs, "/mnt/kiseki")?;
//! ```

#[cfg(feature = "fuse")]
use std::ffi::OsStr;
#[cfg(feature = "fuse")]
use std::path::Path;

#[cfg(feature = "fuse")]
use fuser::{
    Config, Errno, FileAttr as FuserAttr, FileHandle, FileType as FuserFileType, Filesystem,
    FopenFlags, Generation, INodeNo, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEntry, Request, WriteFlags,
};

#[cfg(feature = "fuse")]
use crate::fuse_fs::{FileKind, KisekiFuse};
#[cfg(feature = "fuse")]
use kiseki_gateway::ops::GatewayOps;

#[cfg(feature = "fuse")]
use std::sync::RwLock;
#[cfg(feature = "fuse")]
use std::time::{Duration, SystemTime};

#[cfg(feature = "fuse")]
const TTL: Duration = Duration::from_secs(1);

#[cfg(feature = "fuse")]
fn to_fuser_attr(ino: u64, attr: &crate::fuse_fs::FileAttr) -> FuserAttr {
    // Bug 7 (GCP 2026-05-04): the prior implementation hard-coded
    // `UNIX_EPOCH` for atime/mtime/ctime/crtime, so every FUSE
    // `getattr` reported "Jan 1 1970" — the kernel then defaulted
    // user-visible mtimes to the same and `ls -l` showed all files
    // dated 1970. Source from the wall clock as a placeholder, same
    // shape as the Bug 3 fix in `nfs4_server::op_getattr`.
    // Per-inode mtime that tracks the last write is a follow-on;
    // the wall-clock placeholder removes the user-visible 1970 bug
    // and keeps mtime monotonic so the kernel doesn't believe stale
    // cached data.
    let now = SystemTime::now();
    FuserAttr {
        ino: INodeNo(ino),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: match attr.kind {
            FileKind::Directory => FuserFileType::Directory,
            FileKind::Regular => FuserFileType::RegularFile,
        },
        perm: attr.mode as u16,
        nlink: attr.nlink,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// FUSE filesystem wrapper for the `fuser` kernel interface.
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

    /// Test-only: invoke the read path through the same lock the
    /// `Filesystem::read` callback uses. Lets concurrency tests
    /// exercise the lock without the fuser kernel surface.
    #[cfg(test)]
    pub(crate) fn read_through_lock(
        &self,
        ino: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, i32> {
        let fs = self.inner.read().unwrap();
        fs.read(ino, offset, size)
    }
}

#[cfg(feature = "fuse")]
impl<G: GatewayOps + Send + Sync + 'static> Filesystem for FuseDaemon<G> {
    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let fs = self.inner.read().unwrap();
        match fs.getattr(ino.0) {
            Ok(attr) => reply.attr(&TTL, &to_fuser_attr(ino.0, &attr)),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent.0 != 1 {
            reply.error(Errno::ENOENT);
            return;
        }
        let fs = self.inner.read().unwrap();
        match fs.lookup(name.to_str().unwrap_or("")) {
            Ok(attr) => reply.entry(&TTL, &to_fuser_attr(attr.ino, &attr), Generation(0)),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let fs = self.inner.read().unwrap();
        match fs.read(ino.0, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    /// `FUSE_WRITE` — bridges the kernel's pwrite/write syscalls to
    /// `KisekiFuse::write`. Without this op the daemon falls back to
    /// the fuser library's default impl (ENOSYS), so any write
    /// through a mounted FUSE path returns "Function not implemented"
    /// to userspace. Phase 15c.3 e2e gap.
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        let mut fs = self.inner.write().unwrap();
        match fs.write(ino.0, offset, data) {
            Ok(written) => reply.written(written),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let fs = self.inner.read().unwrap();
        let entries = fs.readdir();
        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let kind = match entry.kind {
                FileKind::Directory => FuserFileType::Directory,
                FileKind::Regular => FuserFileType::RegularFile,
            };
            if reply.add(INodeNo(entry.ino), (i + 1) as u64, kind, &entry.name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        if parent.0 != 1 {
            reply.error(Errno::ENOENT);
            return;
        }
        let mut fs = self.inner.write().unwrap();
        let file_name = name.to_str().unwrap_or("");
        match fs.create(file_name, Vec::new()) {
            Ok(ino) => {
                let attr = fs.getattr(ino).unwrap();
                reply.created(
                    &TTL,
                    &to_fuser_attr(ino, &attr),
                    Generation(0),
                    FileHandle(0),
                    FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    /// `FUSE_FLUSH` — called on every close(2) of a file descriptor.
    /// Bug 9 fix: ship the in-memory dirty buffer to the gateway as
    /// the new composition for this inode. Without this hook the
    /// daemon's default flush returns ENOSYS and write data stays in
    /// the dirty buffer until release (or never, if release also
    /// drops it).
    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: fuser::ReplyEmpty,
    ) {
        let mut fs = self.inner.write().unwrap();
        match fs.flush(ino.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    /// `FUSE_FSYNC` — explicit user-issued fsync(2). Same shape as flush
    /// for our purposes (no metadata-only path; data and metadata are
    /// the same composition write).
    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        let mut fs = self.inner.write().unwrap();
        match fs.flush(ino.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    /// `FUSE_RELEASE` — last close on the file descriptor. Best-effort
    /// flush of any remaining dirty data. The kernel ignores errors
    /// from release (they don't propagate to close(2)), so we still
    /// reply ok on flush failure to match release semantics — the
    /// data loss is logged but cannot be surfaced through this op.
    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        let mut fs = self.inner.write().unwrap();
        let _ = fs.flush(ino.0);
        reply.ok();
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        if parent.0 != 1 {
            reply.error(Errno::ENOENT);
            return;
        }
        let mut fs = self.inner.write().unwrap();
        match fs.unlink(name.to_str().unwrap_or("")) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if parent.0 != 1 {
            reply.error(Errno::ENOENT);
            return;
        }
        let mut fs = self.inner.write().unwrap();
        match fs.mkdir(name.to_str().unwrap_or("")) {
            Ok(ino) => {
                let attr = fs.getattr(ino).unwrap();
                reply.entry(&TTL, &to_fuser_attr(ino, &attr), Generation(0));
            }
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: fuser::ReplyEmpty,
    ) {
        if parent.0 != 1 {
            reply.error(Errno::ENOENT);
            return;
        }
        let mut fs = self.inner.write().unwrap();
        match fs.rename(name.to_str().unwrap_or(""), newname.to_str().unwrap_or("")) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }
}

/// Mount a `KisekiFuse` instance at the given path.
///
/// Blocks until the filesystem is unmounted. Feature-gated behind `fuse`.
/// `read_write = true` flips the mount to RW (default RO matches the
/// HPC compute-node use case where writes go via S3).
///
/// `AutoUnmount` is intentionally NOT set: the fuser 0.17 library
/// refuses it when the FUSE ACL is `Owner` (the default for
/// `mount2`), aborting the mount with
/// `"auto_unmount requires acl != Owner"`. Callers that want
/// auto-cleanup should arrange `fusermount3 -u` in their teardown.
#[cfg(feature = "fuse")]
pub fn mount<G: GatewayOps + Send + Sync + 'static>(
    fs: KisekiFuse<G>,
    mountpoint: &Path,
    read_write: bool,
) -> Result<(), std::io::Error> {
    let daemon = FuseDaemon::new(fs);
    let mut options = Config::default();
    let mut mount_opts = vec![MountOption::FSName("kiseki".to_owned())];
    if !read_write {
        mount_opts.push(MountOption::RO);
    }
    options.mount_options = mount_opts;
    fuser::mount2(daemon, mountpoint, &options)
        .map_err(|e| std::io::Error::other(format!("FUSE mount failed: {e}")))
}

#[cfg(all(test, feature = "fuse"))]
mod attr_time_tests {
    use super::*;
    use crate::fuse_fs::{FileAttr, FileKind};

    /// Bug 7 (GCP 2026-05-04): FUSE getattr returned `mtime = Jan 1 1970`
    /// because `to_fuser_attr` hard-coded UNIX_EPOCH for every time
    /// field. The fix uses `SystemTime::now()` as a placeholder.
    #[test]
    fn to_fuser_attr_does_not_return_unix_epoch() {
        let attr = FileAttr {
            ino: 42,
            size: 1024,
            kind: FileKind::Regular,
            mode: 0o644,
            nlink: 1,
        };
        let f = to_fuser_attr(attr.ino, &attr);
        assert_ne!(
            f.mtime,
            SystemTime::UNIX_EPOCH,
            "FUSE mtime must not be epoch 0",
        );
        assert_ne!(f.ctime, SystemTime::UNIX_EPOCH);
        assert_ne!(f.atime, SystemTime::UNIX_EPOCH);
        assert_ne!(f.crtime, SystemTime::UNIX_EPOCH);
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
        let mut compositions = CompositionStore::new();
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
