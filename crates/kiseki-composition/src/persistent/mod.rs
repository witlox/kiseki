//! Persistent composition state (ADR-040).
//!
//! Splits the storage of `compositions` (the `comp_id` → `Composition`
//! map) from the rest of `CompositionStore` so the same
//! `CompositionStore` struct can be backed by either an in-memory
//! `HashMap` (tests, single-node deployments) or a redb-backed sibling
//! that survives restart. `namespaces` and `multiparts` remain
//! in-memory per ADR-040 §D11.
//!
//! Hydrator state — `last_applied_seq`, `stuck_at_seq`,
//! `stuck_retries` (I-1), and `halted` (§D6.3) — also lives behind the
//! storage trait so the persistent backend can survive crash
//! correctly: I-CP1 requires the same redb transaction commits both
//! the data and the meta keys.
//!
//! Module layout:
//!   - `error`   — `PersistentStoreError` (ADR-040 §D8.1)
//!   - `storage` — `CompositionStorage` trait + `MemoryStorage` impl
//!   - `redb`    — `PersistentRedbStorage` impl

pub mod error;
pub mod redb;
pub mod storage;

pub use error::PersistentStoreError;
pub use redb::PersistentRedbStorage;
pub use storage::{CompositionStorage, HydrationBatch, MemoryStorage};
