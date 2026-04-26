//! Control plane for Kiseki.
//!
//! Manages the tenant hierarchy (org -> project -> workload), IAM,
//! policy, placement, compliance tags, federation, and advisory policy.
//!
//! ADR-027: Single-language Rust implementation. This crate depends
//! ONLY on `kiseki-common` and `kiseki-proto` — no data-path crates.
//!
//! Spec: `ubiquitous-language.md`, I-T1..I-T4, `control-plane.feature`.

#![deny(unsafe_code)]

pub mod advisory_policy;
pub mod cache_policy;
pub mod error;
pub mod federation;
pub mod flavor;
#[allow(
    clippy::result_large_err,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::doc_markdown
)]
pub mod grpc;
pub mod iam;
pub mod idp;
pub mod maintenance;
pub mod namespace;
pub mod node_lifecycle;
pub mod policy;
pub mod replication;
pub mod retention;
pub mod shard_topology;
pub mod storage_admin;
pub mod tenant;
pub mod threshold;
