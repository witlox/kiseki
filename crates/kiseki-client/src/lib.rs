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
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod advisory;
pub mod batching;
pub mod cache;
pub mod discovery;
pub mod error;
#[cfg(feature = "fuse")]
#[allow(missing_docs, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub mod fuse_daemon;
#[allow(missing_docs)]
pub mod fuse_fs;
pub mod policy;
pub mod prefetch;
pub mod scrub;
#[allow(unsafe_code)] // flock on Unix for pool handoff
pub mod staging;
pub mod transport_select;

#[cfg(feature = "remote-http")]
pub mod remote_http;

#[cfg(feature = "remote-nfs")]
#[allow(missing_docs, clippy::cast_possible_truncation)]
pub mod remote_nfs;

#[cfg(feature = "native")]
pub mod native;

/// `GatewayOps` adapter over the ADR-042 TCP-framed-postcard binding.
/// FUSE / NFS / S3 callers consume `GatewayOps`; this lets them ride
/// the native binding the same way `remote_http` lets them ride S3.
#[cfg(feature = "native")]
pub mod native_remote;

/// Bench driver — drives PUT/GET against an externally-running
/// kiseki cluster. The `kiseki-client bench` subcommand uses this
/// (see [`crate::bench::run`]). Only compiled when at least one of
/// `native` or `remote-http` is enabled.
#[cfg(any(feature = "native", feature = "remote-http"))]
pub mod bench;

#[cfg(feature = "ffi")]
#[allow(missing_docs, unsafe_code, clippy::missing_safety_doc)]
pub mod ffi;

#[cfg(feature = "python")]
pub mod python;

pub use cache::{CacheConfig, CacheManager, CacheMode, CacheStats, ClientCache};
pub use discovery::{DiscoveryClient, DiscoveryResponse, SeedEndpoint};
pub use error::ClientError;
