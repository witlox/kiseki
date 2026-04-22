//! Shared Raft type configuration for Kiseki.
//!
//! Defines `KisekiTypeConfig` used by all Raft groups (key manager,
//! log shards, audit shards). Each context defines its own `D`
//! (request) and `R` (response) types, but they share the node
//! identity, entry format, and async runtime.
//!
//! Spec: ADR-007 (key manager HA), I-L2 (log durability).

#![deny(unsafe_code)]

pub mod config;
pub mod log_store;
pub mod network;
pub mod node;
#[allow(
    missing_docs,
    clippy::len_without_is_empty,
    clippy::must_use_candidate,
    clippy::cast_possible_truncation,
    clippy::unwrap_used,
    clippy::manual_map,
    clippy::io_other_error,
    clippy::map_err_ignore,
    mismatched_lifetime_syntaxes,
    deprecated
)]
pub mod redb_log_store;

pub use config::KisekiRaftConfig;
pub use log_store::MemLogStore;
#[allow(
    missing_docs,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::unused_self,
    clippy::doc_markdown,
    clippy::io_other_error,
    clippy::needless_pass_by_value,
    dead_code
)]
pub mod redb_raft_log_store;
pub use redb_raft_log_store::RedbRaftLogStore;
#[allow(
    missing_docs,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::cast_possible_truncation,
    clippy::unused_self,
    clippy::doc_markdown,
    clippy::io_other_error,
    clippy::needless_pass_by_value,
    dead_code
)]
pub mod tcp_transport;

pub use network::{StubNetwork, StubNetworkFactory};
pub use node::KisekiNode;
pub use redb_log_store::RedbLogStore;
pub use tcp_transport::{TcpNetwork, TcpNetworkFactory};
