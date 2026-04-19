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
pub mod node;

pub use config::KisekiRaftConfig;
pub use node::KisekiNode;
