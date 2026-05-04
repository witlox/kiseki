//! Audit log for Kiseki.
//!
//! Per-tenant audit shards (ADR-009). Append-only, immutable, same
//! durability guarantees as the Log (I-A1). Tenant audit export
//! delivers filtered events to the tenant's VLAN (I-A2). Cluster admin
//! sees system-level events only (I-A3). The audit log is a GC consumer
//! — delta GC is blocked until audit has captured the relevant event
//! (I-A4).
//!
//! Invariant mapping:
//!   - I-A1 — append-only, immutable
//!   - I-A2 — tenant-scoped export
//!   - I-A3 — cluster admin sees system events only
//!   - I-A4 — audit log is a GC consumer (blocks delta GC)

#![deny(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod error;
pub mod event;
pub mod health;
pub mod raft;
pub mod raft_store;
pub mod store;

pub use error::AuditError;
pub use event::{AuditEvent, AuditEventType};
pub use health::{AuditHealth, AuditStatus};
pub use raft_store::RaftAuditStore;
pub use store::{AuditLog, AuditOps, AuditQuery};
