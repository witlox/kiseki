//! Pluggable transport layer for Kiseki.
//!
//! Provides the [`Transport`] trait for bidirectional byte-stream
//! connections, a TCP+TLS reference implementation with mTLS
//! (Cluster CA validation, I-Auth1), and feature-flagged RDMA verbs
//! and CXI/libfabric transports for HPC fabrics.
//!
//! Invariant mapping:
//!   - I-K2   — all data on the wire is TLS-encrypted (or pre-encrypted chunks over CXI)
//!   - I-K13  — mTLS with Cluster CA validation
//!   - I-Auth1 — require client cert on data fabric
//!   - I-Auth3 — SPIFFE SVID validation (via SAN matching)

// unsafe_code is denied crate-wide except in feature-gated FFI modules
// (verbs.rs, cxi.rs) which have per-block SAFETY comments.
#![deny(unsafe_code)]

pub mod config;
pub mod error;
pub mod health;
pub mod metrics;
#[allow(unsafe_code)] // pin_linux uses sched_setaffinity
pub mod numa;
pub mod pool;
pub mod revocation;
pub mod selector;
pub mod spiffe;
pub mod tcp_tls;
pub mod traits;

#[cfg(feature = "cxi")]
#[allow(unsafe_code)]
pub mod cxi;

#[cfg(feature = "verbs")]
#[allow(unsafe_code)]
pub mod verbs;

pub use config::TlsConfig;
pub use error::TransportError;
pub use health::{HealthConfig, TransportHealthTracker};
pub use metrics::TransportMetrics;
pub use numa::NumaTopology;
pub use pool::{ConnectionPool, PoolConfig};
pub use selector::{DynConnection, FabricSelector, FabricTransport};
pub use tcp_tls::{TcpTlsTransport, TimeoutConfig};
pub use traits::{Connection, PeerIdentity, Transport};
