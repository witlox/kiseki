//! Domain types shared between the [`Filesystem`](crate::filesystem::Filesystem)
//! trait and the [`reply`](crate::reply) tokens.
//!
//! These are kiseki's view of the POSIX shapes (`stat`, `flock`, etc.).
//! The wrapper converts to/from the bindgen-generated C structs in
//! [`reply`](crate::reply) where finalization happens.

use std::time::{Duration, SystemTime};

/// Inode kind — the subset kiseki supports per ADR-013.
///
/// `Symlink` is in ADR-013's "supported full" set; sockets, blocks,
/// chardevs, fifos are explicitly out of scope for the HPC/AI
/// workload (ADR-013 §"Out of scope").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// Regular file.
    RegularFile,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
}

impl FileType {
    /// libc-style mode bits for this file type.
    #[must_use]
    pub const fn to_mode_bits(self) -> u32 {
        match self {
            Self::RegularFile => libc::S_IFREG,
            Self::Directory => libc::S_IFDIR,
            Self::Symlink => libc::S_IFLNK,
        }
    }
}

/// Inode attributes returned in `getattr` / `lookup` / `setattr` /
/// `create` replies. Mirrors `struct stat`.
#[derive(Debug, Clone, Copy)]
pub struct FileAttr {
    /// Inode number.
    pub ino: u64,
    /// Total size in bytes.
    pub size: u64,
    /// Block count (size / 512, ceil).
    pub blocks: u64,
    /// Last access time.
    pub atime: SystemTime,
    /// Last modification time.
    pub mtime: SystemTime,
    /// Last status change time.
    pub ctime: SystemTime,
    /// File kind.
    pub kind: FileType,
    /// Permission bits.
    pub perm: u16,
    /// Hard link count.
    pub nlink: u32,
    /// Owner uid.
    pub uid: u32,
    /// Owning gid.
    pub gid: u32,
    /// Device id (special files).
    pub rdev: u32,
    /// Block size for filesystem I/O hints.
    pub blksize: u32,
    /// Validity TTL the wrapper should pass to libfuse.
    pub ttl: Duration,
}

/// Bitset describing which fields of [`SetAttrRequest`] the kernel asked
/// to mutate. libfuse populates this from FUSE_SETATTR's `valid` field.
///
/// Method is `is_*` rather than the raw bits because the value the
/// kernel hands us has historical accreted shape (FATTR_*); kiseki's
/// trait doesn't need every variant ADR-013 doesn't list.
#[derive(Debug, Clone, Copy, Default)]
pub struct SetAttrValid {
    /// Caller wants to set the file mode (`chmod`).
    pub mode: bool,
    /// Caller wants to set the owner uid (`chown`).
    pub uid: bool,
    /// Caller wants to set the owner gid (`chown`).
    pub gid: bool,
    /// Caller wants to set the size (`truncate` / `ftruncate`).
    pub size: bool,
    /// Caller wants to set the access time.
    pub atime: bool,
    /// Caller wants to set the modification time.
    pub mtime: bool,
    /// Caller wants to set the status-change time.
    pub ctime: bool,
}

/// Setattr arguments — the union of all fields the kernel may want to
/// change. Only the fields with the corresponding [`SetAttrValid`]
/// flag set are meaningful.
#[derive(Debug, Clone, Copy, Default)]
pub struct SetAttrRequest {
    /// Which fields are valid.
    pub valid: SetAttrValid,
    /// New mode (if `valid.mode`).
    pub mode: u32,
    /// New uid (if `valid.uid`).
    pub uid: u32,
    /// New gid (if `valid.gid`).
    pub gid: u32,
    /// New size (if `valid.size`).
    pub size: u64,
    /// New access time (if `valid.atime`).
    pub atime: Option<SystemTime>,
    /// New mtime (if `valid.mtime`).
    pub mtime: Option<SystemTime>,
    /// New ctime (if `valid.ctime`).
    pub ctime: Option<SystemTime>,
}

/// Owner of a POSIX file lock — opaque 64-bit token assigned by the
/// kernel for deduplication across `fcntl` calls. ADR-013 §"Supported
/// full" lists POSIX file locks via `getlk`/`setlk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LockOwner(pub u64);
