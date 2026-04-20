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

#[cfg(feature = "s3")]
pub mod s3;

pub use error::GatewayError;
pub use mem_gateway::InMemoryGateway;
pub use ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};
