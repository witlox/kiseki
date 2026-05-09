//! Session lifecycle — `fuse_session_new_versioned` / `_mount` /
//! `_loop_mt_31` / `_unmount` / `_destroy`, plus the dedicated
//! `kiseki-fuse-session` `std::thread` (I-FUSE-5) and the
//! crash-detection logic (I-FUSE-8).

use std::ffi::CString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use kiseki_fuse_sys as sys;

use crate::bridge::Bridge;
use crate::error::FuseError;
use crate::filesystem::Filesystem;

/// Userdata stored on the libfuse session and threaded through every
/// trampoline via `fuse_req_userdata(req)`.
pub(crate) struct MountState {
    pub fs: Arc<dyn Filesystem>,
    pub bridge: Bridge,
}

/// Opaque pointer to a libfuse session — sent across threads via the
/// dedicated session-thread spawn.
struct SessionPtr(*mut sys::fuse_session);

// SAFETY: a `*mut fuse_session` is the libfuse session handle.
// libfuse's session API is documented to be safe to operate from
// any thread provided no two threads call `fuse_session_loop*` on
// the same session concurrently. We spawn exactly one session-thread
// per session; the caller of `Session::run_on_dedicated_thread` does
// not also drive the loop.
unsafe impl Send for SessionPtr {}

/// Build + own a libfuse session.
pub(crate) struct Session {
    ptr: SessionPtr,
    /// Kept alive for the session's lifetime; libfuse stores
    /// `&MountState as *mut c_void` and dereferences it from
    /// trampolines.
    _state: Box<MountState>,
    mountpoint: CString,
    args: SessionArgs,
    /// Set when the session has unmounted, so Drop is idempotent.
    unmounted: AtomicBool,
}

struct SessionArgs {
    /// `fuse_args` whose `argv` points at owned memory we keep here.
    raw: sys::fuse_args,
    _argv_storage: Vec<CString>,
    _argv_pointers: Vec<*mut ::core::ffi::c_char>,
}

// SAFETY: SessionArgs holds raw pointers into `_argv_storage`. The
// storage is never re-allocated (we don't push after construction)
// and the pointers don't escape the session's lifetime.
unsafe impl Send for SessionArgs {}

impl Session {
    /// Create + mount a libfuse session bound to a [`Filesystem`]
    /// implementation and a [`Bridge`].
    ///
    /// Wires the [`crate::trampolines`] dispatch table as
    /// `fuse_lowlevel_ops` and stashes `MountState` as session
    /// userdata.
    ///
    /// # Errors
    ///
    /// Returns [`FuseError::Errno`] with a libc errno on:
    /// - `fuse_session_new_versioned` failure (`null` session pointer).
    /// - `fuse_session_mount` failure (negative return code).
    pub(crate) fn create(
        fs: Arc<dyn Filesystem>,
        bridge: Bridge,
        mountpoint: &Path,
        mount_options: &[String],
    ) -> Result<Self, FuseError> {
        // Build fuse_args. argv[0] is conventionally the program
        // name; libfuse uses it for diagnostic prints.
        let mut argv_storage = Vec::with_capacity(mount_options.len() + 1);
        argv_storage.push(
            CString::new("kiseki-fuse").expect("kiseki-fuse: CString from literal cannot fail"),
        );
        for opt in mount_options {
            // -o opt formatting: each user-supplied option goes in
            // verbatim; if they want "-o" prefix they include it
            // themselves.
            argv_storage
                .push(CString::new(opt.clone()).map_err(|_| FuseError::Errno(libc::EINVAL))?);
        }
        let mut argv_pointers: Vec<*mut ::core::ffi::c_char> =
            argv_storage.iter().map(|s| s.as_ptr().cast_mut()).collect();
        // libfuse's API takes argc:i32 + argv:*mut *mut c_char.
        #[allow(clippy::cast_possible_truncation)]
        #[allow(clippy::cast_possible_wrap)]
        let argc = argv_pointers.len() as i32;
        let raw = sys::fuse_args {
            argc,
            argv: argv_pointers.as_mut_ptr(),
            allocated: 0,
        };
        let args = SessionArgs {
            raw,
            _argv_storage: argv_storage,
            _argv_pointers: argv_pointers,
        };

        // Build the userdata pointer; cast to `*mut c_void` for
        // libfuse, but keep the Box alive in the Session struct so
        // it doesn't drop until the session does.
        let state = Box::new(MountState { fs, bridge });
        let userdata_ptr = std::ptr::addr_of!(*state)
            .cast::<::core::ffi::c_void>()
            .cast_mut();

        let ops = crate::trampolines::ops_table();

        // SAFETY: kiseki-fuse-sys exposes `fuse_session_new` (the
        // pre-3.17 ABI, tagged `@@FUSE_3.0` and exported by every
        // libfuse 3.x release). Using the versioned variant would
        // require libfuse ≥ 3.17 at runtime — which Ubuntu 24.04 LTS
        // (3.14.0) does not ship. `args.raw` lives until the session
        // does (kept in `args._argv_storage`); `userdata_ptr` points
        // at `*state` which is kept by the returned struct.
        let mut args_owned = args;
        let session = unsafe {
            sys::fuse_session_new(
                std::ptr::addr_of_mut!(args_owned.raw),
                std::ptr::addr_of!(ops),
                std::mem::size_of::<sys::fuse_lowlevel_ops>(),
                userdata_ptr,
            )
        };
        if session.is_null() {
            return Err(FuseError::Errno(libc::EIO));
        }

        let mountpoint_c = CString::new(mountpoint.to_string_lossy().as_bytes())
            .map_err(|_| FuseError::Errno(libc::EINVAL))?;

        // SAFETY: session is non-null per the check above. The C
        // string outlives the call.
        let rc = unsafe { sys::fuse_session_mount(session, mountpoint_c.as_ptr()) };
        if rc != 0 {
            // Tear down the session before returning the error.
            // SAFETY: session is non-null and not yet mounted; destroy is safe.
            unsafe {
                sys::fuse_session_destroy(session);
            }
            // libfuse returns a non-zero on mount failure; the precise
            // errno is platform-dependent. Map to EIO as a fallback.
            return Err(FuseError::Errno(libc::EIO));
        }

        Ok(Self {
            ptr: SessionPtr(session),
            _state: state,
            mountpoint: mountpoint_c,
            args: args_owned,
            unmounted: AtomicBool::new(false),
        })
    }

    /// Drive `fuse_session_loop_mt_31` on a dedicated `std::thread`
    /// named `kiseki-fuse-session` (I-FUSE-5).
    ///
    /// Returns a [`SessionThread`] that owns the join handle and the
    /// crash-detection bookkeeping. Caller can `.join()` to wait for
    /// graceful unmount, or rely on the watchdog (see
    /// [`SessionThread::with_watchdog`]) for I-FUSE-8 abort on
    /// crash.
    pub(crate) fn run_on_dedicated_thread(self) -> Result<SessionThread, FuseError> {
        // Move `self` into the thread; the thread owns the session
        // for its full lifetime and the cleanup.
        let session_arc = Arc::new(SessionInner::from_session(self));
        let weak = Arc::downgrade(&session_arc);
        let join = std::thread::Builder::new()
            .name("kiseki-fuse-session".into())
            .spawn(move || {
                // Take exclusive ownership for the loop call; on
                // exit, the inner is dropped which destroys the
                // session.
                let inner = session_arc;
                // SAFETY: we hold the Arc; ptr lives until inner drops.
                let rc = unsafe {
                    sys::fuse_session_loop_mt_31(inner.ptr().0, /*clone_fd*/ 1)
                };
                rc
            })
            .map_err(|_| FuseError::Errno(libc::EAGAIN))?;
        Ok(SessionThread {
            handle: Some(join),
            session: weak,
        })
    }

    #[allow(dead_code)] // public API surface for future Session-state inspection.
    fn ptr(&self) -> &SessionPtr {
        &self.ptr
    }

    /// Signal the session loop to exit; safe to call from any
    /// thread.
    #[allow(dead_code)] // public API surface; today the watchdog is the only caller.
    pub(crate) fn exit(&self) {
        // SAFETY: fuse_session_exit is documented as thread-safe and
        // valid against a session for which loop_* is still running
        // OR has already returned.
        unsafe { sys::fuse_session_exit(self.ptr.0) }
    }

    /// Unmount the session. Idempotent.
    pub(crate) fn unmount(&self) {
        if self
            .unmounted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // SAFETY: session is non-null and was successfully mounted.
            unsafe { sys::fuse_session_unmount(self.ptr.0) }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Ensure unmount happens at most once; destroy releases the
        // session itself.
        self.unmount();
        // SAFETY: session is non-null; destroy is the documented
        // cleanup partner of fuse_session_new_versioned.
        unsafe { sys::fuse_session_destroy(self.ptr.0) }
        // args_owned + state Box drop here.
        let _ = &self.args;
        let _ = &self.mountpoint;
    }
}

/// `Send + Sync` wrapper around a `Session` that the dedicated
/// session thread holds while it loops. The Drop runs on the
/// thread's own context; `unmount` + `destroy` are thread-safe per
/// libfuse docs.
struct SessionInner {
    session: parking_lot_lite::Mu<Option<Session>>,
}

impl SessionInner {
    fn from_session(s: Session) -> Self {
        Self {
            session: parking_lot_lite::Mu::new(Some(s)),
        }
    }

    fn ptr(&self) -> SessionPtr {
        // Read-only fast-path access to the raw session pointer for
        // the loop call.
        let g = self.session.read();
        let s = g.as_ref().expect("session present until destroyed");
        SessionPtr(s.ptr.0)
    }
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        // Ensure the inner Session drops here, after the loop has
        // returned. Session's Drop handles unmount + destroy.
        let mut g = self.session.write();
        *g = None;
    }
}

mod parking_lot_lite {
    //! Tiny `Mu` = `std::sync::Mutex<Option<T>>` wrapper exposing a
    //! `read`/`write` shape. Avoids depending on `parking_lot` here
    //! and keeps poison-handling explicit.

    use std::sync::Mutex;

    pub(crate) struct Mu<T>(Mutex<T>);

    pub(crate) struct ReadGuard<'a, T>(std::sync::MutexGuard<'a, T>);
    pub(crate) struct WriteGuard<'a, T>(std::sync::MutexGuard<'a, T>);

    impl<T> Mu<T> {
        pub(crate) const fn new(v: T) -> Self {
            Self(Mutex::new(v))
        }
        pub(crate) fn read(&self) -> ReadGuard<'_, T> {
            // Poison fallthrough: session shutdown after panic — we
            // still need to destroy. Take the inner regardless.
            let g = self.0.lock().unwrap_or_else(|p| p.into_inner());
            ReadGuard(g)
        }
        pub(crate) fn write(&self) -> WriteGuard<'_, T> {
            let g = self.0.lock().unwrap_or_else(|p| p.into_inner());
            WriteGuard(g)
        }
    }

    impl<T> std::ops::Deref for ReadGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl<T> std::ops::Deref for WriteGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl<T> std::ops::DerefMut for WriteGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
}

/// Handle to the dedicated session thread; provides graceful join +
/// crash detection per I-FUSE-8.
pub struct SessionThread {
    handle: Option<JoinHandle<::core::ffi::c_int>>,
    /// Weak reference to the session so we can call `exit` from
    /// outside the loop thread without keeping it alive.
    session: std::sync::Weak<SessionInner>,
}

impl SessionThread {
    /// Test-only: build a `SessionThread` around an arbitrary
    /// `std::thread::JoinHandle<c_int>`. Used by
    /// `tests/safety_contract.rs` to exercise the I-FUSE-8
    /// crash-detection path without needing a real libfuse session.
    #[doc(hidden)]
    pub fn synthetic_for_test(handle: JoinHandle<::core::ffi::c_int>) -> Self {
        Self {
            handle: Some(handle),
            session: std::sync::Weak::new(),
        }
    }

    /// Block until the session thread completes. Returns:
    /// - `Ok(())` on graceful exit (loop returned 0).
    /// - `Err(FuseError::Errno(rc))` on libfuse-reported error
    ///   (loop returned non-zero).
    /// - `Err(FuseError::Io)` on Rust-side panic in the loop thread.
    pub fn join(mut self) -> Result<(), FuseError> {
        let handle = self
            .handle
            .take()
            .expect("kiseki-fuse: SessionThread::join called twice");
        match handle.join() {
            Ok(0) => Ok(()),
            Ok(rc) => Err(FuseError::Errno(rc)),
            Err(_panic) => {
                // Translate panic to a structured error; caller
                // policy (abort vs auto-remount) decides what to do.
                Err(FuseError::Io)
            }
        }
    }

    /// Signal the loop to exit gracefully (calls `fuse_session_exit`).
    /// Safe to call from any thread.
    pub fn request_exit(&self) {
        if let Some(inner) = self.session.upgrade() {
            // SAFETY: see Session::exit.
            unsafe { sys::fuse_session_exit(inner.ptr().0) }
        }
    }

    /// Wrap this handle in a watchdog that aborts the process on
    /// non-graceful exit (I-FUSE-8 default).
    ///
    /// Returns a `WatchdogHandle` that waits for the session thread
    /// to complete; if it returns abnormally, the watchdog triggers
    /// `std::process::abort()`. If `auto_remount` were configured
    /// (Phase 1c+), the watchdog would attempt a single re-mount
    /// before falling back to abort.
    #[must_use = "the watchdog handle owns the session thread; dropping it joins immediately"]
    pub fn with_watchdog(self, behavior: CrashBehavior) -> WatchdogHandle {
        let join = std::thread::Builder::new()
            .name("kiseki-fuse-watchdog".into())
            .spawn(move || {
                let result = self.join();
                match (behavior, &result) {
                    (CrashBehavior::Abort, Err(_)) => {
                        tracing::error!(
                            "kiseki-fuse: session thread exited abnormally; \
                             aborting per I-FUSE-8 default"
                        );
                        std::process::abort();
                    }
                    (CrashBehavior::Abort, Ok(())) => {
                        tracing::info!("kiseki-fuse: session thread exited gracefully");
                    }
                    (CrashBehavior::PropagateError, _) => {
                        // The handle's join() result is what we
                        // return; no abort.
                    }
                }
                result
            })
            .expect("kiseki-fuse: spawn watchdog thread");
        WatchdogHandle { join: Some(join) }
    }
}

impl Drop for SessionThread {
    fn drop(&mut self) {
        // If the thread is still around when we drop, request exit
        // and join — best-effort cleanup.
        if let Some(handle) = self.handle.take() {
            self.request_exit();
            let _ = handle.join();
        }
    }
}

/// What to do if the session thread crashes (I-FUSE-8).
///
/// `Abort` is the default; the wrapper calls `std::process::abort()`
/// to fail-fast and preserve the pre-swap process-crash shape.
///
/// `PropagateError` is intended for tests + library consumers that
/// want to handle the crash themselves; it surfaces the loop's
/// non-zero return as `FuseError::Errno` from `WatchdogHandle::join`.
#[derive(Debug, Clone, Copy)]
pub enum CrashBehavior {
    /// `std::process::abort()` on session-thread crash.
    Abort,
    /// Surface the error from `WatchdogHandle::join`.
    PropagateError,
}

/// Owns the watchdog thread; joining returns the underlying session
/// thread's result.
pub struct WatchdogHandle {
    join: Option<JoinHandle<Result<(), FuseError>>>,
}

impl WatchdogHandle {
    /// Block until the watchdog completes. Returns the session
    /// thread's result.
    pub fn join(mut self) -> Result<(), FuseError> {
        let h = self
            .join
            .take()
            .expect("kiseki-fuse: WatchdogHandle::join called twice");
        h.join().unwrap_or(Err(FuseError::Io))
    }
}

impl Drop for WatchdogHandle {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}
