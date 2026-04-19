//! Memory protection for key material (I-K8).
//!
//! Provides `mlock` (prevent swap) and `MADV_DONTDUMP` (exclude from
//! core dumps) for pages containing key material. Failure to lock is
//! logged but not fatal — `RLIMIT_MEMLOCK` may be insufficient in
//! containerized deployments.

/// Lock a memory region to prevent it from being swapped to disk.
///
/// Also marks the region with `MADV_DONTDUMP` to exclude it from core
/// dumps. Both calls are best-effort: failure logs a warning but does
/// not prevent the caller from using the key material.
///
/// # Safety
///
/// The `ptr` must be a valid pointer to `len` bytes of allocated memory
/// that remains valid for the duration of the lock. The caller is
/// responsible for calling [`munlock`] before the memory is freed.
pub(crate) unsafe fn mlock(ptr: *const u8, len: usize) -> bool {
    // SAFETY: caller guarantees ptr is valid for len bytes.
    let mlock_ok = unsafe { libc::mlock(ptr.cast(), len) } == 0;

    // MADV_DONTDUMP: exclude from core dumps (Linux 3.4+).
    // On macOS this is a no-op (no MADV_DONTDUMP equivalent).
    #[cfg(target_os = "linux")]
    {
        // SAFETY: same memory region, valid for len bytes.
        unsafe {
            libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_DONTDUMP);
        }
    }

    mlock_ok
}

/// Unlock a previously locked memory region, allowing the OS to swap
/// it again.
///
/// # Safety
///
/// Same requirements as [`mlock`].
pub(crate) unsafe fn munlock(ptr: *const u8, len: usize) {
    // SAFETY: caller guarantees ptr is valid for len bytes.
    unsafe {
        libc::munlock(ptr.cast(), len);
    }
}
