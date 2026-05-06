//! Persistent composition state (ADR-040).
//!
//! Splits the storage of `compositions` (the `comp_id` → `Composition`
//! map) from the rest of `CompositionStore` so the same struct can be
//! backed by either an in-memory `HashMap` (tests, single-node
//! deployments) or a fjall-backed sibling that survives restart.
//! `namespaces` and `multiparts` remain in-memory per ADR-040 §D11.
//!
//! Hydrator state — `last_applied_seq`, `stuck_at_seq`,
//! `stuck_retries` (I-1), and `halted` (§D6.3) — also lives behind the
//! storage trait so the persistent backend survives crash correctly:
//! I-CP1 requires the same atomic batch commits both the data and the
//! meta keys.
//!
//! Module layout:
//!   - `error`    — `PersistentStoreError` (ADR-040 §D8.1)
//!   - `encoding` — wire format helpers (composition record, name
//!                   key, stuck-state) shared by every backend
//!   - `storage`  — `CompositionStorage` trait + `MemoryStorage` impl
//!   - `fjall`    — `FjallStorage` impl (the write-heavy backend per
//!                   ADR-022's "migrate to fjall" escape clause —
//!                   replaced redb 2026-05-06 after measuring the
//!                   redb commit-cost ceiling at ~18 k op/s)

pub mod encoding;
pub mod error;
pub mod fjall;
pub mod storage;

pub use error::PersistentStoreError;
pub use fjall::{FjallFlusher, FjallStorage};
pub use storage::{CompositionStorage, HydrationBatch, MemoryStorage};
