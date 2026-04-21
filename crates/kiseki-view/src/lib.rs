//! View Materialization for Kiseki.
//!
//! A view is a protocol-shaped materialized projection of one or more
//! shards, maintained by a stream processor. Rebuildable from the
//! source shards at any time (I-V1).
//!
//! Invariant mapping:
//!   - I-V1 — view derivable from shards alone (rebuildable)
//!   - I-V2 — consistent prefix up to watermark
//!   - I-V3 — consistency model per view descriptor
//!   - I-V4 — MVCC read pins with bounded TTL

#![deny(unsafe_code)]

pub mod descriptor;
pub mod error;
pub mod pin;
pub mod stream_processor;
pub mod versioning;
pub mod view;

pub use descriptor::{ConsistencyModel, ProtocolSemantics, ViewDescriptor};
pub use error::ViewError;
pub use pin::ReadPin;
pub use view::{ViewOps, ViewState, ViewStore};
