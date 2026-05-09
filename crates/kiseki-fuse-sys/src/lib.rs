//! Raw FFI bindings to libfuse 3.x.
//!
//! This crate exposes bindgen-generated declarations for libfuse's
//! low-level API (`fuse_session_new`, `fuse_session_loop`, reply
//! functions, etc.). The bindings are generated at build time from
//! `/usr/include/fuse3/fuse_lowlevel.h` and `fuse_common.h` via
//! `wrapper.h`.
//!
//! # Stability
//!
//! This crate is internal (`publish = false`). The Rust-side stable
//! API for kiseki consumers lives in `kiseki-fuse`. Bindings here
//! are unsafe and may change shape across libfuse versions.
//!
//! # Safety
//!
//! Every function in this crate is `unsafe extern "C"`. The
//! workspace lint `unsafe_code = "deny"` is locally relaxed at the
//! crate root to allow the generated code; the safe wrapper layer
//! lives in `kiseki-fuse` per ADR-043 §D4.
//!
//! # See also
//!
//! - ADR-043 §D2 (libfuse permitted system library)
//! - ADR-043 §D4 (`*-sys` + safe-wrapper convention)
//! - `specs/implementation/libfuse-swap.md` §"Crate layout"

// Bindgen-generated bindings inherit C-style names that don't match
// Rust's snake_case / UpperCamelCase conventions, don't have doc
// comments, etc. The workspace's strict lint set (unsafe_code = deny,
// missing_docs = warn, clippy::all + pedantic = warn) cannot be
// satisfied by generated FFI code. This crate disables the lints
// that bindgen output triggers; the safe wrapper layer in
// `kiseki-fuse` carries the strict lint set instead.
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(missing_docs)]
#![allow(unsafe_code)]
#![allow(clippy::all)]
#![allow(clippy::pedantic)]
#![allow(clippy::nursery)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

// libfuse 3.17 turned `fuse_session_new` into a versioning macro that
// expands to `fuse_session_new_versioned(..., &compile_time_version)`.
// bindgen sees the macro, not a function, so the binding only exposes
// `fuse_session_new_versioned` — which is tagged `@@FUSE_3.17` in the
// shared library and unavailable on libfuse < 3.17 (Ubuntu 24.04 LTS
// ships 3.14.0). To keep kiseki-fuse-sys ABI-portable across every
// libfuse 3.x distro the lowest pinned floor is 3.10), we declare
// the original (3.0+) function directly via `extern "C"` and call
// that from the safe wrapper. The symbol `fuse_session_new@@FUSE_3.0`
// is still exported by every libfuse 3.x release including 3.18.2.
unsafe extern "C" {
    /// Original (`@@FUSE_3.0`) `fuse_session_new` — bindgen can't see
    /// it because libfuse 3.17+ headers `#define` it as a macro.
    pub fn fuse_session_new(
        args: *mut fuse_args,
        op: *const fuse_lowlevel_ops,
        op_size: usize,
        userdata: *mut ::core::ffi::c_void,
    ) -> *mut fuse_session;
}
