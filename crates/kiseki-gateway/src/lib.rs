//! Protocol gateways for Kiseki.
//!
//! Translates NFS and S3 wire protocol requests into operations
//! against views. Reads from views; does not maintain them. Performs
//! gateway-side encryption for protocol-path clients (NFS/S3 clients
//! send plaintext over TLS; gateway encrypts before writing).
//!
//! The build-phases spec (Phase 9) calls for two separate crates:
//! `kiseki-gateway-nfs` and `kiseki-gateway-s3`. We unify into a
//! single crate with feature flags to reduce workspace complexity
//! at this stage. The trait boundary (`GatewayOps`) is the stable
//! contract; the NFS/S3 protocol implementations will be fleshed out
//! when protocol-specific dependencies are integrated.
//!
//! Invariant mapping:
//!   - I-K1, I-K2 — gateway encrypts before writing (no plaintext past boundary)
//!   - I-Auth1 — mTLS on data fabric connections
//!   - I-Auth2 — optional tenant `IdP` second-stage auth

#![deny(unsafe_code)]

pub mod error;
pub mod mem_gateway;
pub mod ops;

#[cfg(feature = "nfs")]
pub mod nfs;

// NFS protocol implementation — internal wire format code.
// Allows for protocol-specific patterns (casts, missing docs on XDR fields).
#[cfg(feature = "nfs")]
#[allow(
    missing_docs,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::doc_markdown,
    clippy::single_match_else,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value,
    clippy::unwrap_used
)]
pub mod nfs3_server;

#[cfg(feature = "nfs")]
#[allow(
    missing_docs,
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value,
    clippy::unwrap_used,
    clippy::items_after_statements,
    clippy::manual_let_else,
    clippy::struct_field_names,
    clippy::unused_self,
    dead_code
)]
pub mod nfs4_server;

#[cfg(feature = "nfs")]
#[allow(
    missing_docs,
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::needless_pass_by_value,
    clippy::unwrap_used
)]
pub mod nfs_ops;

#[cfg(feature = "nfs")]
#[allow(missing_docs, clippy::doc_markdown, clippy::unwrap_used)]
pub mod nfs_server;

#[cfg(feature = "nfs")]
#[allow(
    missing_docs,
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_possible_wrap
)]
pub mod nfs_xdr;

#[cfg(feature = "s3")]
pub mod s3;

#[cfg(feature = "s3")]
pub mod s3_server;

pub use error::GatewayError;
pub use mem_gateway::InMemoryGateway;
pub use ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};
