//! `ViewStorage` trait + `MemoryStorage` impl (ADR-040).
//!
//! Decouples `ViewStore`'s `view_id` → `MaterializedView` map from
//! the in-memory `HashMap` so the same `ViewStore` can be backed by
//! either an in-memory store (tests, single-node deployments) or the
//! redb-backed sibling that survives restart.
//!
//! Pins are NOT part of the storage trait — they're transient
//! session state (TTL'd in ms, see ADR-040 §D11) and recreated by
//! clients after restart. The persistent backend stores the static
//! parts of `MaterializedView`: descriptor + state + watermark +
//! `last_advanced_ms`.

use std::collections::HashMap;

use kiseki_common::ids::{SequenceNumber, ViewId};
use thiserror::Error;

use crate::descriptor::ViewDescriptor;
use crate::view::ViewState;

/// What the persistent backend stores per view.
///
/// Pins live only in the in-memory `MaterializedView`. On reopen the
/// pins start empty and clients re-acquire — see ADR-040 §D11.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PersistedView {
    /// View descriptor (immutable identity + protocol/consistency).
    pub descriptor: ViewDescriptor,
    /// Lifecycle state at the time of the last persist.
    pub state: ViewState,
    /// Highest sequence consumed from the source shard(s) at the
    /// time of the last persist. Survives restart so a follower
    /// resumes from where it left off.
    pub watermark: SequenceNumber,
    /// Wall-clock time (ms) when watermark was last advanced.
    pub last_advanced_ms: u64,
    /// Highest pin id ever issued for this view. Persisted so that
    /// pin ids stay monotonic across restart even though the active
    /// pins themselves are dropped.
    pub next_pin_id: u64,
}

/// Errors a `ViewStorage` op can return.
#[derive(Debug, Error)]
pub enum PersistentStoreError {
    /// redb returned an error (open, transaction, IO).
    #[error("redb storage error: {0}")]
    Redb(String),
    /// postcard could not encode a `PersistedView`.
    #[error("encoding error: {0}")]
    Encode(String),
    /// postcard could not decode a stored record.
    #[error("decoding error: {0}")]
    Decode(String),
    /// On-disk schema version is newer than this binary supports.
    #[error("schema version too new: found {found}, supported up to {supported}")]
    SchemaTooNew {
        /// Schema version found on disk.
        found: u8,
        /// Highest schema version this binary can decode.
        supported: u8,
    },
}

impl From<postcard::Error> for PersistentStoreError {
    fn from(e: postcard::Error) -> Self {
        Self::Encode(e.to_string())
    }
}

/// Storage backend for the `view_id` → `PersistedView` map plus the
/// hydrator's `last_applied_seq` cursor.
pub trait ViewStorage: Send + Sync {
    /// Retrieve a view by id. Returns `None` if absent.
    fn get(&self, id: ViewId) -> Result<Option<PersistedView>, PersistentStoreError>;

    /// Number of views stored.
    fn count(&self) -> Result<u64, PersistentStoreError>;

    /// Enumerate every view. Used at startup to rehydrate the
    /// in-memory `ViewStore`'s `views` map.
    fn list_all(&self) -> Result<Vec<PersistedView>, PersistentStoreError>;

    /// Insert or replace a view. Used by both the gateway
    /// (`create_view` / `discard_view`) and the hydrator
    /// (`advance_watermark`).
    fn put(&mut self, view: PersistedView) -> Result<(), PersistentStoreError>;

    /// Remove a view. Returns `true` if it existed.
    fn remove(&mut self, id: ViewId) -> Result<bool, PersistentStoreError>;

    /// Highest delta sequence whose state has been durably applied.
    /// Symmetric to the `CompositionStore` meta key.
    fn last_applied_seq(&self) -> Result<SequenceNumber, PersistentStoreError>;

    /// Advance `last_applied_seq`. Persisted in the same redb
    /// transaction as the view writes the hydrator emitted.
    fn set_last_applied_seq(&mut self, seq: SequenceNumber) -> Result<(), PersistentStoreError>;
}

// -- In-memory backend ------------------------------------------------------

/// In-memory `ViewStorage` for tests and single-node clusters.
#[derive(Debug)]
pub struct MemoryStorage {
    views: HashMap<ViewId, PersistedView>,
    last_applied_seq: SequenceNumber,
}

impl MemoryStorage {
    /// Construct an empty in-memory storage.
    #[must_use]
    pub fn new() -> Self {
        Self {
            views: HashMap::new(),
            last_applied_seq: SequenceNumber(0),
        }
    }
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewStorage for MemoryStorage {
    fn get(&self, id: ViewId) -> Result<Option<PersistedView>, PersistentStoreError> {
        Ok(self.views.get(&id).cloned())
    }

    fn count(&self) -> Result<u64, PersistentStoreError> {
        Ok(self.views.len() as u64)
    }

    fn list_all(&self) -> Result<Vec<PersistedView>, PersistentStoreError> {
        Ok(self.views.values().cloned().collect())
    }

    fn put(&mut self, view: PersistedView) -> Result<(), PersistentStoreError> {
        self.views.insert(view.descriptor.view_id, view);
        Ok(())
    }

    fn remove(&mut self, id: ViewId) -> Result<bool, PersistentStoreError> {
        Ok(self.views.remove(&id).is_some())
    }

    fn last_applied_seq(&self) -> Result<SequenceNumber, PersistentStoreError> {
        Ok(self.last_applied_seq)
    }

    fn set_last_applied_seq(&mut self, seq: SequenceNumber) -> Result<(), PersistentStoreError> {
        self.last_applied_seq = seq;
        Ok(())
    }
}
