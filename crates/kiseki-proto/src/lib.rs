//! Generated protobuf/gRPC types for Kiseki.
//!
//! Source of truth: `specs/architecture/proto/kiseki/v1/*.proto`. Rust
//! code is generated at build time by `build.rs` via `tonic-build` +
//! `prost-build`. Do not hand-edit anything under `v1` — edit the
//! `.proto` in `specs/architecture/proto/` and let the build emit new
//! output.
//!
//! The Go side of the boundary generates into `control/proto/kiseki/v1/`
//! from the same canonical `.proto` files.

#![allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]
#![allow(missing_docs, rust_2018_idioms)]

/// v1 protobuf types and gRPC services.
pub mod v1 {
    tonic::include_proto!("kiseki.v1");
}
