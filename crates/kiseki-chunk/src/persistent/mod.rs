//! Persistent chunk-meta layer (ADR-022 rev-4).
//!
//! Replaced the pre-rev-4 `save_meta` / `save_frag_meta` JSON
//! rewrite-the-world pattern with a fjall WAL keyed by `chunk_id` /
//! (`chunk_id`, `fragment_index`). Mirrors the
//! `kiseki-composition::persistent` module layout so the two hot-
//! path stores share the same backend, encoding-module idiom, and
//! periodic-flush / `fsync_pending` plumbing.
//!
//! Layout:
//! - `encoding`   — wire-format helpers (chunk + fragment records,
//!   schema version, key encodings) shared by every backend
//! - `fjall_meta` — `FjallMetaStore` impl + `FjallMetaFlusher`
//!   off-thread fsync handle

pub mod encoding;
pub mod fjall_meta;

pub use fjall_meta::{FjallMetaFlusher, FjallMetaStore};
