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
pub mod hydrator;
pub mod log_bridge;
pub mod multipart;
pub mod namespace;

pub use composition::{
    composition_hash_key, decode_composition_create_payload, decode_composition_delete_payload,
    decode_composition_update_payload, encode_composition_create_payload,
    encode_composition_delete_payload, encode_composition_update_payload, Composition,
    CompositionOps, DeleteResult, COMPOSITION_CREATE_PAYLOAD_LEN, COMPOSITION_DELETE_PAYLOAD_LEN,
    COMPOSITION_UPDATE_PAYLOAD_LEN, INLINE_DATA_THRESHOLD,
};
pub use error::CompositionError;
pub use hydrator::CompositionHydrator;
pub use multipart::{MultipartState, MultipartUpload};
pub use namespace::{ComplianceTag, Namespace};
