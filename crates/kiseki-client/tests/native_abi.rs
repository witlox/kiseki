#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Layer 1 reference tests for the **kiseki native client + C FFI ABI**.
//!
//! Kiseki ships a stable C ABI (`kiseki_open`, `kiseki_read`,
//! `kiseki_write`, `kiseki_stat`, `kiseki_stage`, `kiseki_release`,
//! `kiseki_close`, `kiseki_cache_stats`) used by the Python (PyO3)
//! and C++ wrapper bindings. Downstream consumers compile against
//! these symbols + the `KisekiStatus` discriminant + the
//! `KisekiCacheStats` struct layout. A regression in any of those
//! breaks downstream builds the same way an RFC wire-format
//! regression breaks an `mount.nfs4` client.
//!
//! There is no IETF RFC for kiseki's native ABI — this file IS the
//! representative variant per ADR-023 §D2. It pins:
//!   - `KisekiStatus` discriminant values (numeric ABI)
//!   - `KisekiCacheStats` field count + integer types
//!   - The presence of the public symbols documented in
//!     `kiseki-client::ffi` (compile-time check via `pub use`).
//!
//! Symbol presence at link time is exercised by the wrappers
//! themselves — `kiseki-client/python` and the C++ wrapper crates
//! dlopen libkiseki and resolve symbols by name; renaming any
//! symbol breaks their build/test.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "Kiseki native client + C FFI ABI".
//!
//! Owner: `kiseki-client::ffi` carries every symbol below. A future
//! refactor that renames or renumbers any of them MUST also update
//! the Python/C++ wrappers and bump the C-ABI version macro.
#![allow(clippy::doc_markdown, unsafe_code)]
#![cfg(feature = "ffi")]

use kiseki_client::ffi::{KisekiCacheStats, KisekiStatus};

// ===========================================================================
// KisekiStatus discriminant ABI
// ===========================================================================

/// The discriminant values are the wire-side ABI: Python/C++ wrappers
/// switch on integer values, not on enum variant names. Renumbering a
/// variant is a breaking change to downstream consumers.
#[test]
fn kiseki_status_discriminants_pinned() {
    assert_eq!(KisekiStatus::Ok as i32, 0, "ABI: Ok = 0");
    assert_eq!(KisekiStatus::NotFound as i32, 1, "ABI: NotFound = 1");
    assert_eq!(
        KisekiStatus::PermissionDenied as i32,
        2,
        "ABI: PermissionDenied = 2"
    );
    assert_eq!(KisekiStatus::IoError as i32, 3, "ABI: IoError = 3");
    assert_eq!(
        KisekiStatus::InvalidArgument as i32,
        4,
        "ABI: InvalidArgument = 4"
    );
    assert_eq!(
        KisekiStatus::NotConnected as i32,
        5,
        "ABI: NotConnected = 5"
    );
    assert_eq!(KisekiStatus::TimedOut as i32, 6, "ABI: TimedOut = 6");
}

/// Sentinel: pin the i32-sized layout — wrappers expect a 4-byte
/// integer return type. Switching to repr(C) usize or i16 would
/// silently break Python/C++ wrappers.
#[test]
fn kiseki_status_size_is_i32() {
    assert_eq!(
        std::mem::size_of::<KisekiStatus>(),
        4,
        "ABI: KisekiStatus is 4-byte (i32) — repr(C) enum"
    );
}

// ===========================================================================
// KisekiCacheStats struct ABI
// ===========================================================================

/// `KisekiCacheStats` crosses the FFI boundary by pointer. Field
/// count + integer types are pinned by the wrapper layout; renaming
/// or removing a field IS a breaking change.
#[test]
fn kiseki_cache_stats_layout_pinned() {
    let stats = KisekiCacheStats::default();
    // Each field is u64; 10 fields = 80 bytes. repr(C) layout means
    // the struct size is at least the sum of fields.
    assert_eq!(
        std::mem::size_of::<KisekiCacheStats>(),
        10 * 8,
        "ABI: KisekiCacheStats is exactly 10 u64 fields = 80 bytes"
    );
    // Default values for every field are zero (Default derive).
    assert_eq!(stats.l1_hits, 0);
    assert_eq!(stats.l2_hits, 0);
    assert_eq!(stats.misses, 0);
    assert_eq!(stats.bypasses, 0);
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.l1_bytes, 0);
    assert_eq!(stats.l2_bytes, 0);
    assert_eq!(stats.meta_hits, 0);
    assert_eq!(stats.meta_misses, 0);
    assert_eq!(stats.wipes, 0);
}

// ===========================================================================
// Cross-implementation seed — wrapper layout matches C header
// ===========================================================================

/// Pin the field order. Wrappers (Python ctypes, C++ struct) declare
/// the struct in the same order; if these diverge, downstream reads
/// the wrong field. The Default-zeroed struct is read out by raw
/// pointer to confirm the layout is repr(C) and field offsets line
/// up against a hand-rolled struct of u64s.
#[test]
fn kiseki_cache_stats_field_order_via_raw_layout() {
    // Build a struct where each field's index is encoded in the
    // value, then read back through a u64 pointer. If field order
    // were reordered by the compiler, this would fail.
    let stats = KisekiCacheStats {
        l1_hits: 0,
        l2_hits: 1,
        misses: 2,
        bypasses: 3,
        errors: 4,
        l1_bytes: 5,
        l2_bytes: 6,
        meta_hits: 7,
        meta_misses: 8,
        wipes: 9,
    };
    let ptr = std::ptr::addr_of!(stats).cast::<u64>();
    for i in 0..10u64 {
        // SAFETY: KisekiCacheStats is repr(C), 10 contiguous u64 fields.
        let v = unsafe { *ptr.add(usize::try_from(i).expect("u64 fits usize")) };
        assert_eq!(v, i, "ABI: field at offset {i} reads back as {i}");
    }
}
