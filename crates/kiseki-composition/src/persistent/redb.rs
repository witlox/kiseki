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

// -- Schema -----------------------------------------------------------------

/// Current on-disk schema version. Bumped on incompatible changes per
/// ADR-040 §D8.
pub const COMPOSITION_RECORD_SCHEMA_VERSION: u8 = 1;

/// Compositions: `comp_id.0.as_bytes()` → `[version][postcard]`.
const COMPOSITIONS: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("compositions");

/// Meta: see `meta_keys` for the namespace.
const META: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("meta");

mod meta_keys {
    pub const SCHEMA_VERSION: &str = "schema_version";
    pub const LAST_APPLIED_SEQ: &str = "last_applied_seq";
    pub const STUCK_STATE: &str = "stuck_state";
    pub const HALTED: &str = "halted";
}

const DEFAULT_LRU_CAPACITY: usize = 100_000;

// -- Encoding helpers -------------------------------------------------------

/// `[1 byte: version][postcard payload]` — see ADR-040 §D2.
fn encode_composition(comp: &Composition) -> Result<Vec<u8>, PersistentStoreError> {
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

// -- Storage struct ---------------------------------------------------------

/// redb-backed `CompositionStorage`.
pub struct PersistentRedbStorage {
    db: Mutex<Database>,
    cache: Mutex<LruCache<CompositionId, Composition>>,
    metrics: Option<std::sync::Arc<crate::metrics::CompositionMetrics>>,
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
                .unwrap_or(std::num::NonZeroUsize::new(1).unwrap()),
        );
        Ok(Self {
            db: Mutex::new(db),
            cache: Mutex::new(cache),
            metrics: None,
        })
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
        // Cache lookup first — sync mutex, brief.
        if let Some(comp) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id, comp.clone());
        if evicted.is_some() {
            if let Some(ref m) = self.metrics {
                m.lru_evicted_total.inc();
            }
        }
        Ok(Some(comp))
    }

    fn count(&self) -> Result<u64, PersistentStoreError> {
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let txn = db.begin_read()?;
        let table = txn.open_table(COMPOSITIONS)?;
        Ok(table.len()?)
    }

    fn list_in_namespace(&self, ns: NamespaceId) -> Result<Vec<Composition>, PersistentStoreError> {
        // v1: full table scan. ADR-040 calls out that a future
        // revision adds a (namespace_id → comp_id) secondary index.
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let txn = db.begin_read()?;
        let table = txn.open_table(COMPOSITIONS)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            let comp = decode_composition(value.value())?;
            if comp.namespace_id == ns {
                out.push(comp);
            }
        }
        Ok(out)
    }

    fn put(&mut self, comp: Composition) -> Result<(), PersistentStoreError> {
        let id = comp.id;
        let bytes = encode_composition(&comp).inspect_err(|e| {
            self.record_decode_error(e);
        })?;
        {
            let db = self
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(COMPOSITIONS)?;
                table.insert(id.0.as_bytes().as_slice(), bytes.as_slice())?;
            }
            txn.commit().inspect_err(|_| self.record_commit_error())?;
        }
        // Cache update happens *after* commit so a reader that sees
        // the cache value also sees the durable record (D3).
        let push_result = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id, comp);
        self.record_eviction(Self::is_capacity_eviction(push_result.as_ref(), id));
        Ok(())
    }

    fn remove(&mut self, id: CompositionId) -> Result<bool, PersistentStoreError> {
        let existed = {
            let db = self
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let txn = db.begin_write()?;
            let existed = {
                let mut table = txn.open_table(COMPOSITIONS)?;
                let removed = table.remove(id.0.as_bytes().as_slice())?;
                removed.is_some()
            };
            txn.commit()?;
            existed
        };
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop(&id);
        Ok(existed)
    }

    fn last_applied_seq(&self) -> Result<SequenceNumber, PersistentStoreError> {
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let txn = db.begin_read()?;
        let table = txn.open_table(META)?;
        let Some(guard) = table.get(meta_keys::STUCK_STATE)? else {
            return Ok(None);
        };
        decode_stuck_state(guard.value())
    }

    fn halted(&self) -> Result<bool, PersistentStoreError> {
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let db = self
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let txn = db.begin_write()?;
            {
                let mut comps = txn.open_table(COMPOSITIONS)?;
                for comp in &batch.puts {
                    let bytes = encode_composition(comp).inspect_err(|e| {
                        self.record_decode_error(e);
                    })?;
                    comps.insert(comp.id.0.as_bytes().as_slice(), bytes.as_slice())?;
                    commit_inserts.push(comp.clone());
                }
                for id in &batch.removes {
                    comps.remove(id.0.as_bytes().as_slice())?;
                    commit_invalidations.push(*id);
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
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
