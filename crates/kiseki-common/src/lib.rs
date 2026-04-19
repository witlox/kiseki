//! Shared domain types for Kiseki.
//!
//! This crate is the leaf of the workspace dependency graph: it depends on
//! `std`, `uuid`, `zeroize`, and `thiserror` only — nothing else in the
//! Kiseki workspace may depend on anything this crate doesn't already.
//!
//! All identifiers and structural types here come from
//! `specs/ubiquitous-language.md` and `specs/architecture/data-models/*.rs`.
//! Type names MUST match the ubiquitous language exactly (no `Seq` for
//! `SequenceNumber`, no `Tenant` for `OrgId`, etc.).
//!
//! Invariant mapping:
//!   - I-T5, I-T7 — HLC is authoritative for ordering and causality.
//!   - I-T6       — `ClockQuality` reported per node.
//!   - I-WA3, I-WA10, I-WA11 — advisory surface types in this crate carry
//!     opaque capability references; cluster-internal identifiers are NEVER
//!     exposed at the advisory layer.

#![deny(unsafe_code)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod error;
pub mod ids;
pub mod tenancy;
pub mod time;

// Advisory surface lives here per ADR-021 §2 to preserve the no-cycle rule.
pub mod advisory;

// Re-export the flat public surface for convenience. Match
// ubiquitous-language.md names exactly.
pub use error::{KisekiError, PermanentError, RetriableError, SecurityError};
pub use ids::{
    ChunkId, CompositionId, NamespaceId, NodeId, OrgId, ProjectId, SequenceNumber, ShardId, ViewId,
    WorkloadId,
};
pub use tenancy::{ComplianceTag, DedupPolicy, KeyEpoch, Quota, TenantScope};
pub use time::{ClockQuality, DeltaTimestamp, HlcExhausted, HybridLogicalClock, WallTime};

pub use advisory::{
    AccessPattern, AffinityPreference, ClientId, DedupIntent, OperationAdvisory, PhaseId,
    PoolDescriptor, PoolHandle, Priority, RetentionIntent, WorkflowRef, WorkloadProfile,
};
