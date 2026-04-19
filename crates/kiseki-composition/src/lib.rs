//! Composition context for Kiseki.
//!
//! A composition is a tenant-scoped metadata structure describing how
//! to assemble chunks into a coherent data unit (file or object).
//! Stored as a sequence of deltas in the Log, reconstructed by replay.
//!
//! Invariant mapping:
//!   - I-X1 — composition belongs to exactly one tenant
//!   - I-X2 — chunks respect tenant dedup policy
//!   - I-X3 — mutation history fully reconstructible from deltas
//!   - I-L5 — composition not visible until finalize (multipart)
//!   - I-L8 — cross-shard rename returns EXDEV

#![deny(unsafe_code)]

pub mod composition;
pub mod error;
pub mod multipart;
pub mod namespace;

pub use composition::{Composition, CompositionOps};
pub use error::CompositionError;
pub use multipart::{MultipartState, MultipartUpload};
pub use namespace::Namespace;
