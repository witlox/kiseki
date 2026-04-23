//! C FFI bindings for the Kiseki native client (Phase 10.5).
//!
//! Exposes `kiseki_read`, `kiseki_write`, `kiseki_stat` as `extern "C"`
//! functions. Compiled into a shared library (`libkiseki_client.so`)
//! via `cargo build --features ffi`.
//!
//! Header: `include/kiseki_client.h` (generated or handwritten).
//!
//! Wired to `CacheManager` for cache-tier reads. Canonical fetch/write
//! paths are left as TODOs pending gateway client stabilization.
#![allow(clippy::cast_possible_truncation)] // u64 → usize for buffer sizes is intentional

use std::ffi::CStr;

use crate::cache::{CacheConfig, CacheManager, CacheMode, CacheStats};
use crate::staging::{StagingConfig, StagingManager};

use kiseki_common::ids::ChunkId;

// ---------------------------------------------------------------------------
// Opaque handle
// ---------------------------------------------------------------------------

/// Opaque handle to a Kiseki client session.
///
/// Holds the cache manager and a tokio runtime for async operations.
#[repr(C)]
pub struct KisekiHandle {
    cache: CacheManager,
    staging: StagingManager,
    _runtime: tokio::runtime::Runtime,
}

// ---------------------------------------------------------------------------
// Status codes
// ---------------------------------------------------------------------------

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
    /// Operation timed out.
    TimedOut = 6,
}

// ---------------------------------------------------------------------------
// C-compatible cache stats
// ---------------------------------------------------------------------------

/// Cache statistics returned via FFI.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KisekiCacheStats {
    pub l1_hits: u64,
    pub l2_hits: u64,
    pub misses: u64,
    pub bypasses: u64,
    pub errors: u64,
    pub l1_bytes: u64,
    pub l2_bytes: u64,
    pub meta_hits: u64,
    pub meta_misses: u64,
    pub wipes: u64,
}

impl From<CacheStats> for KisekiCacheStats {
    fn from(s: CacheStats) -> Self {
        Self {
            l1_hits: s.l1_hits,
            l2_hits: s.l2_hits,
            misses: s.misses,
            bypasses: s.bypasses,
            errors: s.errors,
            l1_bytes: s.l1_bytes,
            l2_bytes: s.l2_bytes,
            meta_hits: s.meta_hits,
            meta_misses: s.meta_misses,
            wipes: s.wipes,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse `CacheMode` from an environment variable string.
fn parse_cache_mode(s: &str) -> CacheMode {
    match s.to_lowercase().as_str() {
        "pinned" => CacheMode::Pinned,
        "bypass" => CacheMode::Bypass,
        _ => CacheMode::Organic,
    }
}

/// Build a `CacheConfig` from environment variables.
///
/// - `KISEKI_CACHE_MODE`: "organic" (default), "pinned", or "bypass".
/// - `KISEKI_CACHE_DIR`: L2 pool directory. Default: `/tmp/kiseki-cache`.
/// - `KISEKI_CACHE_L2_MAX`: L2 max bytes. Default: 50 GB.
fn config_from_env() -> CacheConfig {
    let mode = match std::env::var("KISEKI_CACHE_MODE") {
        Ok(v) => parse_cache_mode(&v),
        Err(_) => CacheMode::Organic,
    };

    let cache_dir = match std::env::var("KISEKI_CACHE_DIR") {
        Ok(v) => std::path::PathBuf::from(v),
        Err(_) => std::path::PathBuf::from("/tmp/kiseki-cache"),
    };

    let max_cache_bytes = std::env::var("KISEKI_CACHE_L2_MAX")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(50 * 1024 * 1024 * 1024);

    CacheConfig {
        mode,
        max_memory_bytes: CacheConfig::default().max_memory_bytes,
        max_cache_bytes,
        metadata_ttl: CacheConfig::default().metadata_ttl,
        cache_dir,
        max_disconnect_seconds: CacheConfig::default().max_disconnect_seconds,
    }
}

/// Derive a simple `ChunkId` from a path and offset for cache key lookup.
///
/// This is a placeholder until the real metadata service provides
/// file-to-chunk mappings. Uses a truncated hash of `path:offset`.
fn chunk_key_from_path(path: &str, offset: u64) -> ChunkId {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    offset.hash(&mut hasher);
    let h = hasher.finish();
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&h.to_le_bytes());
    id[8..16].copy_from_slice(&h.to_be_bytes());
    ChunkId(id)
}

/// Extract a `&str` from a C string pointer, returning `InvalidArgument` on failure.
///
/// # Safety
///
/// `ptr` must be a valid, null-terminated C string pointer.
unsafe fn cstr_to_str<'a>(ptr: *const std::ffi::c_char) -> Result<&'a str, KisekiStatus> {
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| KisekiStatus::InvalidArgument)
}

// ---------------------------------------------------------------------------
// FFI functions
// ---------------------------------------------------------------------------

/// Open a connection to a Kiseki cluster.
///
/// Creates a `CacheManager` configured from environment variables and
/// a tokio runtime for async operations.
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

    let config = config_from_env();

    // Build tokio runtime for async operations.
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    else {
        return KisekiStatus::IoError;
    };

    let Ok(cache) = CacheManager::new(&config) else {
        return KisekiStatus::IoError;
    };

    // Staging manager uses the cache pool directory (if L2 is active).
    let staging_pool_dir = if config.mode == CacheMode::Bypass {
        None
    } else {
        Some(config.cache_dir.clone())
    };
    let staging = StagingManager::new(staging_pool_dir, StagingConfig::default());

    let handle = Box::into_raw(Box::new(KisekiHandle {
        cache,
        staging,
        _runtime: runtime,
    }));
    unsafe { *handle_out = handle };
    KisekiStatus::Ok
}

/// Close a Kiseki client session.
///
/// Wipes the cache (zeroize all plaintext) and drops all resources.
///
/// # Safety
///
/// `handle` must have been returned by `kiseki_open`.
#[no_mangle]
pub unsafe extern "C" fn kiseki_close(handle: *mut KisekiHandle) -> KisekiStatus {
    if handle.is_null() {
        return KisekiStatus::InvalidArgument;
    }
    let mut h = unsafe { Box::from_raw(handle) };
    h.cache.wipe();
    // h is dropped here — runtime shuts down, staging cleaned up.
    drop(h);
    KisekiStatus::Ok
}

/// Read data from a Kiseki object.
///
/// Tries the cache first (L1 then L2). On cache miss, returns `NotFound`
/// — canonical fetch is not yet wired.
///
/// # Safety
///
/// `handle` must be valid. `path` must be null-terminated.
/// `buf` must point to at least `buf_len` bytes.
/// `bytes_read` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn kiseki_read(
    handle: *mut KisekiHandle,
    path: *const std::ffi::c_char,
    offset: u64,
    buf: *mut u8,
    buf_len: u64,
    bytes_read: *mut u64,
) -> KisekiStatus {
    if handle.is_null() || path.is_null() || buf.is_null() || bytes_read.is_null() {
        return KisekiStatus::InvalidArgument;
    }

    let h = unsafe { &mut *handle };
    let Ok(path_str) = (unsafe { cstr_to_str(path) }) else {
        return KisekiStatus::InvalidArgument;
    };

    let chunk_id = chunk_key_from_path(path_str, offset);

    if let Some(data) = h.cache.get_chunk(&chunk_id) {
        let copy_len = data.len().min(buf_len as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), buf, copy_len);
            *bytes_read = copy_len as u64;
        }
        KisekiStatus::Ok
    } else {
        // TODO: fetch from canonical via gateway client.
        unsafe { *bytes_read = 0 };
        KisekiStatus::NotFound
    }
}

/// Write data to a Kiseki object.
///
/// Write goes to canonical (not yet wired — returns `Ok` as a stub).
/// Updates metadata cache if the path is already cached.
///
/// # Safety
///
/// `handle` must be valid. `path` must be null-terminated.
/// `data` must point to at least `data_len` bytes.
/// `bytes_written` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn kiseki_write(
    handle: *mut KisekiHandle,
    path: *const std::ffi::c_char,
    _data: *const u8,
    data_len: u64,
    bytes_written: *mut u64,
) -> KisekiStatus {
    if handle.is_null() || path.is_null() || bytes_written.is_null() {
        return KisekiStatus::InvalidArgument;
    }

    let h = unsafe { &mut *handle };
    let Ok(path_str) = (unsafe { cstr_to_str(path) }) else {
        return KisekiStatus::InvalidArgument;
    };

    // Invalidate metadata cache for this path — stale after write.
    if h.cache.get_metadata(path_str).is_some() {
        h.cache.put_metadata(path_str.to_owned(), Vec::new());
    }

    // TODO: write to canonical via gateway client.
    unsafe { *bytes_written = data_len };
    KisekiStatus::Ok
}

/// Get file/object attributes.
///
/// Real stat is not yet wired — returns `Ok` with `size=0`.
///
/// # Safety
///
/// `handle` must be valid. `path` must be null-terminated.
/// `size_out` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn kiseki_stat(
    handle: *mut KisekiHandle,
    path: *const std::ffi::c_char,
    size_out: *mut u64,
) -> KisekiStatus {
    if handle.is_null() || path.is_null() || size_out.is_null() {
        return KisekiStatus::InvalidArgument;
    }

    // TODO: query metadata service for real file size.
    unsafe { *size_out = 0 };
    KisekiStatus::Ok
}

/// Stage a dataset into the local cache.
///
/// Delegates to `StagingManager::record_staged`. The actual chunk
/// fetching from canonical is not yet wired — this records the intent
/// so that subsequent reads can be served from cache once chunks arrive.
///
/// # Safety
///
/// `handle` must be valid. `path` must be null-terminated.
#[no_mangle]
pub unsafe extern "C" fn kiseki_stage(
    handle: *mut KisekiHandle,
    path: *const std::ffi::c_char,
    _timeout_secs: u32,
) -> KisekiStatus {
    if handle.is_null() || path.is_null() {
        return KisekiStatus::InvalidArgument;
    }

    let h = unsafe { &mut *handle };
    let Ok(path_str) = (unsafe { cstr_to_str(path) }) else {
        return KisekiStatus::InvalidArgument;
    };

    // TODO: fetch chunk list from metadata service and pull chunks
    // from canonical into cache. For now, record an empty staging entry.
    h.staging.record_staged(path_str.to_owned(), &[], 0);

    KisekiStatus::Ok
}

/// Release a previously staged dataset from the cache.
///
/// Unpins chunks so they become eligible for LRU eviction.
///
/// # Safety
///
/// `handle` must be valid. `path` must be null-terminated.
#[no_mangle]
pub unsafe extern "C" fn kiseki_release(
    handle: *mut KisekiHandle,
    path: *const std::ffi::c_char,
) -> KisekiStatus {
    if handle.is_null() || path.is_null() {
        return KisekiStatus::InvalidArgument;
    }

    let h = unsafe { &mut *handle };
    let Ok(path_str) = (unsafe { cstr_to_str(path) }) else {
        return KisekiStatus::InvalidArgument;
    };

    let released_chunks = h.staging.release(path_str);

    // Invalidate released chunks from cache so they can be evicted.
    for chunk_id in &released_chunks {
        h.cache.invalidate_chunk(chunk_id);
    }

    KisekiStatus::Ok
}

/// Fill a `KisekiCacheStats` struct with current cache statistics.
///
/// # Safety
///
/// `handle` must be valid. `stats_out` must point to a valid
/// `KisekiCacheStats` struct.
#[no_mangle]
pub unsafe extern "C" fn kiseki_cache_stats(
    handle: *mut KisekiHandle,
    stats_out: *mut KisekiCacheStats,
) -> KisekiStatus {
    if handle.is_null() || stats_out.is_null() {
        return KisekiStatus::InvalidArgument;
    }

    let h = unsafe { &*handle };
    let stats: KisekiCacheStats = h.cache.stats().into();
    unsafe { *stats_out = stats };
    KisekiStatus::Ok
}
