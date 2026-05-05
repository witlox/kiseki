//! redb-backed `CompositionStorage` impl (ADR-040).
//!
//! Storage layout (single redb file, two tables):
//!
//!   COMPOSITIONS: `CompositionId` (16 bytes) → `[1 byte version][postcard(Composition)]`
//!   META:         `&str` → variable bytes (see `meta_keys`)
//!
//! Locks (ADR-040 §D4):
//!   - `Mutex<Database>` — sync, held only for the duration of a redb
//!     transaction. **Never held across an `await`.**
//!   - `Mutex<LruCache<CompositionId, Composition>>` — sync, held only
//!     for cache get/insert. **Never held across an `await`.**
//!
//! The outer `tokio::sync::Mutex<dyn CompositionStorage>` owned by the
//! gateway is the only lock that crosses awaits (ADR-032 / ADR-040 §D4).

use std::path::Path;
use std::sync::Mutex;

use ::redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use kiseki_common::ids::{CompositionId, NamespaceId, SequenceNumber};
use lru::LruCache;

use super::error::PersistentStoreError;
use super::storage::{CompositionStorage, HydrationBatch};
use crate::composition::Composition;
use kiseki_common::locks::LockOrDie;

// -- Schema -----------------------------------------------------------------

/// Current on-disk schema version. Bumped on incompatible changes per
/// ADR-040 §D8.
pub const COMPOSITION_RECORD_SCHEMA_VERSION: u8 = 1;

/// Compositions: `comp_id.0.as_bytes()` → `[version][postcard]`.
pub(crate) const COMPOSITIONS: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("compositions");

/// Meta: see `meta_keys` for the namespace.
const META: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("meta");

/// Name index forward: 16-byte `ns_id` || name → 16-byte `composition_id`.
/// Encoding the namespace as a fixed prefix gives us free
/// per-namespace range scans for `name_list` (and future
/// LIST-with-prefix). Lexicographic order on `name` is the natural S3
/// LIST ordering.
pub(crate) const NAMES: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("names");

/// Name index reverse: 16-byte `composition_id` → 16-byte `ns_id` || name.
/// Used by Delete to drop the forward binding without scanning.
pub(crate) const NAMES_REVERSE: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("names_reverse");

mod meta_keys {
    pub const SCHEMA_VERSION: &str = "schema_version";
    pub const LAST_APPLIED_SEQ: &str = "last_applied_seq";
    pub const STUCK_STATE: &str = "stuck_state";
    pub const HALTED: &str = "halted";
}

const DEFAULT_LRU_CAPACITY: usize = 100_000;

// -- Encoding helpers -------------------------------------------------------

/// `[1 byte: version][postcard payload]` — see ADR-040 §D2.
pub(crate) fn encode_composition(
    comp: &Composition,
) -> Result<Vec<u8>, PersistentStoreError> {
    let mut out = Vec::with_capacity(280);
    out.push(COMPOSITION_RECORD_SCHEMA_VERSION);
    let payload = postcard::to_stdvec(comp)?;
    out.extend_from_slice(&payload);
    Ok(out)
}

fn decode_composition(bytes: &[u8]) -> Result<Composition, PersistentStoreError> {
    let Some((&version, payload)) = bytes.split_first() else {
        return Err(PersistentStoreError::Decode("empty record".to_owned()));
    };
    if version > COMPOSITION_RECORD_SCHEMA_VERSION {
        return Err(PersistentStoreError::SchemaTooNew {
            found: version,
            supported: COMPOSITION_RECORD_SCHEMA_VERSION,
        });
    }
    Ok(postcard::from_bytes(payload)?)
}

fn encode_stuck_state(state: Option<(SequenceNumber, u32)>) -> Vec<u8> {
    match state {
        None => Vec::new(), // empty value => not stuck
        Some((seq, retries)) => {
            let mut out = Vec::with_capacity(12);
            out.extend_from_slice(&seq.0.to_le_bytes());
            out.extend_from_slice(&retries.to_le_bytes());
            out
        }
    }
}

/// Encode a (`namespace_id`, name) tuple as a flat key for the NAMES
/// table. Layout: 16 bytes `ns_id` || UTF-8 name. Namespace prefix
/// gives free per-namespace range scans.
pub(crate) fn name_key(ns: NamespaceId, name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + name.len());
    out.extend_from_slice(ns.0.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

/// Decode a flat key from the `NAMES_REVERSE` value field back into
/// `(NamespaceId, name)`. Mirror of `name_key`.
pub(crate) fn decode_name_key(bytes: &[u8]) -> Result<(NamespaceId, String), String> {
    if bytes.len() < 16 {
        return Err(format!("name key too short: {}", bytes.len()));
    }
    let mut ns_buf = [0u8; 16];
    ns_buf.copy_from_slice(&bytes[..16]);
    let name = std::str::from_utf8(&bytes[16..])
        .map_err(|e| format!("name utf8: {e}"))?
        .to_owned();
    Ok((NamespaceId(uuid::Uuid::from_bytes(ns_buf)), name))
}

fn decode_stuck_state(bytes: &[u8]) -> Result<Option<(SequenceNumber, u32)>, PersistentStoreError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() != 12 {
        return Err(PersistentStoreError::Decode(format!(
            "stuck_state has wrong length: {}",
            bytes.len()
        )));
    }
    let mut seq_bytes = [0u8; 8];
    seq_bytes.copy_from_slice(&bytes[0..8]);
    let mut retry_bytes = [0u8; 4];
    retry_bytes.copy_from_slice(&bytes[8..12]);
    Ok(Some((
        SequenceNumber(u64::from_le_bytes(seq_bytes)),
        u32::from_le_bytes(retry_bytes),
    )))
}

/// Three-valued result of a write-behind overlay lookup. Spelled
/// out as an enum because the `Option<Option<T>>` form trips
/// clippy's `option_option` lint and obscures intent.
enum OverlayLookup<T> {
    /// Overlay has the value; reader returns it without consulting redb.
    Hit(T),
    /// Overlay tombstoned this key; reader returns "not found"
    /// without consulting redb (the redb row is stale).
    Tombstone,
    /// Overlay doesn't know about this key; reader falls through to redb.
    Miss,
}

// -- Storage struct ---------------------------------------------------------

/// redb-backed `CompositionStorage`.
///
/// `eventual_durability` controls whether `txn.commit()` calls
/// downgrade to `Durability::Eventual` (no inline fsync). When set,
/// the runtime spawns a periodic flush task that issues a real
/// fsync via [`Self::flush`] so disk state stays at most
/// `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS` behind the in-memory
/// state. Mirrors the chunk-store group-commit pattern from
/// `681de37` — same FUSE p99 fix rationale, just on the
/// composition redb instead of the chunk-store device.
///
/// Crash safety: in multi-node deployments Raft replication
/// recovers ≤ flush_interval-ms of recently-committed compositions
/// via the under-replication scrub on restart. Single-node
/// deployments lose them — `eventual_durability` should stay off
/// for those (default).
pub struct PersistentRedbStorage {
    db: std::sync::Arc<Mutex<Database>>,
    cache: Mutex<LruCache<CompositionId, Composition>>,
    metrics: Option<std::sync::Arc<crate::metrics::CompositionMetrics>>,
    eventual_durability: bool,
    /// rev-3 write-behind handle. `Some` when group commit is on
    /// AND `enable_write_behind` has been called (which the runtime
    /// does at startup before handing the storage to the gateway).
    /// `None` keeps the rev-1/2 synchronous path unchanged.
    write_behind: Option<super::write_behind::WriteBehindHandle>,
}

/// Shared, clone-able handle for forcing a real fsync on the
/// composition redb. Built via [`PersistentRedbStorage::flusher`]
/// and handed to the runtime's periodic flush task.
///
/// Holds an `Arc<Mutex<Database>>` clone — same database as the
/// owning storage. The `flush()` call takes the mutex briefly,
/// issues a no-op `Immediate`-durability commit, and releases.
#[derive(Clone)]
pub struct RedbFlusher {
    db: std::sync::Arc<Mutex<Database>>,
    metrics: Option<std::sync::Arc<crate::metrics::CompositionMetrics>>,
}

impl RedbFlusher {
    /// Force an `Immediate`-durability commit on the underlying
    /// redb so any pending `Durability::None` commits land on
    /// disk. The runtime's periodic flush task calls this.
    ///
    /// # Errors
    /// Returns `PersistentStoreError` if the redb commit fails.
    pub fn flush(&self) -> Result<(), PersistentStoreError> {
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_write()?;
        // No mutation — the commit-with-Immediate is the fsync
        // trigger. redb's commit path handles the WAL flush.
        if let Err(e) = txn.commit() {
            if let Some(m) = self.metrics.as_ref() {
                m.redb_commit_errors_total.inc();
            }
            return Err(e.into());
        }
        Ok(())
    }
}

impl PersistentRedbStorage {
    /// Open or create a redb file at `path` with the default LRU
    /// capacity (100,000 entries; tunable via env in a future revision).
    pub fn open(path: &Path) -> Result<Self, PersistentStoreError> {
        Self::open_with_lru_capacity(path, DEFAULT_LRU_CAPACITY)
    }

    /// Attach the §D10 metrics surface. When set, `get`/`put`/
    /// `apply_hydration_batch` paths emit hit/miss/evict/commit
    /// counters and `decode_errors_total{kind}` on failures.
    /// Tests that don't pass metrics get no-op behavior.
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: std::sync::Arc<crate::metrics::CompositionMetrics>,
    ) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Enable group-commit durability — every `txn.commit()` runs at
    /// `Durability::Eventual` (no inline fsync). The runtime is
    /// expected to drive a periodic [`Self::flush`] (default 100 ms)
    /// to keep disk state fresh.
    ///
    /// Mirrors the chunk-store `set_sync_per_write(false)` knob
    /// from `681de37`. Same crash-safety contract: in multi-node
    /// deployments, Raft replication recovers ≤ flush_interval-ms
    /// of compositions via the under-replication scrub. Single-
    /// node deployments lose them on hard crash — only enable when
    /// the cluster has ≥2 replicas of the composition delta.
    #[must_use]
    pub fn with_eventual_durability(mut self, enabled: bool) -> Self {
        self.eventual_durability = enabled;
        self
    }

    /// Build a clone-able [`RedbFlusher`] handle for the periodic
    /// flush task. The returned handle shares the underlying redb
    /// `Database` (via `Arc<Mutex<…>>`) so concurrent flushes from
    /// the spawn task and writes from the data path serialize on
    /// the same mutex.
    #[must_use]
    pub fn flusher(&self) -> RedbFlusher {
        RedbFlusher {
            db: std::sync::Arc::clone(&self.db),
            metrics: self.metrics.clone(),
        }
    }

    /// Apply the configured durability mode to a freshly-begun
    /// write transaction. Called immediately after `db.begin_write`
    /// in every mutating method. Centralizes the policy so adding
    /// a new mutating method only needs one call.
    ///
    /// redb 4.x exposes only `None` and `Immediate` — `None` is
    /// the "skip the fsync, persisted on next `Immediate` commit"
    /// mode, semantically identical to the older `Eventual`. The
    /// runtime's periodic `flush()` task issues an `Immediate`
    /// commit at the configured cadence to bound the loss window.
    fn apply_durability(&self, txn: &mut redb::WriteTransaction) {
        if self.eventual_durability {
            // `set_durability` returns Result in redb 4.x — only
            // errors when the txn is already committed/aborted,
            // which can't happen on a freshly-begun transaction.
            // Log + continue rather than fail the put.
            if let Err(e) = txn.set_durability(redb::Durability::None) {
                tracing::warn!(
                    error = %e,
                    "redb set_durability(None) failed — falling back to Immediate",
                );
            }
        }
    }

    /// Open with an explicit LRU capacity (for tests).
    pub fn open_with_lru_capacity(
        path: &Path,
        lru_capacity: usize,
    ) -> Result<Self, PersistentStoreError> {
        let db = Database::create(path)?;

        // Initialize tables and write schema_version on first boot.
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(COMPOSITIONS)?;
            let _ = txn.open_table(NAMES)?;
            let _ = txn.open_table(NAMES_REVERSE)?;
            let mut meta = txn.open_table(META)?;
            if meta.get(meta_keys::SCHEMA_VERSION)?.is_none() {
                meta.insert(
                    meta_keys::SCHEMA_VERSION,
                    [COMPOSITION_RECORD_SCHEMA_VERSION].as_slice(),
                )?;
            } else {
                // Existing redb. Verify schema_version is supported.
                let v = meta.get(meta_keys::SCHEMA_VERSION)?.ok_or_else(|| {
                    PersistentStoreError::Decode(
                        "schema_version missing after presence-check".into(),
                    )
                })?;
                let bytes = v.value();
                if bytes.is_empty() {
                    return Err(PersistentStoreError::Decode("schema_version empty".into()));
                }
                let version = bytes[0];
                if version > COMPOSITION_RECORD_SCHEMA_VERSION {
                    return Err(PersistentStoreError::SchemaTooNew {
                        found: version,
                        supported: COMPOSITION_RECORD_SCHEMA_VERSION,
                    });
                }
            }
        }
        txn.commit()?;

        let cache = LruCache::new(
            std::num::NonZeroUsize::new(lru_capacity)
                .unwrap_or(std::num::NonZeroUsize::new(1).expect("1 is non-zero by construction")),
        );
        Ok(Self {
            db: std::sync::Arc::new(Mutex::new(db)),
            cache: Mutex::new(cache),
            metrics: None,
            eventual_durability: false,
            write_behind: None,
        })
    }

    /// Enable the rev-3 write-behind queue and return a drainer
    /// handle the runtime should `tokio::spawn` to flush batches
    /// periodically. Caller-supplied `max_queue_size` is the
    /// soft-cap above which writers fall back to the inline redb
    /// path; `interval` is the periodic flush cadence; `max_batch`
    /// is the per-flush op count cap.
    ///
    /// Idempotent at the API level — calling twice replaces the
    /// existing handle (and any pending overlay entries from the
    /// old one are leaked). Runtime calls this exactly once at
    /// startup, before the storage is handed to the gateway.
    pub fn enable_write_behind(
        &mut self,
        max_queue_size: usize,
        interval: std::time::Duration,
        max_batch: usize,
    ) -> super::write_behind::WriteBehindDrainer {
        let handle = super::write_behind::WriteBehindHandle::new(max_queue_size);
        self.write_behind = Some(handle.clone());
        super::write_behind::WriteBehindDrainer::new(
            handle,
            std::sync::Arc::clone(&self.db),
            self.metrics.clone(),
            interval,
            max_batch,
        )
    }

    /// Look up a composition in the overlay. Three-valued result:
    /// `Hit(c)` = overlay has the value; `Tombstone` = overlay says
    /// "removed, ignore redb"; `Miss` = overlay doesn't know,
    /// caller must consult redb.
    fn overlay_get(&self, id: CompositionId) -> OverlayLookup<Composition> {
        let Some(handle) = self.write_behind.as_ref() else {
            return OverlayLookup::Miss;
        };
        let ov = handle.overlay.read();
        match ov.comp.get(&id) {
            None => OverlayLookup::Miss,
            Some((_, None)) => OverlayLookup::Tombstone,
            Some((_, Some(c))) => OverlayLookup::Hit(c.clone()),
        }
    }

    /// Look up a forward name binding in the overlay.
    fn overlay_name_lookup(
        &self,
        ns: NamespaceId,
        name: &str,
    ) -> OverlayLookup<CompositionId> {
        let Some(handle) = self.write_behind.as_ref() else {
            return OverlayLookup::Miss;
        };
        let ov = handle.overlay.read();
        match ov.name_fwd.get(&(ns, name.to_owned())) {
            None => OverlayLookup::Miss,
            Some((_, None)) => OverlayLookup::Tombstone,
            Some((_, Some(id))) => OverlayLookup::Hit(*id),
        }
    }

    /// Look up a reverse name binding in the overlay.
    fn overlay_name_for(&self, id: CompositionId) -> OverlayLookup<(NamespaceId, String)> {
        let Some(handle) = self.write_behind.as_ref() else {
            return OverlayLookup::Miss;
        };
        let ov = handle.overlay.read();
        match ov.name_rev.get(&id) {
            None => OverlayLookup::Miss,
            Some((_, None)) => OverlayLookup::Tombstone,
            Some((_, Some(k))) => OverlayLookup::Hit(k.clone()),
        }
    }

    fn record_decode_error(&self, e: &PersistentStoreError) {
        if let Some(ref m) = self.metrics {
            m.decode_errors_total
                .with_label_values(&[e.metric_kind()])
                .inc();
        }
    }

    fn record_commit_error(&self) {
        if let Some(ref m) = self.metrics {
            m.redb_commit_errors_total.inc();
        }
    }

    fn record_eviction(&self, evicted: bool) {
        if evicted {
            if let Some(ref m) = self.metrics {
                m.lru_evicted_total.inc();
            }
        }
    }

    /// Translates `LruCache::push`'s tri-valued return into "was it a
    /// real capacity eviction?" — `Some((k, _))` with `k != inserted`
    /// means the LRU pushed out a different key. `Some((k, _))` with
    /// `k == inserted` is just a same-key replace and isn't an eviction.
    /// Takes a reference so we don't pay for an unused `Composition`
    /// clone in the not-evicted hot path.
    fn is_capacity_eviction(
        push_result: Option<&(CompositionId, Composition)>,
        inserted: CompositionId,
    ) -> bool {
        matches!(push_result, Some((k, _)) if *k != inserted)
    }
}

impl CompositionStorage for PersistentRedbStorage {
    fn get(&self, id: CompositionId) -> Result<Option<Composition>, PersistentStoreError> {
        // I-CP9: write-behind overlay takes precedence over both
        // the LRU cache and redb. A pending put or a pending
        // tombstone must be observable here even though the redb
        // commit hasn't landed yet.
        match self.overlay_get(id) {
            OverlayLookup::Hit(c) => return Ok(Some(c)),
            OverlayLookup::Tombstone => return Ok(None),
            OverlayLookup::Miss => {}
        }
        // Cache lookup first — sync mutex, brief.
        if let Some(comp) = self
            .cache
            .lock()
            .lock_or_die("redb.cache")
            .get(&id)
            .cloned()
        {
            if let Some(ref m) = self.metrics {
                m.lru_hit_total.inc();
            }
            return Ok(Some(comp));
        }
        if let Some(ref m) = self.metrics {
            m.lru_miss_total.inc();
        }
        // redb miss path.
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_read()?;
        let table = txn.open_table(COMPOSITIONS)?;
        let key = id.0.as_bytes().as_slice();
        let Some(guard) = table.get(key)? else {
            return Ok(None);
        };
        let comp = decode_composition(guard.value()).inspect_err(|e| {
            self.record_decode_error(e);
        })?;
        // Populate the LRU under its own mutex (no overlap with the
        // db mutex hold since redb's read txn doesn't need it).
        let evicted = self
            .cache
            .lock()
            .lock_or_die("redb.cache")
            .push(id, comp.clone());
        if evicted.is_some() {
            if let Some(ref m) = self.metrics {
                m.lru_evicted_total.inc();
            }
        }
        Ok(Some(comp))
    }

    fn count(&self) -> Result<u64, PersistentStoreError> {
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_read()?;
        let table = txn.open_table(COMPOSITIONS)?;
        Ok(table.len()?)
    }

    fn list_in_namespace(&self, ns: NamespaceId) -> Result<Vec<Composition>, PersistentStoreError> {
        // I-CP9 LIST consistency: merge redb scan with the overlay.
        // - Overlay tombstone (None) hides the redb row.
        // - Overlay Some(comp) replaces the redb row for the same id.
        // - Overlay Some(comp) for an id absent from redb is added.
        let mut out: Vec<Composition> = Vec::new();
        // Overlay snapshot for this namespace (cheap — short read lock).
        let (overlay_keep, overlay_extra): (
            std::collections::HashMap<CompositionId, Option<Composition>>,
            Vec<Composition>,
        ) = if let Some(handle) = &self.write_behind {
            let ov = handle.overlay.read();
            let mut keep = std::collections::HashMap::new();
            let mut extra = Vec::new();
            for (id, (_, payload)) in &ov.comp {
                match payload {
                    Some(c) if c.namespace_id == ns => {
                        keep.insert(*id, Some(c.clone()));
                        extra.push(c.clone());
                    }
                    Some(_) => {
                        // Other namespace — overlay still hides any
                        // redb row with the same id (e.g. moved between
                        // namespaces). Mark as "skip" without an extra.
                        keep.insert(*id, None);
                    }
                    None => {
                        keep.insert(*id, None);
                    }
                }
            }
            (keep, extra)
        } else {
            (std::collections::HashMap::new(), Vec::new())
        };
        // v1: full table scan. ADR-040 calls out that a future
        // revision adds a (namespace_id → comp_id) secondary index.
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_read()?;
        let table = txn.open_table(COMPOSITIONS)?;
        for entry in table.iter()? {
            let (_, value) = entry?;
            let comp = decode_composition(value.value())?;
            if comp.namespace_id != ns {
                continue;
            }
            // Both `Some(Some(_))` (overlay supplied a fresher copy
            // via `extra`) and `Some(None)` (tombstone / moved
            // namespace) mean "skip the redb row." Only an
            // overlay-miss falls through to the push.
            if !overlay_keep.contains_key(&comp.id) {
                out.push(comp);
            }
        }
        out.extend(overlay_extra);
        Ok(out)
    }

    fn put(&mut self, comp: Composition) -> Result<(), PersistentStoreError> {
        let id = comp.id;
        // rev-3: route through the overlay when write-behind is on
        // AND the queue isn't saturated. On saturation we fall back
        // to the inline path so the queue can drain.
        if let Some(handle) = &self.write_behind {
            let mut ov = handle.overlay.write();
            if ov.len() < handle.max_queue_size {
                let seq = ov.next_seq();
                ov.comp.insert(id, (seq, Some(comp.clone())));
                drop(ov);
                handle.notify.notify_one();
                // Cache update mirrors the inline path — readers
                // following this put can still see the value via
                // the overlay (which takes precedence), but
                // populating the cache lets the post-flush hot path
                // serve from cache without re-reading redb.
                let push_result = self.cache.lock().lock_or_die("redb.cache").push(id, comp);
                self.record_eviction(Self::is_capacity_eviction(push_result.as_ref(), id));
                return Ok(());
            }
            // Saturated: fall through to inline.
        }
        let bytes = encode_composition(&comp).inspect_err(|e| {
            self.record_decode_error(e);
        })?;
        {
            let db = self.db.lock().lock_or_die("redb.db");
            let mut txn = db.begin_write()?;
            self.apply_durability(&mut txn);
            {
                let mut table = txn.open_table(COMPOSITIONS)?;
                table.insert(id.0.as_bytes().as_slice(), bytes.as_slice())?;
            }
            txn.commit().inspect_err(|_| self.record_commit_error())?;
        }
        // Cache update happens *after* commit so a reader that sees
        // the cache value also sees the durable record (D3).
        let push_result = self.cache.lock().lock_or_die("redb.cache").push(id, comp);
        self.record_eviction(Self::is_capacity_eviction(push_result.as_ref(), id));
        Ok(())
    }

    fn remove(&mut self, id: CompositionId) -> Result<bool, PersistentStoreError> {
        // rev-3 write-behind path: stamp a tombstone in the overlay
        // and let the drainer clean redb. Existence is best-effort
        // — overlay doesn't always know whether redb still holds
        // the row, so we approximate "did it exist?" as "either
        // the overlay had a non-tombstone OR redb has the row."
        if let Some(handle) = &self.write_behind {
            let prior_overlay = match self.overlay_get(id) {
                OverlayLookup::Hit(c) => Some(c),
                _ => None,
            };
            let mut ov = handle.overlay.write();
            if ov.len() < handle.max_queue_size {
                let seq = ov.next_seq();
                ov.comp.insert(id, (seq, None));
                // Drop the reverse-name binding (and forward, if we
                // know it). The overlay is consulted before redb on
                // both lookups.
                let prior_name = ov
                    .name_rev
                    .get(&id)
                    .and_then(|(_, payload)| payload.clone());
                if let Some(name_key) = prior_name.clone() {
                    let nseq = ov.next_seq();
                    ov.name_fwd.insert(name_key, (nseq, None));
                }
                let rseq = ov.next_seq();
                ov.name_rev.insert(id, (rseq, None));
                drop(ov);
                handle.notify.notify_one();
                self.cache.lock().lock_or_die("redb.cache").pop(&id);
                // Existence flag: overlay said "yes" → existed; else
                // peek redb for a more accurate answer.
                if prior_overlay.is_some() {
                    return Ok(true);
                }
                let exists = {
                    let db = self.db.lock().lock_or_die("redb.db");
                    let txn = db.begin_read()?;
                    let table = txn.open_table(COMPOSITIONS)?;
                    table.get(id.0.as_bytes().as_slice())?.is_some()
                };
                return Ok(exists);
            }
            // Saturated → fall through.
        }
        let existed = {
            let db = self.db.lock().lock_or_die("redb.db");
            let mut txn = db.begin_write()?;
            self.apply_durability(&mut txn);
            let existed = {
                let mut table = txn.open_table(COMPOSITIONS)?;
                let removed = table.remove(id.0.as_bytes().as_slice())?;
                removed.is_some()
            };
            // Drop the name binding atomically with the composition
            // row so a forward `name_lookup` after the txn can't see
            // a key pointing at a vanished composition.
            {
                let mut names = txn.open_table(NAMES)?;
                let mut names_rev = txn.open_table(NAMES_REVERSE)?;
                let composite = names_rev
                    .get(id.0.as_bytes().as_slice())?
                    .map(|guard| guard.value().to_vec());
                if let Some(composite) = composite {
                    names.remove(composite.as_slice())?;
                    names_rev.remove(id.0.as_bytes().as_slice())?;
                }
            }
            txn.commit()?;
            existed
        };
        self.cache.lock().lock_or_die("redb.cache").pop(&id);
        Ok(existed)
    }

    fn name_lookup(
        &self,
        ns: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, PersistentStoreError> {
        // I-CP9: overlay first, redb second.
        match self.overlay_name_lookup(ns, name) {
            OverlayLookup::Hit(id) => return Ok(Some(id)),
            OverlayLookup::Tombstone => return Ok(None),
            OverlayLookup::Miss => {}
        }
        let key = name_key(ns, name);
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_read()?;
        let table = txn.open_table(NAMES)?;
        let Some(guard) = table.get(key.as_slice())? else {
            return Ok(None);
        };
        let bytes = guard.value();
        if bytes.len() != 16 {
            return Err(PersistentStoreError::Decode(format!(
                "name index value has wrong length: {}",
                bytes.len(),
            )));
        }
        let mut buf = [0u8; 16];
        buf.copy_from_slice(bytes);
        Ok(Some(CompositionId(uuid::Uuid::from_bytes(buf))))
    }

    fn name_for(
        &self,
        id: CompositionId,
    ) -> Result<Option<(NamespaceId, String)>, PersistentStoreError> {
        // I-CP9: overlay first, redb second.
        match self.overlay_name_for(id) {
            OverlayLookup::Hit(k) => return Ok(Some(k)),
            OverlayLookup::Tombstone => return Ok(None),
            OverlayLookup::Miss => {}
        }
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_read()?;
        let table = txn.open_table(NAMES_REVERSE)?;
        let Some(guard) = table.get(id.0.as_bytes().as_slice())? else {
            return Ok(None);
        };
        decode_name_key(guard.value())
            .map(Some)
            .map_err(|e| PersistentStoreError::Decode(format!("name reverse decode: {e}")))
    }

    fn name_insert(
        &mut self,
        ns: NamespaceId,
        name: String,
        id: CompositionId,
    ) -> Result<(), PersistentStoreError> {
        // rev-3 write-behind path.
        if let Some(handle) = &self.write_behind {
            let mut ov = handle.overlay.write();
            if ov.len() < handle.max_queue_size {
                let key = (ns, name.clone());
                // If this id had a prior name binding (visible from
                // either the overlay or — implicitly — redb), we'd
                // ideally tombstone the prior forward entry too. The
                // overlay only tracks pending writes; we look up the
                // prior reverse for it, and the drainer's
                // `commit_snapshot_to_redb` re-asserts the same
                // "drop prior forward entry for this id" rule when
                // it lands the redb update. Net effect matches the
                // inline path.
                let prior_rev = ov
                    .name_rev
                    .get(&id)
                    .and_then(|(_, payload)| payload.clone());
                if let Some(prior) = prior_rev {
                    if prior != key {
                        let pseq = ov.next_seq();
                        ov.name_fwd.insert(prior, (pseq, None));
                    }
                }
                let fseq = ov.next_seq();
                ov.name_fwd.insert(key.clone(), (fseq, Some(id)));
                let rseq = ov.next_seq();
                ov.name_rev.insert(id, (rseq, Some(key)));
                drop(ov);
                handle.notify.notify_one();
                return Ok(());
            }
            // Saturated → fall through.
        }
        let new_key = name_key(ns, &name);
        let db = self.db.lock().lock_or_die("redb.db");
        let mut txn = db.begin_write()?;
        self.apply_durability(&mut txn);
        {
            let mut names = txn.open_table(NAMES)?;
            let mut names_rev = txn.open_table(NAMES_REVERSE)?;
            // If the new (ns, name) already maps to a different id,
            // drop that id's reverse entry (it's about to be orphaned).
            // Materialize the bytes before any mutation so the
            // immutable read borrow is gone before the mutable remove.
            let prev_id_bytes = names
                .get(new_key.as_slice())?
                .map(|guard| guard.value().to_vec());
            if let Some(prev) = prev_id_bytes {
                if prev.len() == 16 && prev != id.0.as_bytes().as_slice() {
                    names_rev.remove(prev.as_slice())?;
                }
            }
            // If `id` already had a name in some namespace, drop that
            // forward entry so the reverse map is single-valued.
            let prev_composite = names_rev
                .get(id.0.as_bytes().as_slice())?
                .map(|guard| guard.value().to_vec());
            if let Some(prev) = prev_composite {
                if prev.as_slice() != new_key.as_slice() {
                    names.remove(prev.as_slice())?;
                }
            }
            names.insert(new_key.as_slice(), id.0.as_bytes().as_slice())?;
            names_rev.insert(id.0.as_bytes().as_slice(), new_key.as_slice())?;
        }
        txn.commit().inspect_err(|_| self.record_commit_error())?;
        Ok(())
    }

    fn name_remove(&mut self, ns: NamespaceId, name: &str) -> Result<bool, PersistentStoreError> {
        // rev-3 write-behind path.
        if let Some(handle) = &self.write_behind {
            let key_tup = (ns, name.to_owned());
            let prior_overlay = match self.overlay_name_lookup(ns, name) {
                OverlayLookup::Hit(id) => Some(id),
                _ => None,
            };
            let mut ov = handle.overlay.write();
            if ov.len() < handle.max_queue_size {
                let fseq = ov.next_seq();
                ov.name_fwd.insert(key_tup.clone(), (fseq, None));
                if let Some(id) = prior_overlay {
                    let rseq = ov.next_seq();
                    ov.name_rev.insert(id, (rseq, None));
                }
                drop(ov);
                handle.notify.notify_one();
                if prior_overlay.is_some() {
                    return Ok(true);
                }
                // Peek redb to give an accurate "did it exist?" answer.
                let exists = {
                    let db = self.db.lock().lock_or_die("redb.db");
                    let txn = db.begin_read()?;
                    let table = txn.open_table(NAMES)?;
                    table.get(name_key(ns, name).as_slice())?.is_some()
                };
                return Ok(exists);
            }
            // Saturated → fall through.
        }
        let key = name_key(ns, name);
        let db = self.db.lock().lock_or_die("redb.db");
        let mut txn = db.begin_write()?;
        self.apply_durability(&mut txn);
        let removed = {
            let mut names = txn.open_table(NAMES)?;
            let mut names_rev = txn.open_table(NAMES_REVERSE)?;
            let removed_id_bytes = names
                .remove(key.as_slice())?
                .map(|guard| guard.value().to_vec());
            if let Some(ref id_bytes) = removed_id_bytes {
                names_rev.remove(id_bytes.as_slice())?;
            }
            removed_id_bytes.is_some()
        };
        txn.commit().inspect_err(|_| self.record_commit_error())?;
        Ok(removed)
    }

    fn name_list(
        &self,
        ns: NamespaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, CompositionId)>, PersistentStoreError> {
        // I-CP9 LIST consistency for the name index:
        //   - Overlay tombstone (None) hides the redb entry for that name.
        //   - Overlay Some(id) overrides redb for that name.
        //   - Overlay Some(id) for a name absent from redb is added.
        let overlay_keep: std::collections::HashMap<String, Option<CompositionId>> =
            if let Some(handle) = &self.write_behind {
                let ov = handle.overlay.read();
                ov.name_fwd
                    .iter()
                    .filter(|((entry_ns, name), _)| {
                        *entry_ns == ns
                            && prefix.is_none_or(|p| name.starts_with(p))
                    })
                    .map(|((_, name), (_, payload))| (name.clone(), *payload))
                    .collect()
            } else {
                std::collections::HashMap::new()
            };
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_read()?;
        let table = txn.open_table(NAMES)?;
        // Range scan over the namespace prefix. Keys are
        // 16-byte ns_id || name; for a fixed namespace the iteration
        // is naturally lexicographic on name (S3 LIST ordering).
        let ns_prefix = ns.0.as_bytes();
        let mut start = ns_prefix.to_vec();
        if let Some(p) = prefix {
            start.extend_from_slice(p.as_bytes());
        }
        // Upper bound: bump the last byte; if all bytes are 0xff,
        // fall back to "ns_prefix + 0xff..." to cover the full
        // namespace. For simplicity we scan the whole namespace and
        // filter by prefix in-process — keys are short.
        let mut out: Vec<(String, CompositionId)> = Vec::new();
        let scan_upper = {
            let mut v = ns_prefix.to_vec();
            v.push(0xff);
            v
        };
        for entry in table.range(ns_prefix.as_slice()..scan_upper.as_slice())? {
            let (k, v) = entry?;
            let key_bytes = k.value();
            if key_bytes.len() < 16 || &key_bytes[..16] != ns_prefix {
                continue;
            }
            let name = std::str::from_utf8(&key_bytes[16..])
                .map_err(|e| PersistentStoreError::Decode(format!("name index utf8: {e}")))?;
            if let Some(p) = prefix {
                if !name.starts_with(p) {
                    continue;
                }
            }
            // Overlay precedence: if the overlay says this name was
            // tombstoned or rebound, skip the redb entry (it's about
            // to be replaced by the overlay merge below).
            if overlay_keep.contains_key(name) {
                continue;
            }
            let id_bytes = v.value();
            if id_bytes.len() != 16 {
                continue;
            }
            let mut buf = [0u8; 16];
            buf.copy_from_slice(id_bytes);
            out.push((name.to_owned(), CompositionId(uuid::Uuid::from_bytes(buf))));
        }
        // Merge overlay's `Some(id)` entries; tombstones (`None`)
        // were already excluded from the redb scan above.
        for (name, payload) in overlay_keep {
            if let Some(id) = payload {
                out.push((name, id));
            }
        }
        Ok(out)
    }

    fn last_applied_seq(&self) -> Result<SequenceNumber, PersistentStoreError> {
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_read()?;
        let table = txn.open_table(META)?;
        let Some(guard) = table.get(meta_keys::LAST_APPLIED_SEQ)? else {
            return Ok(SequenceNumber(0));
        };
        let bytes = guard.value();
        if bytes.len() != 8 {
            return Err(PersistentStoreError::Decode(format!(
                "last_applied_seq has wrong length: {}",
                bytes.len()
            )));
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(bytes);
        Ok(SequenceNumber(u64::from_le_bytes(buf)))
    }

    fn stuck_state(&self) -> Result<Option<(SequenceNumber, u32)>, PersistentStoreError> {
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_read()?;
        let table = txn.open_table(META)?;
        let Some(guard) = table.get(meta_keys::STUCK_STATE)? else {
            return Ok(None);
        };
        decode_stuck_state(guard.value())
    }

    fn halted(&self) -> Result<bool, PersistentStoreError> {
        let db = self.db.lock().lock_or_die("redb.db");
        let txn = db.begin_read()?;
        let table = txn.open_table(META)?;
        let Some(guard) = table.get(meta_keys::HALTED)? else {
            return Ok(false);
        };
        Ok(guard.value().first().copied().unwrap_or(0) != 0)
    }

    fn apply_hydration_batch(&mut self, batch: HydrationBatch) -> Result<(), PersistentStoreError> {
        // Atomic batch (I-CP1). All data + meta updates land in a
        // single redb transaction; a crash during commit rolls
        // everything back.
        let mut commit_invalidations: Vec<CompositionId> = Vec::new();
        let mut commit_inserts: Vec<Composition> = Vec::new();

        {
            let db = self.db.lock().lock_or_die("redb.db");
            let mut txn = db.begin_write()?;
            self.apply_durability(&mut txn);
            {
                let mut comps = txn.open_table(COMPOSITIONS)?;
                let mut names = txn.open_table(NAMES)?;
                let mut names_rev = txn.open_table(NAMES_REVERSE)?;
                for comp in &batch.puts {
                    let bytes = encode_composition(comp).inspect_err(|e| {
                        self.record_decode_error(e);
                    })?;
                    comps.insert(comp.id.0.as_bytes().as_slice(), bytes.as_slice())?;
                    commit_inserts.push(comp.clone());
                }
                for id in &batch.removes {
                    comps.remove(id.0.as_bytes().as_slice())?;
                    // Drop any name binding for the removed composition
                    // atomically — same rationale as the standalone
                    // `remove`. Belt-and-braces: the hydrator should
                    // also include the binding in `name_removes` if it
                    // wants the unbind to propagate by name.
                    let composite = names_rev
                        .get(id.0.as_bytes().as_slice())?
                        .map(|guard| guard.value().to_vec());
                    if let Some(composite) = composite {
                        names.remove(composite.as_slice())?;
                        names_rev.remove(id.0.as_bytes().as_slice())?;
                    }
                    commit_invalidations.push(*id);
                }
                for (ns, name, id) in &batch.name_inserts {
                    let new_key = name_key(*ns, name);
                    let prev_id_bytes = names
                        .get(new_key.as_slice())?
                        .map(|guard| guard.value().to_vec());
                    if let Some(prev) = prev_id_bytes {
                        if prev.len() == 16 && prev != id.0.as_bytes().as_slice() {
                            names_rev.remove(prev.as_slice())?;
                        }
                    }
                    let prev_composite = names_rev
                        .get(id.0.as_bytes().as_slice())?
                        .map(|guard| guard.value().to_vec());
                    if let Some(prev) = prev_composite {
                        if prev.as_slice() != new_key.as_slice() {
                            names.remove(prev.as_slice())?;
                        }
                    }
                    names.insert(new_key.as_slice(), id.0.as_bytes().as_slice())?;
                    names_rev.insert(id.0.as_bytes().as_slice(), new_key.as_slice())?;
                }
                for (ns, name) in &batch.name_removes {
                    let key = name_key(*ns, name);
                    let removed_id_bytes = names
                        .remove(key.as_slice())?
                        .map(|guard| guard.value().to_vec());
                    if let Some(id_bytes) = removed_id_bytes {
                        names_rev.remove(id_bytes.as_slice())?;
                    }
                }
            }
            {
                let mut meta = txn.open_table(META)?;
                meta.insert(
                    meta_keys::LAST_APPLIED_SEQ,
                    batch.new_last_applied_seq.0.to_le_bytes().as_slice(),
                )?;
                if let Some(stuck) = batch.stuck_state {
                    let payload = encode_stuck_state(stuck);
                    meta.insert(meta_keys::STUCK_STATE, payload.as_slice())?;
                }
                if let Some(halted) = batch.halted {
                    meta.insert(meta_keys::HALTED, [u8::from(halted)].as_slice())?;
                }
            }
            txn.commit().inspect_err(|_| self.record_commit_error())?;
        }

        // Cache update *after* commit so any reader that observes
        // the cache also observes the durable state (D3).
        let mut evictions: u64 = 0;
        {
            let mut cache = self.cache.lock().lock_or_die("redb.cache");
            for comp in commit_inserts {
                let id = comp.id;
                let push_result = cache.push(id, comp);
                if Self::is_capacity_eviction(push_result.as_ref(), id) {
                    evictions += 1;
                }
            }
            for id in commit_invalidations {
                cache.pop(&id);
            }
        }
        if evictions > 0 {
            if let Some(ref m) = self.metrics {
                m.lru_evicted_total.inc_by(evictions);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composition::Composition;
    use kiseki_common::ids::{ChunkId, OrgId, ShardId};

    fn make_comp(idx: u8) -> Composition {
        Composition {
            id: CompositionId(uuid::Uuid::from_u128(u128::from(idx))),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::from_u128(2)),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            chunks: vec![ChunkId([idx; 32])],
            version: 1,
            size: u64::from(idx) * 100,
            has_inline_data: false,
            content_type: None,
        }
    }

    #[test]
    fn put_and_get_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("test.redb")).unwrap();
        let comp = make_comp(7);
        store.put(comp.clone()).unwrap();
        let got = store.get(comp.id).unwrap().unwrap();
        assert_eq!(got, comp);
    }

    #[test]
    fn get_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentRedbStorage::open(&dir.path().join("test.redb")).unwrap();
        let id = CompositionId(uuid::Uuid::from_u128(99));
        assert!(store.get(id).unwrap().is_none());
    }

    #[test]
    fn remove_drops_record_and_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("test.redb")).unwrap();
        let comp = make_comp(3);
        store.put(comp.clone()).unwrap();
        assert!(store.remove(comp.id).unwrap());
        assert!(store.get(comp.id).unwrap().is_none());
        // remove again is idempotent
        assert!(!store.remove(comp.id).unwrap());
    }

    #[test]
    fn count_and_list_in_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("test.redb")).unwrap();
        for i in 1..=5u8 {
            store.put(make_comp(i)).unwrap();
        }
        assert_eq!(store.count().unwrap(), 5);
        let ns = NamespaceId(uuid::Uuid::from_u128(2));
        assert_eq!(store.list_in_namespace(ns).unwrap().len(), 5);
        let other_ns = NamespaceId(uuid::Uuid::from_u128(99));
        assert_eq!(store.list_in_namespace(other_ns).unwrap().len(), 0);
    }

    #[test]
    fn meta_defaults_on_first_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentRedbStorage::open(&dir.path().join("test.redb")).unwrap();
        assert_eq!(store.last_applied_seq().unwrap().0, 0);
        assert_eq!(store.stuck_state().unwrap(), None);
        assert!(!store.halted().unwrap());
    }

    #[test]
    fn apply_batch_atomically_commits_data_and_meta() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("test.redb")).unwrap();

        let comp = make_comp(11);
        let batch = HydrationBatch {
            puts: vec![comp.clone()],
            removes: vec![],
            name_inserts: vec![],
            name_removes: vec![],
            new_last_applied_seq: SequenceNumber(42),
            stuck_state: Some(Some((SequenceNumber(40), 7))),
            halted: Some(true),
        };
        store.apply_hydration_batch(batch).unwrap();

        assert_eq!(store.last_applied_seq().unwrap().0, 42);
        assert_eq!(store.stuck_state().unwrap(), Some((SequenceNumber(40), 7)));
        assert!(store.halted().unwrap());
        assert_eq!(store.get(comp.id).unwrap().unwrap(), comp);
    }

    #[test]
    fn apply_batch_clears_stuck_when_set_to_some_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("test.redb")).unwrap();
        // Seed a stuck state.
        store
            .apply_hydration_batch(HydrationBatch {
                puts: vec![],
                removes: vec![],
                name_inserts: vec![],
                name_removes: vec![],
                new_last_applied_seq: SequenceNumber(10),
                stuck_state: Some(Some((SequenceNumber(9), 1))),
                halted: None,
            })
            .unwrap();
        // Clear it.
        store
            .apply_hydration_batch(HydrationBatch::advance(SequenceNumber(20)))
            .unwrap();
        assert_eq!(store.stuck_state().unwrap(), None);
        assert_eq!(store.last_applied_seq().unwrap().0, 20);
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist.redb");
        let comp = make_comp(5);

        {
            let mut s = PersistentRedbStorage::open(&path).unwrap();
            s.put(comp.clone()).unwrap();
            s.apply_hydration_batch(HydrationBatch {
                puts: vec![],
                removes: vec![],
                name_inserts: vec![],
                name_removes: vec![],
                new_last_applied_seq: SequenceNumber(100),
                stuck_state: Some(Some((SequenceNumber(99), 5))),
                halted: Some(true),
            })
            .unwrap();
        }

        let s = PersistentRedbStorage::open(&path).unwrap();
        assert_eq!(s.get(comp.id).unwrap().unwrap(), comp);
        assert_eq!(s.last_applied_seq().unwrap().0, 100);
        assert_eq!(s.stuck_state().unwrap(), Some((SequenceNumber(99), 5)));
        assert!(s.halted().unwrap());
    }

    #[test]
    fn schema_too_new_refuses_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.redb");
        // Open + write a fake schema_version > supported.
        {
            let db = ::redb::Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let _ = txn.open_table(COMPOSITIONS).unwrap();
                let mut meta = txn.open_table(META).unwrap();
                meta.insert(
                    meta_keys::SCHEMA_VERSION,
                    [COMPOSITION_RECORD_SCHEMA_VERSION + 1].as_slice(),
                )
                .unwrap();
            }
            txn.commit().unwrap();
        }
        // Opening with the production code path must refuse. Match
        // on the result directly; PersistentRedbStorage isn't Debug
        // (its inner redb::Database isn't), so .unwrap_err() doesn't
        // typecheck.
        match PersistentRedbStorage::open(&path) {
            Ok(_) => panic!("expected SchemaTooNew, got Ok"),
            Err(PersistentStoreError::SchemaTooNew { found, supported }) => {
                assert_eq!(found, COMPOSITION_RECORD_SCHEMA_VERSION + 1);
                assert_eq!(supported, COMPOSITION_RECORD_SCHEMA_VERSION);
            }
            Err(other) => panic!("expected SchemaTooNew, got {other:?}"),
        }
    }

    /// Auditor finding A7 — I-CP4 for the `put()` write path.
    /// Mirror of `cache_serves_post_commit_value_after_apply_batch`
    /// but for the direct-write entry point. Verifies `put()` updates
    /// the cache AFTER the redb commit, so a reader following a
    /// `put()` always sees the new value (never the pre-commit one).
    #[test]
    fn cache_serves_post_commit_value_after_put() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("test.redb")).unwrap();
        let mut comp = make_comp(7);
        store.put(comp.clone()).unwrap();

        // Read populates the LRU.
        assert_eq!(store.get(comp.id).unwrap().unwrap(), comp);

        // Direct put with a bumped version (gateway path uses this
        // in `set_content_type`, `update`, `create_at`).
        comp.version = 99;
        store.put(comp.clone()).unwrap();

        // Cache must now serve the bumped version, not the stale one.
        let got = store.get(comp.id).unwrap().unwrap();
        assert_eq!(got.version, 99);
    }

    #[test]
    fn cache_serves_post_commit_value_after_apply_batch() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("test.redb")).unwrap();
        let mut comp = make_comp(7);
        store.put(comp.clone()).unwrap();

        // Read populates the LRU.
        assert_eq!(store.get(comp.id).unwrap().unwrap(), comp);

        // Apply a batch that bumps the version.
        comp.version = 99;
        store
            .apply_hydration_batch(HydrationBatch {
                puts: vec![comp.clone()],
                removes: vec![],
                name_inserts: vec![],
                name_removes: vec![],
                new_last_applied_seq: SequenceNumber(1),
                stuck_state: Some(None),
                halted: None,
            })
            .unwrap();

        // Cache must now serve the bumped version, not the stale one.
        let got = store.get(comp.id).unwrap().unwrap();
        assert_eq!(got.version, 99);
    }
}
