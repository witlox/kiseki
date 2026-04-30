//! `CompositionStorage` trait + `MemoryStorage` impl (ADR-040).
//!
//! The trait is the seam where `CompositionStore` decouples its
//! `comp_id` → `Composition` map from the in-memory `HashMap`. Two
//! implementations satisfy it:
//!
//!   - `MemoryStorage` — `HashMap` + plain fields. Used by tests and
//!     by single-node deployments without `KISEKI_DATA_DIR`. Keeps
//!     existing behavior bit-compatible.
//!   - `PersistentRedbStorage` (in `redb.rs`) — redb-backed.
//!
//! Hydrator state (`last_applied_seq`, `stuck_at_seq`,
//! `stuck_retries`, `halted`) lives in the storage so the persistent
//! backend can commit data + meta in a single transaction (I-CP1).

use std::collections::HashMap;

use kiseki_common::ids::{CompositionId, NamespaceId, SequenceNumber};

use super::error::PersistentStoreError;
use crate::composition::Composition;

/// Storage backend for the `comp_id` → `Composition` map plus
/// hydrator meta state.
pub trait CompositionStorage: Send + Sync {
    /// Retrieve a composition by id. Returns `None` if absent.
    fn get(&self, id: CompositionId) -> Result<Option<Composition>, PersistentStoreError>;

    /// Number of compositions currently stored. May not be cheap on a
    /// persistent backend — call sparingly.
    fn count(&self) -> Result<u64, PersistentStoreError>;

    /// All compositions in a namespace. Used by the bucket-list path.
    /// Persistent backend implementations are encouraged to maintain
    /// a (`namespace_id` → `comp_id`) secondary index in a future
    /// revision; the v1 redb impl does a full table scan.
    fn list_in_namespace(&self, ns: NamespaceId) -> Result<Vec<Composition>, PersistentStoreError>;

    /// Insert or replace a composition. Used by both the gateway
    /// (`create` / `update` / `set_content_type`) and the hydrator
    /// (when applying a single op outside the batch path).
    fn put(&mut self, comp: Composition) -> Result<(), PersistentStoreError>;

    /// Remove a composition. Returns `true` if it existed.
    fn remove(&mut self, id: CompositionId) -> Result<bool, PersistentStoreError>;

    // -- Hydrator meta state (ADR-040 §D5, §D5.1, §D6.3, I-CP1, I-CP6) --

    /// Highest delta sequence whose state has been durably applied
    /// to this store. The hydrator polls
    /// `read_deltas(from = last_applied_seq + 1, ...)`.
    fn last_applied_seq(&self) -> Result<SequenceNumber, PersistentStoreError>;

    /// Per-stuck-delta retry counter (I-1 / N-1 closure). Returns
    /// `(stuck_at_seq, retries)` if a delta is currently being
    /// retried; `None` once it succeeds or is promoted to a permanent
    /// skip. Persisted in the same redb transaction as
    /// `last_applied_seq` so a crash-loop accumulates retries
    /// reliably across restarts.
    fn stuck_state(&self) -> Result<Option<(SequenceNumber, u32)>, PersistentStoreError>;

    /// Halt-mode flag. When `true`, the gateway returns 503 for
    /// composition-not-found lookups instead of 404 (I-2). The
    /// hydrator sets this when §D6.3's gap-detection rule fires.
    fn halted(&self) -> Result<bool, PersistentStoreError>;

    /// Apply a hydrator batch atomically. The persistent backend
    /// commits all inserts + removes + meta updates in a single
    /// redb transaction (I-CP1).
    fn apply_hydration_batch(&mut self, batch: HydrationBatch) -> Result<(), PersistentStoreError>;
}

/// One hydrator-poll's worth of state changes. Applied atomically by
/// `apply_hydration_batch`.
#[derive(Debug)]
pub struct HydrationBatch {
    /// Compositions to insert (Create deltas) or replace (Update
    /// deltas — Update applies as a `put` since the new
    /// `Composition` already has the bumped `version` and updated
    /// `chunks`/`size`).
    pub puts: Vec<Composition>,
    /// Composition ids to remove (Delete deltas).
    pub removes: Vec<CompositionId>,
    /// Advance `last_applied_seq` to this value. Always set; the
    /// hydrator never commits a batch without advancing.
    pub new_last_applied_seq: SequenceNumber,
    /// Update the stuck-state. `Some(Some(_))` sets a new value,
    /// `Some(None)` clears it, `None` leaves it unchanged.
    pub stuck_state: Option<Option<(SequenceNumber, u32)>>,
    /// Update the halt flag. `None` leaves it unchanged.
    pub halted: Option<bool>,
}

impl HydrationBatch {
    /// Empty batch advancing to the given seq, clearing stuck state.
    /// Used when every delta in the poll applied cleanly.
    #[must_use]
    pub fn advance(new_last_applied_seq: SequenceNumber) -> Self {
        Self {
            puts: Vec::new(),
            removes: Vec::new(),
            new_last_applied_seq,
            stuck_state: Some(None),
            halted: None,
        }
    }

    /// True if the batch has any data changes (vs. just meta updates).
    #[must_use]
    pub fn has_data_changes(&self) -> bool {
        !self.puts.is_empty() || !self.removes.is_empty()
    }
}

// ---------------------------------------------------------------------------
// In-memory backend — HashMap + plain fields. Bit-compatible with the
// pre-ADR-040 CompositionStore behavior.
// ---------------------------------------------------------------------------

/// In-memory `CompositionStorage` for tests and single-node clusters.
#[derive(Debug)]
pub struct MemoryStorage {
    compositions: HashMap<CompositionId, Composition>,
    last_applied_seq: SequenceNumber,
    stuck_state: Option<(SequenceNumber, u32)>,
    halted: bool,
}

impl MemoryStorage {
    /// Construct an empty in-memory storage.
    #[must_use]
    pub fn new() -> Self {
        Self {
            compositions: HashMap::new(),
            last_applied_seq: SequenceNumber(0),
            stuck_state: None,
            halted: false,
        }
    }
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl CompositionStorage for MemoryStorage {
    fn get(&self, id: CompositionId) -> Result<Option<Composition>, PersistentStoreError> {
        Ok(self.compositions.get(&id).cloned())
    }

    fn count(&self) -> Result<u64, PersistentStoreError> {
        Ok(self.compositions.len() as u64)
    }

    fn list_in_namespace(&self, ns: NamespaceId) -> Result<Vec<Composition>, PersistentStoreError> {
        Ok(self
            .compositions
            .values()
            .filter(|c| c.namespace_id == ns)
            .cloned()
            .collect())
    }

    fn put(&mut self, comp: Composition) -> Result<(), PersistentStoreError> {
        self.compositions.insert(comp.id, comp);
        Ok(())
    }

    fn remove(&mut self, id: CompositionId) -> Result<bool, PersistentStoreError> {
        Ok(self.compositions.remove(&id).is_some())
    }

    fn last_applied_seq(&self) -> Result<SequenceNumber, PersistentStoreError> {
        Ok(self.last_applied_seq)
    }

    fn stuck_state(&self) -> Result<Option<(SequenceNumber, u32)>, PersistentStoreError> {
        Ok(self.stuck_state)
    }

    fn halted(&self) -> Result<bool, PersistentStoreError> {
        Ok(self.halted)
    }

    fn apply_hydration_batch(&mut self, batch: HydrationBatch) -> Result<(), PersistentStoreError> {
        for comp in batch.puts {
            self.compositions.insert(comp.id, comp);
        }
        for id in batch.removes {
            self.compositions.remove(&id);
        }
        self.last_applied_seq = batch.new_last_applied_seq;
        if let Some(stuck) = batch.stuck_state {
            self.stuck_state = stuck;
        }
        if let Some(halted) = batch.halted {
            self.halted = halted;
        }
        Ok(())
    }
}
