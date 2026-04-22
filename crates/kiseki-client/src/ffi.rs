//! C FFI bindings for the Kiseki native client (Phase 10.5).
//!
//! Exposes `kiseki_read`, `kiseki_write`, `kiseki_stat` as `extern "C"`
//! functions. Compiled into a shared library (`libkiseki_client.so`)
//! via `cargo build --features ffi`.
//!
//! Header: `include/kiseki_client.h` (generated or handwritten).
//!
//! These are stubs — implementations will delegate to `KisekiFuse`
//! or a direct gateway client when the native API stabilizes.

/// Opaque handle to a Kiseki client session.
#[repr(C)]
pub struct KisekiHandle {
    _private: [u8; 0],
}

/// Status codes returned by FFI functions.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KisekiStatus {
    /// Operation succeeded.
    Ok = 0,
    /// File or object not found.
    NotFound = 1,
    /// Permission denied.
    PermissionDenied = 2,
    /// I/O error.
    IoError = 3,
    /// Invalid argument.
    InvalidArgument = 4,
    /// Client not connected.
    NotConnected = 5,
}

/// Open a connection to a Kiseki cluster.
///
/// # Safety
///
/// `seed_addr` must be a valid null-terminated C string.
/// The returned handle must be freed with `kiseki_close`.
#[no_mangle]
pub unsafe extern "C" fn kiseki_open(
    _seed_addr: *const std::ffi::c_char,
    handle_out: *mut *mut KisekiHandle,
) -> KisekiStatus {
    if handle_out.is_null() {
        return KisekiStatus::InvalidArgument;
    }
    // Stub: allocate a dummy handle.
    let handle = Box::into_raw(Box::new(KisekiHandle { _private: [] }));
    unsafe { *handle_out = handle };
    KisekiStatus::Ok
}

/// Close a Kiseki client session.
///
/// # Safety
///
/// `handle` must have been returned by `kiseki_open`.
#[no_mangle]
pub unsafe extern "C" fn kiseki_close(handle: *mut KisekiHandle) -> KisekiStatus {
    if handle.is_null() {
        return KisekiStatus::InvalidArgument;
    }
    unsafe { drop(Box::from_raw(handle)) };
    KisekiStatus::Ok
}

/// Read data from a Kiseki object.
///
/// # Safety
///
/// `handle` must be valid. `path` must be null-terminated.
/// `buf` must point to at least `buf_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kiseki_read(
    _handle: *mut KisekiHandle,
    _path: *const std::ffi::c_char,
    _offset: u64,
    _buf: *mut u8,
    _buf_len: u64,
    _bytes_read: *mut u64,
) -> KisekiStatus {
    // Stub: not yet implemented.
    KisekiStatus::NotConnected
}

/// Write data to a Kiseki object.
///
/// # Safety
///
/// `handle` must be valid. `path` must be null-terminated.
/// `data` must point to at least `data_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kiseki_write(
    _handle: *mut KisekiHandle,
    _path: *const std::ffi::c_char,
    _data: *const u8,
    _data_len: u64,
    _bytes_written: *mut u64,
) -> KisekiStatus {
    // Stub: not yet implemented.
    KisekiStatus::NotConnected
}

/// Get file/object attributes.
///
/// # Safety
///
/// `handle` must be valid. `path` must be null-terminated.
#[no_mangle]
pub unsafe extern "C" fn kiseki_stat(
    _handle: *mut KisekiHandle,
    _path: *const std::ffi::c_char,
    _size_out: *mut u64,
) -> KisekiStatus {
    // Stub: not yet implemented.
    KisekiStatus::NotConnected
}
