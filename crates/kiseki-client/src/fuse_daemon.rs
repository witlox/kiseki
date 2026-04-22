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
    FileAttr as FuserAttr, FileType as FuserFileType, Filesystem, MountOption, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEntry, Request,
};

#[cfg(feature = "fuse")]
use crate::fuse_fs::{FileKind, KisekiFuse};
#[cfg(feature = "fuse")]
use kiseki_gateway::ops::GatewayOps;

#[cfg(feature = "fuse")]
use std::sync::Mutex;
#[cfg(feature = "fuse")]
use std::time::{Duration, SystemTime};

#[cfg(feature = "fuse")]
const TTL: Duration = Duration::from_secs(1);

#[cfg(feature = "fuse")]
fn to_fuser_attr(ino: u64, attr: &crate::fuse_fs::FileAttr) -> FuserAttr {
    FuserAttr {
        ino,
        size: attr.size,
        blocks: (attr.size + 511) / 512,
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
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
#[cfg(feature = "fuse")]
pub struct FuseDaemon<G: GatewayOps> {
    inner: Mutex<KisekiFuse<G>>,
}

#[cfg(feature = "fuse")]
impl<G: GatewayOps> FuseDaemon<G> {
    /// Create a new FUSE daemon wrapping a `KisekiFuse` instance.
    pub fn new(fs: KisekiFuse<G>) -> Self {
        Self {
            inner: Mutex::new(fs),
        }
    }
}

#[cfg(feature = "fuse")]
impl<G: GatewayOps + Send + Sync + 'static> Filesystem for FuseDaemon<G> {
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let fs = self.inner.lock().unwrap();
        match fs.getattr(ino) {
            Ok(attr) => reply.attr(&TTL, &to_fuser_attr(ino, &attr)),
            Err(e) => reply.error(e),
        }
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        if parent != 1 {
            reply.error(libc::ENOENT);
            return;
        }
        let fs = self.inner.lock().unwrap();
        match fs.lookup(name.to_str().unwrap_or("")) {
            Ok(attr) => reply.entry(&TTL, &to_fuser_attr(attr.ino, &attr), 0),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let fs = self.inner.lock().unwrap();
        match fs.read(ino, offset as u64, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let fs = self.inner.lock().unwrap();
        let entries = fs.readdir();
        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let kind = match entry.kind {
                FileKind::Directory => FuserFileType::Directory,
                FileKind::Regular => FuserFileType::RegularFile,
            };
            if reply.add(entry.ino, (i + 1) as i64, kind, &entry.name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        if parent != 1 {
            reply.error(libc::ENOENT);
            return;
        }
        let mut fs = self.inner.lock().unwrap();
        let file_name = name.to_str().unwrap_or("");
        match fs.create(file_name, Vec::new()) {
            Ok(ino) => {
                let attr = fs.getattr(ino).unwrap();
                reply.created(&TTL, &to_fuser_attr(ino, &attr), 0, 0, 0);
            }
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        if parent != 1 {
            reply.error(libc::ENOENT);
            return;
        }
        let mut fs = self.inner.lock().unwrap();
        match fs.unlink(name.to_str().unwrap_or("")) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if parent != 1 {
            reply.error(libc::ENOENT);
            return;
        }
        let mut fs = self.inner.lock().unwrap();
        match fs.mkdir(name.to_str().unwrap_or("")) {
            Ok(ino) => {
                let attr = fs.getattr(ino).unwrap();
                reply.entry(&TTL, &to_fuser_attr(ino, &attr), 0);
            }
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        if parent != 1 {
            reply.error(libc::ENOENT);
            return;
        }
        let mut fs = self.inner.lock().unwrap();
        match fs.rename(name.to_str().unwrap_or(""), newname.to_str().unwrap_or("")) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
}

/// Mount a `KisekiFuse` instance at the given path.
///
/// Blocks until the filesystem is unmounted. Feature-gated behind `fuse`.
#[cfg(feature = "fuse")]
pub fn mount<G: GatewayOps + Send + Sync + 'static>(
    fs: KisekiFuse<G>,
    mountpoint: &Path,
) -> Result<(), std::io::Error> {
    let daemon = FuseDaemon::new(fs);
    let options = vec![
        MountOption::RO,
        MountOption::FSName("kiseki".to_owned()),
        MountOption::AutoUnmount,
    ];
    fuser::mount2(daemon, mountpoint, &options)
        .map_err(|e| std::io::Error::other(format!("FUSE mount failed: {e}")))
}
