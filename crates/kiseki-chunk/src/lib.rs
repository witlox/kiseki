//! Chunk Storage for Kiseki.
//!
//! Manages encrypted, content-addressed chunks. Chunks are immutable
//! (I-C1), reference-counted (I-C2), placed in affinity pools (I-C3),
//! and protected by retention holds (I-C2b).
//!
//! Invariant mapping:
//!   - I-C1 — chunks immutable; no update API
//!   - I-C2 — no GC while refcount > 0
//!   - I-C2b — no GC while retention hold active
//!   - I-C3 — placement per affinity policy
//!   - I-C4 — EC per pool (durability strategy)

#![deny(unsafe_code)]

pub mod ec;
pub mod error;
pub mod placement;
pub mod pool;
pub mod store;

pub use error::ChunkError;
pub use pool::{AffinityPool, DurabilityStrategy};
pub use store::{ChunkOps, ChunkStore};
