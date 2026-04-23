//! Native client for Kiseki.
//!
//! Runs in workload processes on compute nodes. Exposes POSIX (FUSE)
//! and native API. Performs client-side tenant-layer encryption —
//! plaintext never leaves the workload process. Discovers shards/views/
//! gateways dynamically from the data fabric (ADR-008).
//!
//! Invariant mapping:
//!   - I-K1, I-K2 — client encrypts before sending (no plaintext on wire)
//!   - I-Auth1 — mTLS with Cluster CA for data fabric connections

#![deny(unsafe_code)]

pub mod batching;
pub mod cache;
pub mod discovery;
pub mod error;
#[cfg(feature = "fuse")]
#[allow(missing_docs, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub mod fuse_daemon;
#[allow(missing_docs)]
pub mod fuse_fs;
pub mod prefetch;
#[allow(unsafe_code)] // flock on Unix for pool handoff
pub mod staging;
pub mod transport_select;

#[cfg(feature = "ffi")]
#[allow(missing_docs, clippy::missing_safety_doc)]
pub mod ffi;

#[cfg(feature = "python")]
pub mod python;

pub use cache::{CacheConfig, CacheManager, CacheMode, CacheStats, ClientCache};
pub use discovery::{DiscoveryClient, DiscoveryResponse, SeedEndpoint};
pub use error::ClientError;
