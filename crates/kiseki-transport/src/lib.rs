//! Pluggable transport layer for Kiseki.
//!
//! Provides the [`Transport`] trait for bidirectional byte-stream
//! connections, a TCP+TLS reference implementation with mTLS
//! (Cluster CA validation, I-Auth1), and feature-flagged stubs for
//! libfabric/CXI and RDMA verbs transports.
//!
//! Invariant mapping:
//!   - I-K2   — all data on the wire is TLS-encrypted (or pre-encrypted chunks over CXI)
//!   - I-K13  — mTLS with Cluster CA validation
//!   - I-Auth1 — require client cert on data fabric
//!   - I-Auth3 — SPIFFE SVID validation (via SAN matching)

#![deny(unsafe_code)]

pub mod config;
pub mod error;
pub mod health;
pub mod metrics;
pub mod pool;
pub mod revocation;
pub mod spiffe;
pub mod tcp_tls;
pub mod traits;

#[cfg(feature = "cxi")]
pub mod cxi;

#[cfg(feature = "verbs")]
pub mod verbs;

pub use config::TlsConfig;
pub use error::TransportError;
pub use health::{HealthConfig, TransportHealthTracker};
pub use metrics::TransportMetrics;
pub use pool::{ConnectionPool, PoolConfig};
pub use tcp_tls::{TcpTlsTransport, TimeoutConfig};
pub use traits::{Connection, PeerIdentity, Transport};
