//! `CompositionStorage` trait + `MemoryStorage` impl (ADR-040).
//!
//! The trait is the seam where `CompositionStore` decouples its
//! `comp_id` → `Composition` map from the in-memory `HashMap`. Two
//! implementations satisfy it:
//!
//!   - `MemoryStorage` — `HashMap` + plain fields. Used by tests and
//!     by single-node deployments without `KISEKI_DATA_DIR`. Keeps
//!     existing behavior bit-compatible.
//!   - `FjallStorage` (in `fjall.rs`) — fjall-backed (LSM keyspace
//!     with WAL). Replaced redb on the write-heavy path 2026-05-06
//!     per ADR-022's escape clause; metric and durability knobs are
//!     unchanged.
//!
//! Hydrator state (`last_applied_seq`, `stuck_at_seq`,
//! `stuck_retries`, `halted`) lives in the storage so the persistent
//! backend can commit data + meta in a single atomic batch (I-CP1).

use std::collections::HashMap;

use kiseki_common::ids::{CompositionId, NamespaceId, SequenceNumber, ShardId};

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
    /// revision; the current `FjallStorage` impl does a full
    /// partition scan.
    fn list_in_namespace(&self, ns: NamespaceId) -> Result<Vec<Composition>, PersistentStoreError>;

    /// Insert or replace a composition. Used by both the gateway
    /// (`create` / `update` / `set_content_type`) and the hydrator
    /// (when applying a single op outside the batch path).
    ///
    /// `&self` rather than `&mut self`: persistent backends rely on
    /// fjall's internal journal-mutex for serialization, and the
    /// in-memory backend uses interior mutability. This lets
    /// concurrent writers operate on the store without an outer
    /// Mutex around the trait object — fjall's per-write-batch
    /// commit is still atomic.
    fn put(&self, comp: Composition) -> Result<(), PersistentStoreError>;

    /// Remove a composition. Returns `true` if it existed.
    fn remove(&self, id: CompositionId) -> Result<bool, PersistentStoreError>;

    // -- Name index (per-bucket key → composition_id, S3 semantics) --
    //
    // The name index gives the S3 PUT/GET/DELETE/LIST path real
    // key-based naming on top of the composition store. Without it,
    // every PUT just creates a fresh composition UUID and the URL
    // `key` is ignored — making `If-None-Match: *`, GET-by-key and
    // DELETE-by-key impossible to express. The hydrator updates the
    // index from the Create delta's optional `name` field so
    // followers see the same key→id mapping as the leader.

    /// Resolve `(namespace_id, name)` → `composition_id`. Returns
    /// `None` if no composition is bound to that name in the namespace.
    fn name_lookup(
        &self,
        ns: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, PersistentStoreError>;

    /// Reverse lookup: `composition_id` → `(namespace_id, name)`.
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
        &self,
        ns: NamespaceId,
        name: String,
        id: CompositionId,
    ) -> Result<(), PersistentStoreError>;

    /// Atomic create-then-name. Stores `comp` AND binds `(ns, name)`
    /// → `comp.id` in a single backend batch — one journal-mutex
    /// acquisition + one fsync instead of two. The persistent
    /// backend MUST commit both atomically (no observable state
    /// where the composition exists but the name is missing, or
    /// vice versa).
    ///
    /// Caller MUST guarantee `comp.id` is freshly minted (never
    /// previously bound to a name); the backend is allowed to skip
    /// the reverse-name pre-flight read on that basis. The forward
    /// `(ns, name)` cascade-replace check still runs because the
    /// caller may not have pre-validated the binding (S3 PUT-
    /// overwrite without `If-None-Match`).
    ///
    /// Atomic create-then-name. Stores `comp` AND binds `(ns, name)`
    /// → `comp.id` in a single backend batch — one journal-mutex
    /// acquisition + one fsync instead of two. The persistent
    /// backend MUST commit both atomically (no observable state
    /// where the composition exists but the name is missing, or
    /// vice versa).
    ///
    /// `prior_id` is the existing `(ns, name) → comp_id` binding
    /// observed by the caller under the storage lock (or `None`
    /// when no binding exists). The backend uses this to drive the
    /// overwrite-replace cascade (drop the stale reverse for the
    /// old `comp_id`) without paying its own pre-flight read.
    /// Callers that don't already hold the lookup result pass
    /// whatever they have; the persistent backend may do its own
    /// lookup if `prior_id` is unreliable, but the
    /// gateway-driven path always supplies it from the same
    /// storage-critical-section that drives this call.
    ///
    /// Caller MUST guarantee `comp.id` is freshly minted (never
    /// previously bound to a name). The forward `(ns, name)`
    /// cascade-replace check uses `prior_id`; the reverse-name
    /// pre-flight on `comp.id` is skipped on that basis.
    ///
    /// Default implementation: sequential `put` + `name_insert`,
    /// ignoring `prior_id` (in-memory backends do their cascade
    /// inside `name_insert` anyway, where the lookup is O(1) and
    /// not worth bypassing). Persistent backends override to fold
    /// both into one batch and consume `prior_id`.
    fn put_with_name(
        &self,
        comp: Composition,
        ns: NamespaceId,
        name: String,
        _prior_id: Option<CompositionId>,
    ) -> Result<(), PersistentStoreError> {
        let id = comp.id;
        self.put(comp)?;
        self.name_insert(ns, name, id)
    }

    /// Unbind `name` in `ns`. Returns `true` if a binding existed.
    fn name_remove(&self, ns: NamespaceId, name: &str) -> Result<bool, PersistentStoreError>;

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
    /// for the given shard. The hydrator for that shard polls
    /// `read_deltas(from = last_applied_seq(shard) + 1, ...)`.
    ///
    /// Per-shard: each Raft shard has its own sequence space, so the
    /// "high water mark" must be keyed by shard. A new shard returns
    /// `SequenceNumber(0)` until its first hydration batch commits.
    fn last_applied_seq(&self, shard_id: ShardId) -> Result<SequenceNumber, PersistentStoreError>;

    /// Per-stuck-delta retry counter (I-1 / N-1 closure), keyed by
    /// shard. Returns `(stuck_at_seq, retries)` if a delta is
    /// currently being retried on that shard; `None` once it
    /// succeeds or is promoted to a permanent skip. Persisted in
    /// the same backend batch as `last_applied_seq` so a crash-loop
    /// accumulates retries reliably across restarts.
    fn stuck_state(
        &self,
        shard_id: ShardId,
    ) -> Result<Option<(SequenceNumber, u32)>, PersistentStoreError>;

    /// Halt-mode flag, **per-shard** (I-CP5b, amended 2026-05-19 for
    /// issue #87 PR-2). When `true` for a given shard, the gateway
    /// returns 503 for composition lookups whose `composition_id` maps
    /// to that shard, instead of 404. Halts on other shards are
    /// independent — reads of unrelated shards' compositions
    /// continue normally. The hydrator sets it for one shard when
    /// §D6.3's gap-detection rule fires on that shard.
    ///
    /// Before PR-2 this was a single node-global bool. Production
    /// incident 2026-05-19 (issue #87) demonstrated that the
    /// node-wide blast amplified a per-shard transient into a
    /// cluster-wide read 503; per-shard scoping bounds the
    /// blast-radius to the actually-affected shard.
    fn halted(&self, shard_id: ShardId) -> Result<bool, PersistentStoreError>;

    /// True iff any shard on this node is currently halted.
    /// Temporary helper (issue #87 PR-2) for callers that don't yet
    /// route reads through the `composition_id → shard_id` resolver.
    /// The gateway's read-path 503 short-circuit uses this until
    /// PR-3 wires per-composition shard resolution. New code should
    /// prefer the shard-scoped [`halted`] accessor.
    fn halted_any(&self) -> Result<bool, PersistentStoreError>;

    /// Apply a hydrator batch atomically. The persistent backend
    /// commits all inserts + removes + meta updates in a single
    /// atomic batch (I-CP1). `batch.shard_id` scopes the
    /// `last_applied_seq` / `stuck_state` meta updates.
    fn apply_hydration_batch(&self, batch: HydrationBatch) -> Result<(), PersistentStoreError>;
}

/// One hydrator-poll's worth of state changes. Applied atomically by
/// `apply_hydration_batch`.
#[derive(Debug)]
pub struct HydrationBatch {
    /// Which shard's hydrator produced this batch. Scopes the
    /// `new_last_applied_seq` + `stuck_state` meta updates so
    /// per-shard hydrators don't stomp on each other's high-water
    /// marks (ADR-040, multi-shard hydrator extension).
    pub shard_id: ShardId,
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
    /// Empty batch advancing the named shard's `last_applied_seq` to
    /// the given value, clearing stuck state. Used when every delta
    /// in the poll applied cleanly.
    #[must_use]
    pub fn advance(shard_id: ShardId, new_last_applied_seq: SequenceNumber) -> Self {
        Self {
            shard_id,
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
///
/// All fields use interior mutability so the `CompositionStorage`
/// trait can take `&self` on writes (the persistent fjall backend
/// natively supports `&self` thanks to fjall's internal journal
/// mutex; the in-memory backend mirrors that contract via
/// `parking_lot::Mutex` here).
pub struct MemoryStorage {
    compositions: parking_lot::Mutex<HashMap<CompositionId, Composition>>,
    /// Name index forward: (`namespace_id`, name) → `composition_id`.
    /// Maintained alongside the composition table; persisted as part
    /// of `apply_hydration_batch` on the leader and updated atomically
    /// with the underlying composition mutations on followers via
    /// `name_inserts` / `name_removes`.
    names: parking_lot::Mutex<HashMap<(NamespaceId, String), CompositionId>>,
    /// Name index reverse: `composition_id` → (`namespace_id`, name). Used
    /// by Delete deltas to find what to unbind. A composition without
    /// a name (NFS path, internal use) has no entry here.
    names_reverse: parking_lot::Mutex<HashMap<CompositionId, (NamespaceId, String)>>,
    /// Per-shard last-applied sequence. Multi-shard hydrators write
    /// disjoint keys.
    last_applied_seq: parking_lot::Mutex<HashMap<ShardId, SequenceNumber>>,
    /// Per-shard stuck-delta retry counter.
    stuck_state: parking_lot::Mutex<HashMap<ShardId, Option<(SequenceNumber, u32)>>>,
    /// Halt flag is **per-shard** (I-CP5b, issue #87 PR-2). A shard
    /// not present in the map is treated as not-halted.
    halted: parking_lot::Mutex<HashMap<ShardId, bool>>,
}

impl std::fmt::Debug for MemoryStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStorage")
            .field("compositions_len", &self.compositions.lock().len())
            .field("names_len", &self.names.lock().len())
            .finish_non_exhaustive()
    }
}

impl MemoryStorage {
    /// Construct an empty in-memory storage.
    #[must_use]
    pub fn new() -> Self {
        Self {
            compositions: parking_lot::Mutex::new(HashMap::new()),
            names: parking_lot::Mutex::new(HashMap::new()),
            names_reverse: parking_lot::Mutex::new(HashMap::new()),
            last_applied_seq: parking_lot::Mutex::new(HashMap::new()),
            stuck_state: parking_lot::Mutex::new(HashMap::new()),
            halted: parking_lot::Mutex::new(HashMap::new()),
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
        Ok(self.compositions.lock().get(&id).cloned())
    }

    fn count(&self) -> Result<u64, PersistentStoreError> {
        Ok(self.compositions.lock().len() as u64)
    }

    fn list_in_namespace(&self, ns: NamespaceId) -> Result<Vec<Composition>, PersistentStoreError> {
        Ok(self
            .compositions
            .lock()
            .values()
            .filter(|c| c.namespace_id == ns)
            .cloned()
            .collect())
    }

    fn put(&self, comp: Composition) -> Result<(), PersistentStoreError> {
        self.compositions.lock().insert(comp.id, comp);
        Ok(())
    }

    fn remove(&self, id: CompositionId) -> Result<bool, PersistentStoreError> {
        // Drop the name binding when the composition goes away —
        // otherwise a future PUT to the same key would resolve to a
        // dangling composition_id. The reverse map stays consistent.
        if let Some((ns, name)) = self.names_reverse.lock().remove(&id) {
            self.names.lock().remove(&(ns, name));
        }
        Ok(self.compositions.lock().remove(&id).is_some())
    }

    fn name_lookup(
        &self,
        ns: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, PersistentStoreError> {
        Ok(self.names.lock().get(&(ns, name.to_owned())).copied())
    }

    fn name_for(
        &self,
        id: CompositionId,
    ) -> Result<Option<(NamespaceId, String)>, PersistentStoreError> {
        Ok(self.names_reverse.lock().get(&id).cloned())
    }

    fn name_insert(
        &self,
        ns: NamespaceId,
        name: String,
        id: CompositionId,
    ) -> Result<(), PersistentStoreError> {
        // Overwrite-replace: if name already binds to a different
        // composition, drop the old reverse entry. If id already has a
        // name, drop its old forward entry. Caller is responsible for
        // pre-flight conditional checks (If-None-Match etc.).
        let mut names = self.names.lock();
        let mut names_reverse = self.names_reverse.lock();
        if let Some(old_id) = names.get(&(ns, name.clone())).copied() {
            if old_id != id {
                names_reverse.remove(&old_id);
            }
        }
        if let Some((old_ns, old_name)) = names_reverse.get(&id).cloned() {
            if old_ns != ns || old_name != name {
                names.remove(&(old_ns, old_name));
            }
        }
        names.insert((ns, name.clone()), id);
        names_reverse.insert(id, (ns, name));
        Ok(())
    }

    fn name_remove(&self, ns: NamespaceId, name: &str) -> Result<bool, PersistentStoreError> {
        let key = (ns, name.to_owned());
        let mut names = self.names.lock();
        let mut names_reverse = self.names_reverse.lock();
        if let Some(id) = names.remove(&key) {
            names_reverse.remove(&id);
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
            .lock()
            .iter()
            .filter(|((n, name), _)| *n == ns && prefix.is_none_or(|p| name.starts_with(p)))
            .map(|((_, name), id)| (name.clone(), *id))
            .collect();
        // Stable order — S3 LIST ordering is alphabetical.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn last_applied_seq(&self, shard_id: ShardId) -> Result<SequenceNumber, PersistentStoreError> {
        Ok(self
            .last_applied_seq
            .lock()
            .get(&shard_id)
            .copied()
            .unwrap_or(SequenceNumber(0)))
    }

    fn stuck_state(
        &self,
        shard_id: ShardId,
    ) -> Result<Option<(SequenceNumber, u32)>, PersistentStoreError> {
        Ok(self
            .stuck_state
            .lock()
            .get(&shard_id)
            .copied()
            .unwrap_or(None))
    }

    fn halted(&self, shard_id: ShardId) -> Result<bool, PersistentStoreError> {
        Ok(self.halted.lock().get(&shard_id).copied().unwrap_or(false))
    }

    fn halted_any(&self) -> Result<bool, PersistentStoreError> {
        Ok(self.halted.lock().values().any(|&v| v))
    }

    fn apply_hydration_batch(&self, batch: HydrationBatch) -> Result<(), PersistentStoreError> {
        let mut compositions = self.compositions.lock();
        let mut names = self.names.lock();
        let mut names_reverse = self.names_reverse.lock();
        for comp in batch.puts {
            compositions.insert(comp.id, comp);
        }
        for id in batch.removes {
            // Drop any name binding for the removed composition first
            // so the forward index can't outlive the data row.
            if let Some((ns, name)) = names_reverse.remove(&id) {
                names.remove(&(ns, name));
            }
            compositions.remove(&id);
        }
        for (ns, name, id) in batch.name_inserts {
            // Reuse the same overwrite-replace semantics as
            // `name_insert` so a redo of the same Create delta on a
            // restarted hydrator stays idempotent.
            if let Some(old_id) = names.get(&(ns, name.clone())).copied() {
                if old_id != id {
                    names_reverse.remove(&old_id);
                }
            }
            if let Some((old_ns, old_name)) = names_reverse.get(&id).cloned() {
                if old_ns != ns || old_name != name {
                    names.remove(&(old_ns, old_name));
                }
            }
            names.insert((ns, name.clone()), id);
            names_reverse.insert(id, (ns, name));
        }
        for (ns, name) in batch.name_removes {
            let key = (ns, name);
            if let Some(id) = names.remove(&key) {
                names_reverse.remove(&id);
            }
        }
        self.last_applied_seq
            .lock()
            .insert(batch.shard_id, batch.new_last_applied_seq);
        if let Some(stuck) = batch.stuck_state {
            self.stuck_state.lock().insert(batch.shard_id, stuck);
        }
        if let Some(halted) = batch.halted {
            self.halted.lock().insert(batch.shard_id, halted);
        }
        Ok(())
    }
}
