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
