//! `extern "C"` trampolines that dispatch libfuse callbacks into the
//! user's [`Filesystem`](crate::filesystem::Filesystem) impl.
//!
//! Each trampoline:
//! 1. Recovers `&MountState` from `fuse_req_userdata(req)`.
//! 2. Builds a [`Request`] from `fuse_req_ctx(req)` + a fresh
//!    [`RequestId`].
//! 3. Builds the appropriate `Reply*` token wrapping `req`.
//! 4. Dispatches into the trait method.
//!
//! The trait method is called synchronously from the libfuse session
//! thread; if the implementation wants async finalization it routes
//! through [`Bridge::spawn`](crate::bridge::Bridge::spawn).

use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use kiseki_fuse_sys as sys;

use crate::filesystem::{GetXattrReply, ListXattrReply};
use crate::reply::{
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
};
use crate::request::{Request, RequestId};
use crate::session::MountState;
use crate::types::{LockOwner, SetAttrRequest, SetAttrValid};

/// Build the static `fuse_lowlevel_ops` dispatch table.
///
/// Wired ops are populated; unwired (per ADR-013 §"Out of scope"
/// or pending escalation, e.g., `syncfs`) are left `None` so libfuse
/// returns `ENOSYS` to the kernel.
#[must_use]
pub(crate) fn ops_table() -> sys::fuse_lowlevel_ops {
    // SAFETY: zero-init'd `fuse_lowlevel_ops` is the libfuse-documented
    // way to start; every field is `Option<unsafe extern "C" fn(...)>`,
    // for which `None` is a valid bit pattern (zero).
    let mut ops: sys::fuse_lowlevel_ops = unsafe { std::mem::zeroed() };
    ops.init = Some(t_init);
    ops.destroy = Some(t_destroy);
    ops.lookup = Some(t_lookup);
    ops.forget = Some(t_forget);
    ops.getattr = Some(t_getattr);
    ops.setattr = Some(t_setattr);
    ops.readlink = Some(t_readlink);
    ops.mkdir = Some(t_mkdir);
    ops.unlink = Some(t_unlink);
    ops.rmdir = Some(t_rmdir);
    ops.symlink = Some(t_symlink);
    ops.rename = Some(t_rename);
    ops.open = Some(t_open);
    ops.read = Some(t_read);
    ops.write = Some(t_write);
    ops.flush = Some(t_flush);
    ops.release = Some(t_release);
    ops.fsync = Some(t_fsync);
    ops.opendir = Some(t_opendir);
    ops.readdir = Some(t_readdir);
    ops.releasedir = Some(t_releasedir);
    ops.statfs = Some(t_statfs);
    ops.setxattr = Some(t_setxattr);
    ops.getxattr = Some(t_getxattr);
    ops.listxattr = Some(t_listxattr);
    ops.removexattr = Some(t_removexattr);
    ops.create = Some(t_create);
    ops.getlk = Some(t_getlk);
    ops.setlk = Some(t_setlk);
    ops
}

// =============================================================================
// Common dispatch helper
// =============================================================================

/// Recover the `&MountState` + `Request` and build an `OpContext`.
///
/// SAFETY: `req` must be the libfuse-issued request handle for an op
/// dispatched through the table built by [`ops_table`]. The returned
/// state reference is valid for the duration of the trampoline call.
unsafe fn dispatch_prep(
    req: sys::fuse_req_t,
) -> (&'static MountState, crate::filesystem::OpContext) {
    // SAFETY: fuse_req_userdata returns the userdata set at
    // fuse_session_new_versioned; we set it to `&*Box<MountState>`.
    // The Session's Drop runs after the loop exits, so userdata is
    // valid for every dispatch.
    let userdata = unsafe { sys::fuse_req_userdata(req) };
    let state: &MountState = unsafe { &*(userdata.cast::<MountState>()) };

    // SAFETY: fuse_req_ctx returns a valid pointer for the request's
    // lifetime; copying the four fields out keeps no reference past
    // the trampoline.
    let ctx_ptr = unsafe { sys::fuse_req_ctx(req) };
    let ctx = unsafe { *ctx_ptr };
    let request = Request {
        id: RequestId::next(),
        caller_uid: ctx.uid,
        caller_gid: ctx.gid,
        #[allow(clippy::cast_sign_loss)]
        caller_pid: ctx.pid as u32,
        umask: ctx.umask,
    };
    let op_ctx = crate::filesystem::OpContext {
        request,
        bridge: state.bridge.clone(),
    };
    (state, op_ctx)
}

/// Convert a `*const c_char` name from libfuse to an OsStr.
///
/// SAFETY: `name` is a valid NUL-terminated C string — libfuse
/// hands us kernel-supplied directory entry names that are NUL-
/// terminated and live for the duration of the trampoline call.
unsafe fn cname_to_osstr<'a>(name: *const ::core::ffi::c_char) -> &'a OsStr {
    // SAFETY: see fn doc.
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    OsStr::from_bytes(bytes)
}

// =============================================================================
// init / destroy — informational, no reply expected
// =============================================================================

unsafe extern "C" fn t_init(_userdata: *mut ::core::ffi::c_void, _conn: *mut sys::fuse_conn_info) {
    // No reply token. The conn parameter could be tuned (max_readahead,
    // capabilities); leaving defaults for Phase 1b — Phase 2 will
    // configure FOPEN_KEEP_CACHE / FUSE_EXPORT_SUPPORT here.
}

unsafe extern "C" fn t_destroy(_userdata: *mut ::core::ffi::c_void) {
    // userdata is &MountState; do not free it here — Session::Drop
    // owns the Box.
}

// =============================================================================
// lookup, forget, getattr, setattr
// =============================================================================

unsafe extern "C" fn t_lookup(
    req: sys::fuse_req_t,
    parent: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
) {
    // SAFETY: see dispatch_prep / cname_to_osstr / ReplyEntry::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let reply = unsafe { ReplyEntry::from_raw(req, "lookup") };
    let nm = unsafe { cname_to_osstr(name) };
    state.fs.lookup(&ctx, parent, nm, reply);
}

unsafe extern "C" fn t_forget(req: sys::fuse_req_t, ino: sys::fuse_ino_t, nlookup: u64) {
    // forget has no reply; libfuse handles `fuse_reply_none` for us.
    // SAFETY: dispatch_prep.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    state.fs.forget(&ctx, ino, nlookup);
    // SAFETY: fuse_reply_none is the documented finalize for forget.
    unsafe {
        sys::fuse_reply_none(req);
    }
}

unsafe extern "C" fn t_getattr(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    _fi: *mut sys::fuse_file_info,
) {
    // SAFETY: see dispatch_prep / ReplyAttr::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let reply = unsafe { ReplyAttr::from_raw(req, "getattr") };
    state.fs.getattr(&ctx, ino, reply);
}

unsafe extern "C" fn t_setattr(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    attr: *mut sys::stat,
    to_set: ::core::ffi::c_int,
    _fi: *mut sys::fuse_file_info,
) {
    // SAFETY: see dispatch_prep.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    // SAFETY: attr is valid for the call duration per libfuse contract.
    let st = unsafe { &*attr };
    let valid_bits = to_set as u32;
    let valid = SetAttrValid {
        // FATTR_* bit positions per libfuse fuse_lowlevel.h:
        mode: (valid_bits & sys::FUSE_SET_ATTR_MODE) != 0,
        uid: (valid_bits & sys::FUSE_SET_ATTR_UID) != 0,
        gid: (valid_bits & sys::FUSE_SET_ATTR_GID) != 0,
        size: (valid_bits & sys::FUSE_SET_ATTR_SIZE) != 0,
        atime: (valid_bits & sys::FUSE_SET_ATTR_ATIME) != 0,
        mtime: (valid_bits & sys::FUSE_SET_ATTR_MTIME) != 0,
        ctime: (valid_bits & sys::FUSE_SET_ATTR_CTIME) != 0,
    };
    #[allow(clippy::cast_sign_loss)]
    let req_attr = SetAttrRequest {
        valid,
        mode: st.st_mode,
        uid: st.st_uid,
        gid: st.st_gid,
        size: st.st_size as u64,
        atime: stat_to_systime(st.st_atim.tv_sec, st.st_atim.tv_nsec),
        mtime: stat_to_systime(st.st_mtim.tv_sec, st.st_mtim.tv_nsec),
        ctime: stat_to_systime(st.st_ctim.tv_sec, st.st_ctim.tv_nsec),
    };
    let reply = unsafe { ReplyAttr::from_raw(req, "setattr") };
    state.fs.setattr(&ctx, ino, req_attr, reply);
}

// =============================================================================
// readlink, mkdir, unlink, rmdir, symlink, rename
// =============================================================================

unsafe extern "C" fn t_readlink(req: sys::fuse_req_t, ino: sys::fuse_ino_t) {
    // SAFETY: dispatch_prep / ReplyData::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let reply = unsafe { ReplyData::from_raw(req, "readlink") };
    state.fs.readlink(&ctx, ino, reply);
}

unsafe extern "C" fn t_mkdir(
    req: sys::fuse_req_t,
    parent: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
    mode: sys::mode_t,
) {
    // SAFETY: dispatch_prep / cname_to_osstr / ReplyEntry::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let nm = unsafe { cname_to_osstr(name) };
    let reply = unsafe { ReplyEntry::from_raw(req, "mkdir") };
    // libfuse low-level passes umask=0 (high-level only); see fuse_lowlevel_ops.
    state.fs.mkdir(&ctx, parent, nm, mode, 0, reply);
}

unsafe extern "C" fn t_unlink(
    req: sys::fuse_req_t,
    parent: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
) {
    // SAFETY: dispatch_prep / cname_to_osstr / ReplyEmpty::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let nm = unsafe { cname_to_osstr(name) };
    let reply = unsafe { ReplyEmpty::from_raw(req, "unlink") };
    state.fs.unlink(&ctx, parent, nm, reply);
}

unsafe extern "C" fn t_rmdir(
    req: sys::fuse_req_t,
    parent: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
) {
    // SAFETY: dispatch_prep / cname_to_osstr / ReplyEmpty::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let nm = unsafe { cname_to_osstr(name) };
    let reply = unsafe { ReplyEmpty::from_raw(req, "rmdir") };
    state.fs.rmdir(&ctx, parent, nm, reply);
}

unsafe extern "C" fn t_symlink(
    req: sys::fuse_req_t,
    link: *const ::core::ffi::c_char,
    parent: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
) {
    // SAFETY: dispatch_prep / cname_to_osstr / ReplyEntry::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let nm = unsafe { cname_to_osstr(name) };
    let link_bytes = unsafe { CStr::from_ptr(link) }.to_bytes();
    let link_path = Path::new(OsStr::from_bytes(link_bytes));
    let reply = unsafe { ReplyEntry::from_raw(req, "symlink") };
    state.fs.symlink(&ctx, parent, nm, link_path, reply);
}

unsafe extern "C" fn t_rename(
    req: sys::fuse_req_t,
    parent: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
    newparent: sys::fuse_ino_t,
    newname: *const ::core::ffi::c_char,
    flags: ::core::ffi::c_uint,
) {
    // SAFETY: dispatch_prep / cname_to_osstr / ReplyEmpty::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let nm = unsafe { cname_to_osstr(name) };
    let newnm = unsafe { cname_to_osstr(newname) };
    let reply = unsafe { ReplyEmpty::from_raw(req, "rename") };
    state
        .fs
        .rename(&ctx, parent, nm, newparent, newnm, flags, reply);
}

// =============================================================================
// open, read, write, flush, release, fsync
// =============================================================================

unsafe extern "C" fn t_open(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / ReplyOpen::from_raw / fi non-null per libfuse.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let flags = unsafe { (*fi).flags };
    let reply = unsafe { ReplyOpen::from_raw(req, "open") };
    state.fs.open(&ctx, ino, flags, reply);
}

unsafe extern "C" fn t_read(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    size: usize,
    off: sys::off_t,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / ReplyData::from_raw / fi non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let fh = unsafe { (*fi).fh };
    let reply = unsafe { ReplyData::from_raw(req, "read") };
    #[allow(clippy::cast_possible_truncation)]
    state.fs.read(&ctx, ino, fh, off, size as u32, reply);
}

unsafe extern "C" fn t_write(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    buf: *const ::core::ffi::c_char,
    size: usize,
    off: sys::off_t,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / ReplyWrite::from_raw / fi non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let fh = unsafe { (*fi).fh };
    // SAFETY: buf valid for size bytes per libfuse contract.
    let bytes = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), size) };
    let reply = unsafe { ReplyWrite::from_raw(req, "write") };
    let lock_owner = unsafe { (*fi).lock_owner };
    let lo = if lock_owner == 0 {
        None
    } else {
        Some(LockOwner(lock_owner))
    };
    state.fs.write(&ctx, ino, fh, off, bytes, 0, lo, reply);
}

unsafe extern "C" fn t_flush(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / ReplyEmpty::from_raw / fi non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let fh = unsafe { (*fi).fh };
    let lock_owner = LockOwner(unsafe { (*fi).lock_owner });
    let reply = unsafe { ReplyEmpty::from_raw(req, "flush") };
    state.fs.flush(&ctx, ino, fh, lock_owner, reply);
}

unsafe extern "C" fn t_release(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / ReplyEmpty::from_raw / fi non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let fh = unsafe { (*fi).fh };
    let flags = unsafe { (*fi).flags };
    let lock_owner = unsafe { (*fi).lock_owner };
    let lo = if lock_owner == 0 {
        None
    } else {
        Some(LockOwner(lock_owner))
    };
    let reply = unsafe { ReplyEmpty::from_raw(req, "release") };
    // The `flush` field on fuse_file_info is a bit; treat any nonzero.
    let flush_bit = unsafe { (*fi).flush() != 0 };
    state.fs.release(&ctx, ino, fh, flags, lo, flush_bit, reply);
}

unsafe extern "C" fn t_fsync(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    datasync: ::core::ffi::c_int,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / ReplyEmpty::from_raw / fi non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let fh = unsafe { (*fi).fh };
    let reply = unsafe { ReplyEmpty::from_raw(req, "fsync") };
    state.fs.fsync(&ctx, ino, fh, datasync != 0, reply);
}

// =============================================================================
// opendir, readdir, releasedir
// =============================================================================

unsafe extern "C" fn t_opendir(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / ReplyOpen::from_raw / fi non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let flags = unsafe { (*fi).flags };
    let reply = unsafe { ReplyOpen::from_raw(req, "opendir") };
    state.fs.opendir(&ctx, ino, flags, reply);
}

unsafe extern "C" fn t_readdir(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    size: usize,
    off: sys::off_t,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / ReplyDirectory::from_raw / fi non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let fh = unsafe { (*fi).fh };
    let reply = unsafe { ReplyDirectory::from_raw(req, "readdir", size) };
    state.fs.readdir(&ctx, ino, fh, off, reply);
}

unsafe extern "C" fn t_releasedir(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / ReplyEmpty::from_raw / fi non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let fh = unsafe { (*fi).fh };
    let flags = unsafe { (*fi).flags };
    let reply = unsafe { ReplyEmpty::from_raw(req, "releasedir") };
    state.fs.releasedir(&ctx, ino, fh, flags, reply);
}

// =============================================================================
// statfs
// =============================================================================

unsafe extern "C" fn t_statfs(req: sys::fuse_req_t, ino: sys::fuse_ino_t) {
    // SAFETY: dispatch_prep / ReplyStatfs::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let reply = unsafe { ReplyStatfs::from_raw(req, "statfs") };
    state.fs.statfs(&ctx, ino, reply);
}

// =============================================================================
// xattr quartet
// =============================================================================

unsafe extern "C" fn t_setxattr(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
    value: *const ::core::ffi::c_char,
    size: usize,
    flags: ::core::ffi::c_int,
) {
    // SAFETY: dispatch_prep / cname_to_osstr / value valid for size.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let nm = unsafe { cname_to_osstr(name) };
    let bytes = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), size) };
    let reply = unsafe { ReplyEmpty::from_raw(req, "setxattr") };
    state.fs.setxattr(&ctx, ino, nm, bytes, flags, 0, reply);
}

unsafe extern "C" fn t_getxattr(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
    size: usize,
) {
    // SAFETY: dispatch_prep / cname_to_osstr.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let nm = unsafe { cname_to_osstr(name) };
    #[allow(clippy::cast_possible_truncation)]
    let size_u32 = size as u32;
    let reply = if size == 0 {
        let r = unsafe { ReplyXattr::from_raw(req, "getxattr") };
        GetXattrReply::from_size(r)
    } else {
        let r = unsafe { ReplyData::from_raw(req, "getxattr") };
        GetXattrReply::from_data(r)
    };
    state.fs.getxattr(&ctx, ino, nm, size_u32, reply);
}

unsafe extern "C" fn t_listxattr(req: sys::fuse_req_t, ino: sys::fuse_ino_t, size: usize) {
    // SAFETY: dispatch_prep.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    #[allow(clippy::cast_possible_truncation)]
    let size_u32 = size as u32;
    let reply = if size == 0 {
        let r = unsafe { ReplyXattr::from_raw(req, "listxattr") };
        ListXattrReply::from_size(r)
    } else {
        let r = unsafe { ReplyData::from_raw(req, "listxattr") };
        ListXattrReply::from_data(r)
    };
    state.fs.listxattr(&ctx, ino, size_u32, reply);
}

unsafe extern "C" fn t_removexattr(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
) {
    // SAFETY: dispatch_prep / cname_to_osstr.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let nm = unsafe { cname_to_osstr(name) };
    let reply = unsafe { ReplyEmpty::from_raw(req, "removexattr") };
    state.fs.removexattr(&ctx, ino, nm, reply);
}

// =============================================================================
// create
// =============================================================================

unsafe extern "C" fn t_create(
    req: sys::fuse_req_t,
    parent: sys::fuse_ino_t,
    name: *const ::core::ffi::c_char,
    mode: sys::mode_t,
    fi: *mut sys::fuse_file_info,
) {
    // SAFETY: dispatch_prep / cname_to_osstr / ReplyCreate::from_raw.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let nm = unsafe { cname_to_osstr(name) };
    let flags = unsafe { (*fi).flags };
    let reply = unsafe { ReplyCreate::from_raw(req, "create") };
    // libfuse low-level doesn't expose umask; pass 0.
    state.fs.create(&ctx, parent, nm, mode, 0, flags, reply);
}

// =============================================================================
// getlk, setlk
// =============================================================================

unsafe extern "C" fn t_getlk(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    fi: *mut sys::fuse_file_info,
    lock: *mut sys::flock,
) {
    // SAFETY: dispatch_prep / ReplyLock::from_raw / fi & lock non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let fh = unsafe { (*fi).fh };
    let lock_owner = LockOwner(unsafe { (*fi).lock_owner });
    let lk = unsafe { *lock };
    let reply = unsafe { ReplyLock::from_raw(req, "getlk") };
    let (start, end) = lock_range(&lk);
    state.fs.getlk(
        &ctx,
        ino,
        fh,
        lock_owner,
        start,
        end,
        i32::from(lk.l_type),
        #[allow(clippy::cast_sign_loss)]
        {
            lk.l_pid as u32
        },
        reply,
    );
}

unsafe extern "C" fn t_setlk(
    req: sys::fuse_req_t,
    ino: sys::fuse_ino_t,
    fi: *mut sys::fuse_file_info,
    lock: *mut sys::flock,
    sleep: ::core::ffi::c_int,
) {
    // SAFETY: dispatch_prep / ReplyEmpty::from_raw / fi & lock non-null.
    let (state, ctx) = unsafe { dispatch_prep(req) };
    let fh = unsafe { (*fi).fh };
    let lock_owner = LockOwner(unsafe { (*fi).lock_owner });
    let lk = unsafe { *lock };
    let reply = unsafe { ReplyEmpty::from_raw(req, "setlk") };
    let (start, end) = lock_range(&lk);
    state.fs.setlk(
        &ctx,
        ino,
        fh,
        lock_owner,
        start,
        end,
        i32::from(lk.l_type),
        #[allow(clippy::cast_sign_loss)]
        {
            lk.l_pid as u32
        },
        sleep != 0,
        reply,
    );
}

// =============================================================================
// helpers
// =============================================================================

#[allow(clippy::cast_sign_loss)]
fn lock_range(lk: &sys::flock) -> (u64, u64) {
    let start = lk.l_start as u64;
    let len = if lk.l_len <= 0 {
        u64::MAX - start
    } else {
        lk.l_len as u64
    };
    let end = start.saturating_add(len);
    (start, end)
}

#[allow(clippy::cast_sign_loss)]
fn stat_to_systime(secs: i64, nanos: i64) -> Option<std::time::SystemTime> {
    if secs < 0 {
        return None;
    }
    let dur = std::time::Duration::new(secs as u64, nanos as u32);
    Some(std::time::UNIX_EPOCH + dur)
}
