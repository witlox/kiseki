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

    // -- Name index (per-bucket key → composition_id, S3 semantics) --
    //
    // The name index gives the S3 PUT/GET/DELETE/LIST path real
    // key-based naming on top of the composition store. Without it,
    // every PUT just creates a fresh composition UUID and the URL
    // `key` is ignored — making `If-None-Match: *`, GET-by-key and
    // DELETE-by-key impossible to express. The hydrator updates the
    // index from the Create delta's optional `name` field so
    // followers see the same key→id mapping as the leader.

    /// Resolve `(namespace_id, name)` → composition_id. Returns
    /// `None` if no composition is bound to that name in the namespace.
    fn name_lookup(
        &self,
        ns: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, PersistentStoreError>;

    /// Reverse lookup: composition_id → `(namespace_id, name)`.
    /// Returns `None` if the composition was created without a name
    /// (internal / NFS path) or has been unbound.
    fn name_for(
        &self,
        id: CompositionId,
    ) -> Result<Option<(NamespaceId, String)>, PersistentStoreError>;

    /// Bind `name` to `id` in `ns`. Overwrites any existing binding
    /// (S3 PUT-overwrite semantics — the caller is responsible for
    /// having checked conditional headers like `If-None-Match: *`
    /// before calling this).
    fn name_insert(
        &mut self,
        ns: NamespaceId,
        name: String,
        id: CompositionId,
    ) -> Result<(), PersistentStoreError>;

    /// Unbind `name` in `ns`. Returns `true` if a binding existed.
    fn name_remove(&mut self, ns: NamespaceId, name: &str) -> Result<bool, PersistentStoreError>;

    /// Enumerate `(name, composition_id)` bindings in a namespace.
    /// `prefix` filters by string prefix when `Some` (S3 LIST with
    /// `?prefix=`).
    fn name_list(
        &self,
        ns: NamespaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, CompositionId)>, PersistentStoreError>;

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
    /// Name bindings to insert: `(namespace_id, name, composition_id)`.
    /// Populated from Create deltas that carry a name (S3 PUT path).
    /// Followers replay these so GET-by-key + LIST work uniformly
    /// across nodes.
    pub name_inserts: Vec<(NamespaceId, String, CompositionId)>,
    /// Name bindings to remove: `(namespace_id, name)`. Populated
    /// from Delete deltas via reverse-lookup of the composition's
    /// current name binding. The hydrator resolves the name on the
    /// leader (or via its own local `name_for` lookup) before
    /// emitting the batch.
    pub name_removes: Vec<(NamespaceId, String)>,
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
            name_inserts: Vec::new(),
            name_removes: Vec::new(),
            new_last_applied_seq,
            stuck_state: Some(None),
            halted: None,
        }
    }

    /// True if the batch has any data changes (vs. just meta updates).
    #[must_use]
    pub fn has_data_changes(&self) -> bool {
        !self.puts.is_empty()
            || !self.removes.is_empty()
            || !self.name_inserts.is_empty()
            || !self.name_removes.is_empty()
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
    /// Name index forward: (namespace_id, name) → composition_id.
    /// Maintained alongside the composition table; persisted as part
    /// of `apply_hydration_batch` on the leader and updated atomically
    /// with the underlying composition mutations on followers via
    /// `name_inserts` / `name_removes`.
    names: HashMap<(NamespaceId, String), CompositionId>,
    /// Name index reverse: composition_id → (namespace_id, name). Used
    /// by Delete deltas to find what to unbind. A composition without
    /// a name (NFS path, internal use) has no entry here.
    names_reverse: HashMap<CompositionId, (NamespaceId, String)>,
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
            names: HashMap::new(),
            names_reverse: HashMap::new(),
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
        // Drop the name binding when the composition goes away —
        // otherwise a future PUT to the same key would resolve to a
        // dangling composition_id. The reverse map stays consistent.
        if let Some((ns, name)) = self.names_reverse.remove(&id) {
            self.names.remove(&(ns, name));
        }
        Ok(self.compositions.remove(&id).is_some())
    }

    fn name_lookup(
        &self,
        ns: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, PersistentStoreError> {
        Ok(self.names.get(&(ns, name.to_owned())).copied())
    }

    fn name_for(
        &self,
        id: CompositionId,
    ) -> Result<Option<(NamespaceId, String)>, PersistentStoreError> {
        Ok(self.names_reverse.get(&id).cloned())
    }

    fn name_insert(
        &mut self,
        ns: NamespaceId,
        name: String,
        id: CompositionId,
    ) -> Result<(), PersistentStoreError> {
        // Overwrite-replace: if name already binds to a different
        // composition, drop the old reverse entry. If id already has a
        // name, drop its old forward entry. Caller is responsible for
        // pre-flight conditional checks (If-None-Match etc.).
        if let Some(old_id) = self.names.get(&(ns, name.clone())).copied() {
            if old_id != id {
                self.names_reverse.remove(&old_id);
            }
        }
        if let Some((old_ns, old_name)) = self.names_reverse.get(&id).cloned() {
            if old_ns != ns || old_name != name {
                self.names.remove(&(old_ns, old_name));
            }
        }
        self.names.insert((ns, name.clone()), id);
        self.names_reverse.insert(id, (ns, name));
        Ok(())
    }

    fn name_remove(&mut self, ns: NamespaceId, name: &str) -> Result<bool, PersistentStoreError> {
        let key = (ns, name.to_owned());
        if let Some(id) = self.names.remove(&key) {
            self.names_reverse.remove(&id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn name_list(
        &self,
        ns: NamespaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, CompositionId)>, PersistentStoreError> {
        let mut out: Vec<(String, CompositionId)> = self
            .names
            .iter()
            .filter(|((n, name), _)| {
                *n == ns && prefix.is_none_or(|p| name.starts_with(p))
            })
            .map(|((_, name), id)| (name.clone(), *id))
            .collect();
        // Stable order — S3 LIST ordering is alphabetical.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
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
            // Drop any name binding for the removed composition first
            // so the forward index can't outlive the data row.
            if let Some((ns, name)) = self.names_reverse.remove(&id) {
                self.names.remove(&(ns, name));
            }
            self.compositions.remove(&id);
        }
        for (ns, name, id) in batch.name_inserts {
            // Reuse the same overwrite-replace semantics as
            // `name_insert` so a redo of the same Create delta on a
            // restarted hydrator stays idempotent.
            if let Some(old_id) = self.names.get(&(ns, name.clone())).copied() {
                if old_id != id {
                    self.names_reverse.remove(&old_id);
                }
            }
            if let Some((old_ns, old_name)) = self.names_reverse.get(&id).cloned() {
                if old_ns != ns || old_name != name {
                    self.names.remove(&(old_ns, old_name));
                }
            }
            self.names.insert((ns, name.clone()), id);
            self.names_reverse.insert(id, (ns, name));
        }
        for (ns, name) in batch.name_removes {
            let key = (ns, name);
            if let Some(id) = self.names.remove(&key) {
                self.names_reverse.remove(&id);
            }
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
