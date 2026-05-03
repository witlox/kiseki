//! Persistent `ViewStore` (ADR-040).
//!
//! Symmetric to `kiseki_composition::persistent` (which makes
//! `compositions` durable across restart). For views, ADR-040 §D11
//! specifies "all of it persists" since views aren't transient — but
//! pins ARE session state with millisecond TTLs, so they're dropped
//! on restart and clients re-pin.
//!
//! Module layout mirrors the composition equivalent:
//!   - `storage` — `ViewStorage` trait + `MemoryStorage` impl
//!   - `redb`    — `PersistentRedbStorage` impl

pub mod redb;
pub mod storage;

pub use redb::PersistentRedbStorage;
pub use storage::{MemoryStorage, ViewStorage};
