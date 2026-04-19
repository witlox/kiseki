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

pub mod cache;
pub mod discovery;
pub mod error;

pub use cache::ClientCache;
pub use discovery::{DiscoveryResponse, SeedEndpoint};
pub use error::ClientError;
