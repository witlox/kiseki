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
use kiseki_log::intent::PerspectiveSeq;

use super::error::PersistentStoreError;
use crate::composition::Composition;

/// ADR-047 MF-1 / MF-9 — per-name LWW guard predicate. Returns `true`
/// iff a bind / unbind with the `incoming` perspective seq should be
/// applied over the `stored` perspective seq.
///
/// Cross-surface rule (see [`CompositionStorage::name_insert_with_seq`]
/// for the long-form contract):
///
/// - `stored = _`, `incoming = None` → accept. Sync surface; Raft
///   commit order is authoritative; unconditional bind.
/// - `stored = None`, `incoming = Some(_)` → accept. `None < Some(_)`,
///   so any async write beats the (implicit) sync floor.
/// - `stored = Some(s_old)`, `incoming = Some(s_new)` → accept iff
///   `s_new >= s_old`, per-key LWW by HLC seq.
///
/// Equality (`s_new == s_old`) accepts so a retry of the same write
/// is idempotent. The skip path (`s_new < s_old`) is the load-bearing
/// case from MF-1: a newer-HLC write incorporated FIRST must not be
/// overwritten by an older-HLC write incorporated SECOND.
#[must_use]
pub fn lww_accepts_bind(stored: Option<PerspectiveSeq>, incoming: Option<PerspectiveSeq>) -> bool {
    match (stored, incoming) {
        // Sync (incoming = None) always wins — unconditional bind.
        // Same accept reasoning covers None stored + Some incoming
        // (None < Some(_)): the merged arm keeps clippy happy.
        (_, None) | (None, Some(_)) => true,
        // Per-key LWW by HLC perspective seq, equality idempotent.
        (Some(s_old), Some(s_new)) => s_new >= s_old,
    }
}

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
    ///
    /// Cross-surface seq contract (ADR-047 MF-1 / MF-9): plain
    /// `name_insert` is the sync surface path (POSIX / NFS / FUSE
    /// plus the gateway's own atomic create-then-name). It binds
    /// unconditionally and clears any stored perspective seq for the
    /// name — sync writes are Raft-commit-order authoritative and
    /// the next async write (with `Some(seq)`) wins the LWW guard
    /// against `None` (we treat `None` as `-∞`). The asynchronous
    /// (decoupled-ack) producer uses [`Self::name_insert_with_seq`]
    /// which applies the seq guard.
    fn name_insert(
        &self,
        ns: NamespaceId,
        name: String,
        id: CompositionId,
    ) -> Result<(), PersistentStoreError>;

    /// Bind `name` to `id` in `ns` with an ingress-assigned
    /// [`PerspectiveSeq`] (ADR-047 MF-1 / MF-9 LWW guard).
    ///
    /// `incoming = None` is treated as **sync surface** and binds
    /// unconditionally, identical to plain [`Self::name_insert`].
    ///
    /// `incoming = Some(seq)` is treated as **async surface** and
    /// binds iff `Some(seq) >= stored_seq` per the per-name LWW
    /// guard (`None < Some(_)` is the cross-surface ordering: a
    /// stored `None` from a prior sync write **always loses** to an
    /// incoming async seq — last-writer-wins, where the sync write
    /// "landed at unspecified time" and the async write has a
    /// real HLC). If the incoming seq is strictly older than the
    /// stored one, the bind is a no-op (the durable state is
    /// already the LWW winner).
    ///
    /// Returns `Ok(true)` when the bind happened, `Ok(false)` when
    /// the seq guard skipped it (the stored seq was strictly newer).
    ///
    /// Default implementation: reads the stored seq, applies the
    /// rule, and calls into [`Self::name_insert`] + records the new
    /// seq via [`Self::name_seq_record`]. Persistent backends MAY
    /// override to fold the lookup + write into one batch.
    fn name_insert_with_seq(
        &self,
        ns: NamespaceId,
        name: String,
        id: CompositionId,
        incoming: Option<PerspectiveSeq>,
    ) -> Result<bool, PersistentStoreError> {
        let stored = self.name_seq_lookup(ns, &name)?;
        if !lww_accepts_bind(stored, incoming) {
            return Ok(false);
        }
        self.name_insert(ns, name.clone(), id)?;
        self.name_seq_record(ns, name, incoming)?;
        Ok(true)
    }

    /// Read the perspective seq stored alongside `(ns, name)`.
    /// Returns `None` when no binding exists OR when the existing
    /// binding has no recorded seq (sync-bound: a write that landed
    /// through the synchronous gateway path, or one whose stamp was
    /// cleared by a sync overwrite). Per the LWW rule a stored `None`
    /// is treated as `-∞`, so any incoming `Some(_)` wins.
    fn name_seq_lookup(
        &self,
        ns: NamespaceId,
        name: &str,
    ) -> Result<Option<PerspectiveSeq>, PersistentStoreError>;

    /// Record (or clear) the perspective seq for `(ns, name)`. Called
    /// from [`Self::name_insert_with_seq`] after the bind succeeds. A
    /// `seq = None` argument CLEARS the recorded seq for the name —
    /// the sync-surface "raft commit order is authoritative" stamp.
    fn name_seq_record(
        &self,
        ns: NamespaceId,
        name: String,
        seq: Option<PerspectiveSeq>,
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
    ///
    /// **Cross-surface seq contract:** plain `name_remove` is the
    /// **sync surface** (POSIX / NFS / FUSE DELETE) path — unbinds
    /// unconditionally and clears any stored seq. The asynchronous
    /// (decoupled-ack) hydrator path uses
    /// [`Self::name_remove_with_seq`] which applies the LWW guard
    /// (an older-seq Delete must not unbind a newer-seq Create's
    /// binding).
    fn name_remove(&self, ns: NamespaceId, name: &str) -> Result<bool, PersistentStoreError>;

    /// Unbind `name` in `ns` with an ingress-assigned
    /// [`PerspectiveSeq`] guard. Same cross-surface rule as
    /// [`Self::name_insert_with_seq`]: `incoming = None` is sync
    /// (unconditional unbind), `incoming = Some(seq)` is async and
    /// only unbinds iff `Some(seq) >= stored_seq` (treating stored
    /// `None` as `-∞`).
    ///
    /// Returns `Ok(true)` when the unbind happened, `Ok(false)`
    /// when the seq guard skipped it.
    ///
    /// Default implementation: read stored seq, apply rule, call
    /// [`Self::name_remove`] + clear via [`Self::name_seq_record`].
    fn name_remove_with_seq(
        &self,
        ns: NamespaceId,
        name: &str,
        incoming: Option<PerspectiveSeq>,
    ) -> Result<bool, PersistentStoreError> {
        let stored = self.name_seq_lookup(ns, name)?;
        if !lww_accepts_bind(stored, incoming) {
            return Ok(false);
        }
        let removed = self.name_remove(ns, name)?;
        // Clear the seq row even when the binding row was absent —
        // keeps the two indexes consistent on a stale unbind replay.
        self.name_seq_record(ns, name.to_owned(), None)?;
        Ok(removed)
    }

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
    ///
    /// Used by the gateway's read-path 503 short-circuit, which fires
    /// only on lookup-miss (`guard.get(composition_id)` returned
    /// `NotFound`). At that point the gateway doesn't know which shard
    /// the missing composition belongs to — composition records are
    /// keyed by `CompositionId` (a `UUIDv4`, not derived from a routable
    /// key) and don't carry a `shard_id` field. Per-composition shard
    /// resolution would require a data-model change (adding `shard_id`
    /// to `Composition`, snapshot-format bump, migration) whose cost
    /// exceeds the residual blast-radius savings now that the trip
    /// predicate (I-CP5) requires positive compaction evidence.
    ///
    /// New code that has `shard_id` in scope (the hydrator, the
    /// per-shard `/cluster/shards/{id}/leader` health endpoint) should
    /// use the shard-scoped [`halted`] accessor directly. `halted_any`
    /// is intentionally the gateway's coarse signal; the per-shard
    /// storage layout (I-CP5b) still bounds the durable halt state.
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
    /// Name bindings to insert: `(namespace_id, name, composition_id,
    /// perspective_seq)`. Populated from Create deltas that carry a
    /// name (S3 PUT path). Followers replay these so GET-by-key +
    /// LIST work uniformly across nodes.
    ///
    /// The fourth tuple element carries the optional ingress
    /// [`PerspectiveSeq`] decoded from the Create payload (ADR-047
    /// MF-1 / MF-9). `Some(seq)` engages the per-name LWW guard at
    /// apply time; `None` (sync surfaces) binds unconditionally.
    pub name_inserts: Vec<(NamespaceId, String, CompositionId, Option<PerspectiveSeq>)>,
    /// Name bindings to remove: `(namespace_id, name,
    /// perspective_seq)`. Populated from Delete deltas via
    /// reverse-lookup of the composition's current name binding.
    /// The hydrator resolves the name on the leader (or via its own
    /// local `name_for` lookup) before emitting the batch.
    ///
    /// `perspective_seq = Some(seq)` engages the per-name LWW guard
    /// — an older-seq Delete must not unbind a newer-seq Create's
    /// binding (MF-1). `None` unbinds unconditionally (sync surface).
    pub name_removes: Vec<(NamespaceId, String, Option<PerspectiveSeq>)>,
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
    /// ADR-047 MF-1: per-name perspective-seq stamp for the LWW
    /// guard. `Some(_)` = the binding was last touched by an async
    /// (decoupled-ack) write at that seq; absent = the binding was
    /// sync-bound (or the stamp was cleared by a sync overwrite).
    /// Lives in its own map so a sync write that goes through
    /// `name_insert` keeps the existing fast path (clears the seq
    /// via `name_seq_record(None)`).
    name_seqs: parking_lot::Mutex<HashMap<(NamespaceId, String), PerspectiveSeq>>,
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
            name_seqs: parking_lot::Mutex::new(HashMap::new()),
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
        // dangling composition_id. The reverse map AND the per-name
        // seq stamp stay consistent.
        if let Some((ns, name)) = self.names_reverse.lock().remove(&id) {
            self.names.lock().remove(&(ns, name.clone()));
            self.name_seqs.lock().remove(&(ns, name));
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
        //
        // **Cross-surface seq contract (ADR-047 MF-9):** this is the
        // sync-surface path — unconditional bind + clear the stored
        // perspective-seq stamp (so the next async write with
        // `Some(seq)` wins LWW; `None` is treated as `-∞` against
        // any incoming async seq).
        let mut names = self.names.lock();
        let mut names_reverse = self.names_reverse.lock();
        let mut name_seqs = self.name_seqs.lock();
        if let Some(old_id) = names.get(&(ns, name.clone())).copied() {
            if old_id != id {
                names_reverse.remove(&old_id);
            }
        }
        if let Some((old_ns, old_name)) = names_reverse.get(&id).cloned() {
            if old_ns != ns || old_name != name {
                names.remove(&(old_ns, old_name.clone()));
                name_seqs.remove(&(old_ns, old_name));
            }
        }
        names.insert((ns, name.clone()), id);
        names_reverse.insert(id, (ns, name.clone()));
        // Sync write: clear the seq stamp.
        name_seqs.remove(&(ns, name));
        Ok(())
    }

    fn name_seq_lookup(
        &self,
        ns: NamespaceId,
        name: &str,
    ) -> Result<Option<PerspectiveSeq>, PersistentStoreError> {
        Ok(self.name_seqs.lock().get(&(ns, name.to_owned())).copied())
    }

    fn name_seq_record(
        &self,
        ns: NamespaceId,
        name: String,
        seq: Option<PerspectiveSeq>,
    ) -> Result<(), PersistentStoreError> {
        let mut name_seqs = self.name_seqs.lock();
        let key = (ns, name);
        match seq {
            Some(s) => {
                name_seqs.insert(key, s);
            }
            None => {
                name_seqs.remove(&key);
            }
        }
        Ok(())
    }

    fn name_remove(&self, ns: NamespaceId, name: &str) -> Result<bool, PersistentStoreError> {
        let key = (ns, name.to_owned());
        let mut names = self.names.lock();
        let mut names_reverse = self.names_reverse.lock();
        let mut name_seqs = self.name_seqs.lock();
        // Sync unbind: clear the seq stamp unconditionally.
        name_seqs.remove(&key);
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
        let mut name_seqs = self.name_seqs.lock();
        for comp in batch.puts {
            compositions.insert(comp.id, comp);
        }
        for id in batch.removes {
            // Drop any name binding for the removed composition first
            // so the forward index can't outlive the data row.
            if let Some((ns, name)) = names_reverse.remove(&id) {
                names.remove(&(ns, name.clone()));
                name_seqs.remove(&(ns, name));
            }
            compositions.remove(&id);
        }
        // ADR-047 MF-1 / MF-9 — per-name LWW guard. For each insert,
        // bind iff `lww_accepts_bind(stored_seq, incoming_seq)`. The
        // sync-surface case (`incoming = None`) binds unconditionally
        // AND clears the stored seq (`-∞` stamp); async (`Some(seq)`)
        // updates the stamp on accept.
        for (ns, name, id, incoming_seq) in batch.name_inserts {
            let key = (ns, name.clone());
            let stored = name_seqs.get(&key).copied();
            if !lww_accepts_bind(stored, incoming_seq) {
                tracing::debug!(
                    ns = %ns.0,
                    name = %name,
                    stored = ?stored,
                    incoming = ?incoming_seq,
                    "name-bind: skipped (LWW guard: incoming seq < stored)",
                );
                continue;
            }
            // Overwrite-replace cascade (matches `name_insert`).
            if let Some(old_id) = names.get(&key).copied() {
                if old_id != id {
                    names_reverse.remove(&old_id);
                }
            }
            if let Some((old_ns, old_name)) = names_reverse.get(&id).cloned() {
                if old_ns != ns || old_name != name {
                    names.remove(&(old_ns, old_name.clone()));
                    name_seqs.remove(&(old_ns, old_name));
                }
            }
            names.insert(key.clone(), id);
            names_reverse.insert(id, (ns, name));
            match incoming_seq {
                Some(s) => {
                    name_seqs.insert(key, s);
                }
                None => {
                    name_seqs.remove(&key);
                }
            }
        }
        for (ns, name, incoming_seq) in batch.name_removes {
            let key = (ns, name.clone());
            let stored = name_seqs.get(&key).copied();
            if !lww_accepts_bind(stored, incoming_seq) {
                tracing::debug!(
                    ns = %ns.0,
                    name = %name,
                    stored = ?stored,
                    incoming = ?incoming_seq,
                    "name-unbind: skipped (LWW guard: incoming seq < stored)",
                );
                continue;
            }
            if let Some(id) = names.remove(&key) {
                names_reverse.remove(&id);
            }
            // Clear the seq stamp on accept.
            name_seqs.remove(&key);
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

// ---------------------------------------------------------------------------
// ADR-047 MF-1 / MF-9 — per-name perspective-seq LWW guard tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod lww_guard_tests {
    use super::*;
    use kiseki_common::ids::NodeId;
    use kiseki_common::time::HybridLogicalClock;

    fn ns() -> NamespaceId {
        NamespaceId(uuid::Uuid::from_u128(2))
    }
    fn id(n: u128) -> CompositionId {
        CompositionId(uuid::Uuid::from_u128(n))
    }
    fn seq(physical_ms: u64, logical: u32, node: u64) -> PerspectiveSeq {
        PerspectiveSeq(HybridLogicalClock {
            physical_ms,
            logical,
            node_id: NodeId(node),
        })
    }

    // --- Pure predicate tests (lww_accepts_bind) --------------------------

    #[test]
    fn lww_predicate_table() {
        // sync incoming always wins
        assert!(lww_accepts_bind(None, None));
        assert!(lww_accepts_bind(Some(seq(1, 0, 1)), None));
        // None stored, Some incoming → accept (None < Some(_))
        assert!(lww_accepts_bind(None, Some(seq(1, 0, 1))));
        // Equal seqs → idempotent accept
        assert!(lww_accepts_bind(Some(seq(5, 0, 1)), Some(seq(5, 0, 1))));
        // Newer incoming → accept
        assert!(lww_accepts_bind(Some(seq(5, 0, 1)), Some(seq(6, 0, 1))));
        // Older incoming → reject
        assert!(!lww_accepts_bind(Some(seq(6, 0, 1)), Some(seq(5, 0, 1))));
        // Same physical_ms, newer logical → accept
        assert!(lww_accepts_bind(Some(seq(5, 0, 1)), Some(seq(5, 1, 1))));
        // Same physical_ms, older logical → reject
        assert!(!lww_accepts_bind(Some(seq(5, 1, 1)), Some(seq(5, 0, 1))));
    }

    // --- MemoryStorage bind/unbind paths ----------------------------------

    #[test]
    fn bind_guard_async_newer_wins() {
        let store = MemoryStorage::new();
        let ns = ns();
        // First: bind name with seq=2
        assert!(store
            .name_insert_with_seq(ns, "k".into(), id(1), Some(seq(2, 0, 1)))
            .unwrap());
        assert_eq!(store.name_lookup(ns, "k").unwrap(), Some(id(1)));
        // Then: bind same name with seq=3, different id → wins
        assert!(store
            .name_insert_with_seq(ns, "k".into(), id(2), Some(seq(3, 0, 1)))
            .unwrap());
        assert_eq!(store.name_lookup(ns, "k").unwrap(), Some(id(2)));
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), Some(seq(3, 0, 1)));
    }

    #[test]
    fn bind_guard_async_older_skipped() {
        let store = MemoryStorage::new();
        let ns = ns();
        // First: bind with seq=3
        assert!(store
            .name_insert_with_seq(ns, "k".into(), id(1), Some(seq(3, 0, 1)))
            .unwrap());
        // Then: try to bind with seq=2 → REJECTED
        assert!(!store
            .name_insert_with_seq(ns, "k".into(), id(2), Some(seq(2, 0, 1)))
            .unwrap());
        // Lookup still resolves to original id; stored seq unchanged
        assert_eq!(store.name_lookup(ns, "k").unwrap(), Some(id(1)));
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), Some(seq(3, 0, 1)));
    }

    #[test]
    fn bind_guard_equal_seq_idempotent() {
        let store = MemoryStorage::new();
        let ns = ns();
        assert!(store
            .name_insert_with_seq(ns, "k".into(), id(1), Some(seq(3, 0, 1)))
            .unwrap());
        // Same seq, same id → idempotent accept (a retry of the same write).
        assert!(store
            .name_insert_with_seq(ns, "k".into(), id(1), Some(seq(3, 0, 1)))
            .unwrap());
        assert_eq!(store.name_lookup(ns, "k").unwrap(), Some(id(1)));
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), Some(seq(3, 0, 1)));
    }

    #[test]
    fn bind_guard_sync_always_binds() {
        let store = MemoryStorage::new();
        let ns = ns();
        // sync writes through name_insert (the plain sync entry point).
        store.name_insert(ns, "k".into(), id(1)).unwrap();
        store.name_insert(ns, "k".into(), id(2)).unwrap();
        // Second sync write wins — raft commit order is authoritative.
        assert_eq!(store.name_lookup(ns, "k").unwrap(), Some(id(2)));
        // Sync clears the seq stamp.
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), None);
    }

    #[test]
    fn bind_guard_sync_then_async_async_wins() {
        let store = MemoryStorage::new();
        let ns = ns();
        // Sync first: no seq stamped.
        store.name_insert(ns, "k".into(), id(1)).unwrap();
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), None);
        // Async with Some(seq) → None < Some(_), so async wins.
        assert!(store
            .name_insert_with_seq(ns, "k".into(), id(2), Some(seq(5, 0, 1)))
            .unwrap());
        assert_eq!(store.name_lookup(ns, "k").unwrap(), Some(id(2)));
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), Some(seq(5, 0, 1)));
    }

    #[test]
    fn bind_guard_async_then_sync_sync_wins() {
        let store = MemoryStorage::new();
        let ns = ns();
        // Async first: stamp seq=5.
        assert!(store
            .name_insert_with_seq(ns, "k".into(), id(1), Some(seq(5, 0, 1)))
            .unwrap());
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), Some(seq(5, 0, 1)));
        // Sync (None) → unconditional bind. Sync wins, stamp cleared.
        store.name_insert(ns, "k".into(), id(2)).unwrap();
        assert_eq!(store.name_lookup(ns, "k").unwrap(), Some(id(2)));
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), None);
    }

    #[test]
    fn stage_delete_guard_skips_older() {
        // An older-seq async unbind must NOT unbind a newer-seq name.
        // Hydration apply path: feed in a `name_removes` entry with an
        // older seq than what's stored — expect the unbind to be
        // dropped silently.
        let store = MemoryStorage::new();
        let ns = ns();
        // Bind with seq=5
        assert!(store
            .name_insert_with_seq(ns, "k".into(), id(1), Some(seq(5, 0, 1)))
            .unwrap());
        // Apply a hydration batch with an OLDER-seq unbind → no-op.
        let batch = HydrationBatch {
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            puts: Vec::new(),
            removes: Vec::new(),
            name_inserts: Vec::new(),
            name_removes: vec![(ns, "k".into(), Some(seq(2, 0, 1)))],
            new_last_applied_seq: SequenceNumber(0),
            stuck_state: None,
            halted: None,
        };
        store.apply_hydration_batch(batch).unwrap();
        // Binding survives.
        assert_eq!(store.name_lookup(ns, "k").unwrap(), Some(id(1)));
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), Some(seq(5, 0, 1)));
    }

    #[test]
    fn hydration_batch_lww_guard_drops_older_insert() {
        // The mirror case of stage_delete_guard_skips_older for inserts:
        // a hydration apply with an older-seq Create must not regress
        // the newer-seq binding.
        let store = MemoryStorage::new();
        let ns = ns();
        assert!(store
            .name_insert_with_seq(ns, "k".into(), id(1), Some(seq(5, 0, 1)))
            .unwrap());
        let batch = HydrationBatch {
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            puts: Vec::new(),
            removes: Vec::new(),
            name_inserts: vec![(ns, "k".into(), id(2), Some(seq(2, 0, 1)))],
            name_removes: Vec::new(),
            new_last_applied_seq: SequenceNumber(0),
            stuck_state: None,
            halted: None,
        };
        store.apply_hydration_batch(batch).unwrap();
        assert_eq!(store.name_lookup(ns, "k").unwrap(), Some(id(1)));
        assert_eq!(store.name_seq_lookup(ns, "k").unwrap(), Some(seq(5, 0, 1)));
    }
}
