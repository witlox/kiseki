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

pub mod error;
pub mod iam;
pub mod maintenance;
pub mod namespace;
pub mod tenant;
