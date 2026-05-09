//! Reply tokens — the consume-once contract (I-FUSE-1, I-FUSE-2).
//!
//! Each `Reply*` type wraps an opaque libfuse `fuse_req_t` and a
//! state flag. The consume methods (`.attr()`, `.data()`, `.ok()`,
//! `.error()`, etc.) take `self` by value, so the type system enforces
//! that exactly one consume can happen per token. If the token's `Drop`
//! runs while still in `Pending`, the wrapper replies `EIO` to libfuse
//! and increments a Prometheus counter — guaranteeing libfuse never
//! holds a request slot indefinitely.
//!
//! The reply tokens are **`!Send` and `!Sync`**: they hold a raw
//! `*mut sys::fuse_req`, which auto-implements neither. To finalize
//! a token from a tokio task, route through [`Bridge`](crate::bridge::Bridge);
//! `Bridge::spawn` consumes the token in the bridge's `!Send` thread
//! and uses an internal `Send` envelope for the cross-thread move.
//!
//! # Reply types
//!
//! Each ADR-013 op shape needs a different libfuse reply finalizer:
//!
//! | Type | Finalizer | Used by |
//! |------|-----------|---------|
//! | [`ReplyAttr`]      | `fuse_reply_attr`     | `getattr`, `setattr` |
//! | [`ReplyEntry`]     | `fuse_reply_entry`    | `lookup`, `mkdir`, `symlink`, `link` |
//! | [`ReplyCreate`]    | `fuse_reply_create`   | `create` |
//! | [`ReplyOpen`]      | `fuse_reply_open`     | `open`, `opendir` |
//! | [`ReplyData`]      | `fuse_reply_buf`      | `read`, `readlink`, `getxattr` (data path), `listxattr` (data path) |
//! | [`ReplyWrite`]     | `fuse_reply_write`    | `write` |
//! | [`ReplyEmpty`]     | `fuse_reply_err(0)`   | `unlink`, `rmdir`, `rename`, `flush`, `release`, `fsync`, `releasedir`, `setxattr`, `removexattr`, `setlk` |
//! | [`ReplyDirectory`] | `fuse_reply_buf` of accumulated `direntry` | `readdir` |
//! | [`ReplyStatfs`]    | `fuse_reply_statfs`   | `statfs` |
//! | [`ReplyXattr`]     | `fuse_reply_xattr`    | `getxattr`/`listxattr` size-query path |
//! | [`ReplyLock`]      | `fuse_reply_lock`     | `getlk` |
//!
//! Every type also has `.error(errno: i32)` (or `.error(FuseError)`)
//! for the failure path.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use kiseki_fuse_sys as sys;
use prometheus::{register_int_counter_vec, IntCounterVec, Opts};

use crate::error::FuseError;
use crate::types::{FileAttr, FileType};

mod flock_ext {
    //! Local extension: hide the `c_short` casts behind a tiny module
    //! so the trait surface stays focused.
    use kiseki_fuse_sys::flock;

    /// Build a `flock` C struct from kiseki-side fields.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_possible_wrap)]
    pub(crate) const fn build(
        l_type: i32,
        l_whence: i32,
        l_start: i64,
        l_len: i64,
        l_pid: i32,
    ) -> flock {
        flock {
            l_type: l_type as ::core::ffi::c_short,
            l_whence: l_whence as ::core::ffi::c_short,
            l_start: l_start,
            l_len: l_len,
            l_pid: l_pid,
        }
    }
}

/// Prometheus counter for I-FUSE-2 drop-without-consume.
///
/// Wrapped in `OnceLock` because Prometheus disallows duplicate
/// registration; tests + multi-mount processes both call into this.
fn drop_without_consume_counter() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            Opts::new(
                "kiseki_fuse_drop_without_consume_total",
                "FUSE reply tokens dropped without consume — replied EIO. \
                 Non-zero count indicates a kiseki-fuse bug; see I-FUSE-2."
            ),
            &["op"],
        )
        .expect("kiseki-fuse: failed to register Prometheus counter at init")
    })
}

/// Internal state of a reply token.
///
/// `Pending(req)` carries the libfuse request handle; consume methods
/// `mem::replace` to `Consumed` and call the relevant `fuse_reply_*`.
/// Drop on `Pending` issues `fuse_reply_err(EIO)`.
enum InnerState {
    Pending(*mut sys::fuse_req),
    Consumed,
}

impl InnerState {
    /// Take the pending pointer, leaving `Consumed` in place. Returns
    /// `None` if already consumed (which shouldn't happen — every
    /// consume method takes `self` by value).
    fn take(&mut self) -> Option<*mut sys::fuse_req> {
        match std::mem::replace(self, Self::Consumed) {
            Self::Pending(req) => Some(req),
            Self::Consumed => None,
        }
    }
}

/// Common drop machinery: replies `EIO`, increments counter, warns.
fn drop_pending(state: &mut InnerState, op: &'static str) {
    if let Some(req) = state.take() {
        // SAFETY: libfuse documents `fuse_reply_err` as thread-safe and
        // valid until the request is consumed. We hold the only Rust
        // reference to `req` (it's `*mut`), and the trampoline that
        // built this token has not finalized via any other path.
        unsafe {
            sys::fuse_reply_err(req, libc::EIO);
        }
        drop_without_consume_counter()
            .with_label_values(&[op])
            .inc();
        tracing::warn!(
            op,
            "kiseki-fuse: reply token dropped without consume; replied EIO (I-FUSE-2)"
        );
        #[cfg(debug_assertions)]
        panic!(
            "kiseki-fuse: reply token for op={op} dropped without consume; \
             see I-FUSE-2 in specs/invariants.md"
        );
    }
}

// =============================================================================
// ReplyAttr — `getattr`, `setattr`
// =============================================================================

/// Reply token for `getattr` / `setattr`.
///
/// Consume via [`ReplyAttr::attr`] (success) or [`ReplyAttr::error`]
/// (failure).
pub struct ReplyAttr {
    state: InnerState,
    op: &'static str,
}

impl ReplyAttr {
    /// SAFETY: caller (the trampoline) must guarantee `req` is the
    /// libfuse-issued request handle for this op, not yet replied to.
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply with the requested attributes and TTL.
    pub fn attr(mut self, attr: &FileAttr) {
        if let Some(req) = self.state.take() {
            let st = file_attr_to_stat(attr);
            // SAFETY: see drop_pending. `&st` is a stack-local valid
            // for the call; libfuse copies it before returning.
            unsafe {
                sys::fuse_reply_attr(req, &st, attr.ttl.as_secs_f64());
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyAttr {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyEntry — `lookup`, `mkdir`, `symlink`, `link`
// =============================================================================

/// Reply token for `lookup` / `mkdir` / `symlink` / `link`.
pub struct ReplyEntry {
    state: InnerState,
    op: &'static str,
}

impl ReplyEntry {
    /// SAFETY: see [`ReplyAttr::from_raw`].
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply with the new entry's attributes + a generation number.
    /// Generation is used by NFS export — set to 0 if not exporting.
    pub fn entry(mut self, attr: &FileAttr, generation: u64) {
        if let Some(req) = self.state.take() {
            let entry = sys::fuse_entry_param {
                ino: attr.ino,
                generation,
                attr: file_attr_to_stat(attr),
                attr_timeout: attr.ttl.as_secs_f64(),
                entry_timeout: attr.ttl.as_secs_f64(),
            };
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_entry(req, &entry);
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyEntry {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyCreate — `create`
// =============================================================================

/// Reply token for `create` (combined create + open). Returns the new
/// inode's attributes plus an open file handle.
pub struct ReplyCreate {
    state: InnerState,
    op: &'static str,
}

impl ReplyCreate {
    /// SAFETY: see [`ReplyAttr::from_raw`].
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply with the created entry + open file handle.
    pub fn created(mut self, attr: &FileAttr, generation: u64, fh: u64, fopen_flags: u32) {
        if let Some(req) = self.state.take() {
            let entry = sys::fuse_entry_param {
                ino: attr.ino,
                generation,
                attr: file_attr_to_stat(attr),
                attr_timeout: attr.ttl.as_secs_f64(),
                entry_timeout: attr.ttl.as_secs_f64(),
            };
            let fi = make_fuse_file_info(fh, fopen_flags);
            // SAFETY: see drop_pending. `&entry` and `&fi` are
            // stack-local valid for the call duration.
            unsafe {
                sys::fuse_reply_create(req, &entry, &fi);
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyCreate {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyOpen — `open`, `opendir`
// =============================================================================

/// Reply token for `open` / `opendir`. Returns an open file handle
/// the kernel will pass back on subsequent ops.
pub struct ReplyOpen {
    state: InnerState,
    op: &'static str,
}

impl ReplyOpen {
    /// SAFETY: see [`ReplyAttr::from_raw`].
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply with an open file handle.
    pub fn opened(mut self, fh: u64, fopen_flags: u32) {
        if let Some(req) = self.state.take() {
            let fi = make_fuse_file_info(fh, fopen_flags);
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_open(req, &fi);
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyOpen {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyData — `read`, `readlink`, getxattr/listxattr (data path)
// =============================================================================

/// Reply token for ops that return raw bytes.
pub struct ReplyData {
    state: InnerState,
    op: &'static str,
}

impl ReplyData {
    /// SAFETY: see [`ReplyAttr::from_raw`].
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply with the given byte slice. libfuse copies the bytes
    /// before returning (the slice does not need to outlive the call).
    pub fn data(mut self, bytes: &[u8]) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending. The pointer is valid for
            // `bytes.len()` bytes; libfuse copies.
            #[allow(clippy::cast_possible_wrap)]
            unsafe {
                sys::fuse_reply_buf(req, bytes.as_ptr().cast::<i8>(), bytes.len());
            }
        }
    }

    /// Reply with an empty byte slice — used for short reads at EOF.
    pub fn empty(self) {
        self.data(&[]);
    }

    /// Reply with a symlink target — convenience over `data` that
    /// converts the path to a NUL-terminated C string for
    /// `fuse_reply_readlink`.
    pub fn readlink(mut self, target: &Path) {
        if let Some(req) = self.state.take() {
            // libfuse's fuse_reply_readlink takes a C string.
            // ENOENT-shaped paths shouldn't be empty; if construction
            // fails, fall back to EIO.
            match CString::new(target.as_os_str().as_bytes()) {
                Ok(cstr) => {
                    // SAFETY: cstr lives for the call; libfuse copies.
                    unsafe {
                        sys::fuse_reply_readlink(req, cstr.as_ptr());
                    }
                }
                Err(_) => {
                    // SAFETY: see drop_pending.
                    unsafe {
                        sys::fuse_reply_err(req, libc::EIO);
                    }
                }
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyData {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyWrite — `write`
// =============================================================================

/// Reply token for `write`. Returns how many bytes were actually
/// written (may be less than requested for short writes).
pub struct ReplyWrite {
    state: InnerState,
    op: &'static str,
}

impl ReplyWrite {
    /// SAFETY: see [`ReplyAttr::from_raw`].
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply with the byte count actually written.
    pub fn written(mut self, count: usize) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_write(req, count);
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyWrite {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyEmpty — ops that have no return value
// =============================================================================

/// Reply token for ops that succeed silently (`unlink`, `rmdir`,
/// `rename`, `flush`, `release`, `fsync`, `releasedir`, `setxattr`,
/// `removexattr`, `setlk`). Consume with [`ReplyEmpty::ok`] (success)
/// or [`ReplyEmpty::error`] (failure).
pub struct ReplyEmpty {
    state: InnerState,
    op: &'static str,
}

impl ReplyEmpty {
    /// SAFETY: see [`ReplyAttr::from_raw`].
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply success.
    pub fn ok(mut self) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending. `fuse_reply_err(req, 0)` is
            // libfuse's idiom for a no-content success.
            unsafe {
                sys::fuse_reply_err(req, 0);
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyEmpty {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyDirectory — `readdir`
// =============================================================================

/// Reply token for `readdir`. The user pushes entries via
/// [`ReplyDirectory::add`] up to the requested size; finalize with
/// [`ReplyDirectory::ok`] or [`ReplyDirectory::error`].
pub struct ReplyDirectory {
    state: InnerState,
    op: &'static str,
    max_size: usize,
    buf: Vec<u8>,
}

impl ReplyDirectory {
    /// SAFETY: see [`ReplyAttr::from_raw`]. `max_size` is the kernel-
    /// requested max bytes.
    pub(crate) unsafe fn from_raw(
        req: *mut sys::fuse_req,
        op: &'static str,
        max_size: usize,
    ) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
            max_size,
            buf: Vec::with_capacity(max_size),
        }
    }

    /// Add a directory entry. Returns `true` if the buffer is full
    /// (caller should stop iterating). Returns `false` on success.
    ///
    /// `next_offset` is what the kernel passes back in the subsequent
    /// `readdir` call (the cursor); typical impl uses `index + 1`.
    #[must_use = "true means buffer full; caller should stop pushing entries"]
    pub fn add(&mut self, ino: u64, next_offset: i64, kind: FileType, name: &[u8]) -> bool {
        let req = match &self.state {
            InnerState::Pending(r) => *r,
            InnerState::Consumed => return true,
        };
        let st = stat_for_direntry(ino, kind);
        // SAFETY: req is valid; cstr lifetime is the call duration.
        // Use `fuse_add_direntry` which writes a properly aligned
        // dirent into our buffer and returns the bytes used.
        let cname = match CString::new(name) {
            Ok(s) => s,
            Err(_) => return true,
        };
        let remaining = self.max_size - self.buf.len();
        let mut tmp = vec![0_u8; remaining];
        let n = unsafe {
            sys::fuse_add_direntry(
                req,
                tmp.as_mut_ptr().cast::<i8>(),
                remaining,
                cname.as_ptr(),
                &st,
                next_offset,
            )
        };
        if n > remaining {
            // libfuse reports the size that *would* have been needed.
            // Buffer full — caller must stop.
            return true;
        }
        tmp.truncate(n);
        self.buf.extend_from_slice(&tmp);
        false
    }

    /// Finalize with the accumulated buffer.
    pub fn ok(mut self) {
        if let Some(req) = self.state.take() {
            // SAFETY: buffer is valid for the call; libfuse copies.
            #[allow(clippy::cast_possible_wrap)]
            unsafe {
                sys::fuse_reply_buf(req, self.buf.as_ptr().cast::<i8>(), self.buf.len());
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyDirectory {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyStatfs — `statfs`
// =============================================================================

/// Filesystem stats — what `statfs` returns. Mirrors `struct statvfs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatFs {
    /// Block size (bytes).
    pub bsize: u64,
    /// Fundamental block size (bytes).
    pub frsize: u64,
    /// Total blocks in the filesystem.
    pub blocks: u64,
    /// Free blocks.
    pub bfree: u64,
    /// Free blocks for unprivileged users.
    pub bavail: u64,
    /// Total file nodes.
    pub files: u64,
    /// Free file nodes.
    pub ffree: u64,
    /// Free file nodes for unprivileged users.
    pub favail: u64,
    /// Maximum filename length.
    pub namemax: u64,
}

/// Reply token for `statfs`.
pub struct ReplyStatfs {
    state: InnerState,
    op: &'static str,
}

impl ReplyStatfs {
    /// SAFETY: see [`ReplyAttr::from_raw`].
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply with filesystem stats.
    pub fn stats(mut self, st: &StatFs) {
        if let Some(req) = self.state.take() {
            // libc::statvfs has private fields; build through unsafe-
            // zero-init. The bindgen-generated `statvfs` lets us set
            // all the public fields directly.
            // SAFETY: a fully zero `statvfs` is a valid all-zero
            // record per POSIX; we then overwrite every field we
            // care about. Trailing fields (`f_fsid`, flags, etc.)
            // remain zero, which is the documented "unknown" sentinel.
            let mut v: sys::statvfs = unsafe { std::mem::zeroed() };
            v.f_bsize = st.bsize;
            v.f_frsize = st.frsize;
            v.f_blocks = st.blocks;
            v.f_bfree = st.bfree;
            v.f_bavail = st.bavail;
            v.f_files = st.files;
            v.f_ffree = st.ffree;
            v.f_favail = st.favail;
            v.f_namemax = st.namemax;
            // SAFETY: `&v` valid for the call duration; libfuse copies.
            unsafe {
                sys::fuse_reply_statfs(req, &v);
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyStatfs {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyXattr — `getxattr`/`listxattr` size-query path
// =============================================================================

/// Reply token for `getxattr` / `listxattr` size-query. When the
/// caller passes `size = 0` the kernel just wants the total size of
/// the value (or list); we reply with that count, not the data.
pub struct ReplyXattr {
    state: InnerState,
    op: &'static str,
}

impl ReplyXattr {
    /// SAFETY: see [`ReplyAttr::from_raw`].
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply with the size in bytes the corresponding data-path call
    /// would produce.
    pub fn size(mut self, count: usize) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_xattr(req, count);
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyXattr {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// ReplyLock — `getlk`
// =============================================================================

/// Reply token for `getlk`. Returns either `Unlocked` (no conflicting
/// lock) or the conflicting lock's owner + range.
pub struct ReplyLock {
    state: InnerState,
    op: &'static str,
}

impl ReplyLock {
    /// SAFETY: see [`ReplyAttr::from_raw`].
    pub(crate) unsafe fn from_raw(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            state: InnerState::Pending(req),
            op,
        }
    }

    /// Reply with no conflicting lock.
    ///
    /// libfuse's contract is that `l_type = F_UNLCK` indicates "no
    /// conflict"; the other fields are unused.
    pub fn unlocked(mut self) {
        if let Some(req) = self.state.take() {
            let lock = flock_ext::build(libc::F_UNLCK, libc::SEEK_SET, 0, 0, 0);
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_lock(req, &lock);
            }
        }
    }

    /// Reply with a conflicting lock.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_possible_wrap)]
    pub fn conflict(mut self, lk_type: i32, start: u64, end: u64, pid: u32) {
        if let Some(req) = self.state.take() {
            let len = if end >= start {
                (end - start) as i64
            } else {
                0
            };
            let lock = flock_ext::build(lk_type, libc::SEEK_SET, start as i64, len, pid as i32);
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_lock(req, &lock);
            }
        }
    }

    /// Reply with an error.
    pub fn error(mut self, err: FuseError) {
        if let Some(req) = self.state.take() {
            // SAFETY: see drop_pending.
            unsafe {
                sys::fuse_reply_err(req, err.to_errno());
            }
        }
    }
}

impl Drop for ReplyLock {
    fn drop(&mut self) {
        drop_pending(&mut self.state, self.op);
    }
}

// =============================================================================
// Helpers — internal conversion between kiseki types and bindgen C structs
// =============================================================================

#[allow(clippy::cast_possible_truncation)]
fn file_attr_to_stat(attr: &FileAttr) -> sys::stat {
    // SAFETY: `stat` is a plain-old-data C struct; zero-init is valid.
    // We then fill every field we care about from `attr`.
    let mut st: sys::stat = unsafe { std::mem::zeroed() };
    st.st_ino = attr.ino;
    st.st_size = attr.size as i64;
    st.st_blocks = attr.blocks as i64;
    st.st_blksize = attr.blksize as i64;
    st.st_nlink = u64::from(attr.nlink);
    st.st_mode = attr.kind.to_mode_bits() | u32::from(attr.perm);
    st.st_uid = attr.uid;
    st.st_gid = attr.gid;
    st.st_rdev = u64::from(attr.rdev);
    let (atime_s, atime_ns) = system_time_parts(attr.atime);
    let (mtime_s, mtime_ns) = system_time_parts(attr.mtime);
    let (ctime_s, ctime_ns) = system_time_parts(attr.ctime);
    st.st_atim.tv_sec = atime_s;
    st.st_atim.tv_nsec = atime_ns;
    st.st_mtim.tv_sec = mtime_s;
    st.st_mtim.tv_nsec = mtime_ns;
    st.st_ctim.tv_sec = ctime_s;
    st.st_ctim.tv_nsec = ctime_ns;
    st
}

fn stat_for_direntry(ino: u64, kind: FileType) -> sys::stat {
    // SAFETY: zero-init `stat` then fill the two fields readdir uses
    // (ino, mode-with-type-bits). libfuse only reads st_ino +
    // (st_mode & S_IFMT) from the dirent's stat.
    let mut st: sys::stat = unsafe { std::mem::zeroed() };
    st.st_ino = ino;
    st.st_mode = kind.to_mode_bits();
    st
}

#[allow(clippy::cast_possible_wrap)]
fn system_time_parts(t: std::time::SystemTime) -> (i64, i64) {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, i64::from(d.subsec_nanos())),
        Err(_) => (0, 0),
    }
}

fn make_fuse_file_info(fh: u64, flags: u32) -> sys::fuse_file_info {
    // SAFETY: zero-init `fuse_file_info` is the libfuse idiom — bit
    // fields default to 0.
    let mut fi: sys::fuse_file_info = unsafe { std::mem::zeroed() };
    fi.fh = fh;
    // The `flags` member in `fuse_file_info` is `i32` (open flags
    // sign-extended from kernel int).
    #[allow(clippy::cast_possible_wrap)]
    {
        fi.flags = flags as i32;
    }
    fi
}

// =============================================================================
// Reply token trait — what the bridge accepts
// =============================================================================

/// Trait-object handle the [`Bridge`](crate::bridge::Bridge) uses to
/// move a reply token between threads and finalize on the bridge's
/// own thread. Implemented by every `Reply*` type in this module.
///
/// The trait is sealed: external types can't implement it. This
/// keeps the I-FUSE-3 contract — the bridge is the *only* way to
/// move a reply across threads — auditable.
mod sealed {
    pub trait Sealed {}
}

/// The bridge-acceptable reply types.
#[allow(private_bounds)]
pub trait ReplyToken: sealed::Sealed + Sized {
    /// The success payload type for this reply (e.g., `FileAttr`,
    /// `Vec<u8>`, `()`).
    type Output: Send + 'static;

    /// Internal: hand off the libfuse request pointer + op name to
    /// the bridge, consuming the token without finalizing it. The
    /// returned [`BridgedReply`](crate::bridge::BridgedReply) is
    /// `Send`.
    #[doc(hidden)]
    fn into_bridged(self) -> crate::bridge::BridgedReply;

    /// Internal: finalize a bridged reply with the result, called by
    /// the bridge thread after recv/timeout.
    #[doc(hidden)]
    fn finalize_bridged(
        bridged: crate::bridge::BridgedReply,
        result: Result<Self::Output, FuseError>,
    );
}

// Macro to wire each Reply* into the bridge with the right
// finalize_bridged shape. The output type for each is what the
// success arm of the consume method takes. The `$req` ident is
// macro-bound at the call site and accessible in `$finalize_ok` —
// without naming it in the macro pattern, hygiene would block the
// closure body's reference.
macro_rules! impl_reply_token {
    ($ty:ident, $output:ty, |$req:ident, $value:ident| $finalize_ok:block) => {
        impl sealed::Sealed for $ty {}
        impl ReplyToken for $ty {
            type Output = $output;

            #[doc(hidden)]
            fn into_bridged(mut self) -> crate::bridge::BridgedReply {
                let req = match self.state.take() {
                    Some(r) => r,
                    None => {
                        return crate::bridge::BridgedReply::dummy(self.op);
                    }
                };
                let op = self.op;
                std::mem::forget(self); // bypass our Drop; bridge owns finalization now.
                                        // SAFETY: the bridged-reply newtype takes the raw
                                        // pointer + op name; the only consumer is the bridge
                                        // thread's finalize call (which sees the same pointer
                                        // and op).
                unsafe { crate::bridge::BridgedReply::new(req, op) }
            }

            #[doc(hidden)]
            fn finalize_bridged(
                bridged: crate::bridge::BridgedReply,
                result: Result<Self::Output, FuseError>,
            ) {
                let ($req, _op) = match bridged.into_raw() {
                    Some(parts) => parts,
                    None => return,
                };
                match result {
                    Ok($value) => $finalize_ok,
                    Err(err) => {
                        // SAFETY: the bridge holds the only reference
                        // to req; finalize via fuse_reply_err.
                        unsafe {
                            sys::fuse_reply_err($req, err.to_errno());
                        }
                    }
                }
            }
        }
    };
}

impl_reply_token!(ReplyAttr, FileAttr, |req, attr| {
    let st = file_attr_to_stat(&attr);
    // SAFETY: see ReplyAttr::attr — `&st` valid for the call.
    unsafe {
        sys::fuse_reply_attr(req, &st, attr.ttl.as_secs_f64());
    }
});

impl_reply_token!(ReplyEmpty, (), |req, _v| {
    // SAFETY: see ReplyEmpty::ok.
    unsafe {
        sys::fuse_reply_err(req, 0);
    }
});

impl_reply_token!(ReplyData, Vec<u8>, |req, bytes| {
    #[allow(clippy::cast_possible_wrap)]
    // SAFETY: see ReplyData::data — bytes valid for call duration.
    unsafe {
        sys::fuse_reply_buf(req, bytes.as_ptr().cast::<i8>(), bytes.len());
    }
});

impl_reply_token!(ReplyWrite, usize, |req, count| {
    // SAFETY: see ReplyWrite::written.
    unsafe {
        sys::fuse_reply_write(req, count);
    }
});

// =============================================================================
// Re-export point so types.rs's `FlockExt` can be visible without
// circular dep.
// =============================================================================
