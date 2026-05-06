//! Fjall-backed `CompositionStorage` (ADR-022 successor for the
//! write-heavy persistent path).
//!
//! Replaces redb on the composition hot path: every PUT lands a
//! composition row + (optionally) a name binding. redb's B-tree
//! commit cost capped at ~18 k op/s on the perf harness; fjall is
//! an LSM (memtable + WAL on the write side, background compaction
//! off the hot path) and is the migration target ADR-022 already
//! names in its "Consequences".
//!
//! ## Schema
//!
//! Four keyspaces (one physical LSM-tree each) inside one fjall
//! database:
//!
//! - `comps`     — `comp_id.0.as_bytes()` → `[version][postcard(Composition)]`
//! - `names`     — 16-byte `ns_id` || name → `comp_id.0.as_bytes()`
//! - `names_rev` — `comp_id.0.as_bytes()` → 16-byte `ns_id` || name
//! - `meta`      — fixed string keys (last_applied_seq, stuck_state,
//!                 halted, schema_version)
//!
//! Encoding helpers (`encode_composition`, `decode_composition`,
//! `name_key`, `decode_name_key`, `encode_stuck_state`,
//! `decode_stuck_state`) live in `super::encoding` — single source
//! of truth for the on-disk record format so a future backend swap
//! does not re-serialize rows.
//!
//! ## Durability model
//!
//! fjall writes go to the keyspace's memtable + the database's WAL.
//! `PersistMode::Buffer` returns as soon as the WAL bytes are queued
//! (no fsync); the periodic flusher in the runtime drives
//! `PersistMode::SyncAll` at the configured interval (matches the
//! existing `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS` knob — same
//! crash-safety contract as the redb write-behind queue, ADR-040
//! rev-3 wording).
//!
//! Since the LSM amortizes writes through the memtable, this
//! implementation does NOT need a separate write-behind queue
//! layer — the queue's batching role is what fjall does natively.

#![allow(clippy::missing_errors_doc)]

use std::path::Path;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};
use kiseki_common::ids::{CompositionId, NamespaceId, SequenceNumber};

use super::error::PersistentStoreError;
use super::storage::{CompositionStorage, HydrationBatch};
use crate::composition::Composition;

use super::encoding::{
    decode_composition, decode_stuck_state, encode_composition, encode_stuck_state,
    name_key, COMPOSITION_RECORD_SCHEMA_VERSION,
};

// Each fjall keyspace is its own physical LSM-tree (column-family
// equivalent). We split into four so writes to disjoint axes don't
// share a memtable / flush trigger.
const KS_COMPS: &str = "comps";
const KS_NAMES: &str = "names";
const KS_NAMES_REV: &str = "names_rev";
const KS_META: &str = "meta";

mod meta_keys {
    pub const SCHEMA_VERSION: &[u8] = b"schema_version";
    pub const LAST_APPLIED_SEQ: &[u8] = b"last_applied_seq";
    pub const STUCK_STATE: &[u8] = b"stuck_state";
    pub const HALTED: &[u8] = b"halted";
}

/// Fjall-backed implementation of [`CompositionStorage`].
pub struct FjallStorage {
    db: Database,
    comps: Keyspace,
    names: Keyspace,
    names_rev: Keyspace,
    meta: Keyspace,
    /// When set, every write commits with `PersistMode::SyncAll`
    /// inline — POSIX-fsync semantics, slow. When unset, writes
    /// commit with `PersistMode::Buffer` (queued, no fsync); the
    /// runtime's periodic flusher calls `flush()` to issue an
    /// explicit fsync at a bounded cadence. Same shape as the
    /// previous redb `eventual_durability` knob.
    sync_per_write: bool,
}

impl FjallStorage {
    /// Open or create a fjall database at `path`.
    pub fn open(path: &Path) -> Result<Self, PersistentStoreError> {
        let db = Database::builder(path).open().map_err(map_fjall_err)?;
        let comps = db
            .keyspace(KS_COMPS, KeyspaceCreateOptions::default)
            .map_err(map_fjall_err)?;
        let names = db
            .keyspace(KS_NAMES, KeyspaceCreateOptions::default)
            .map_err(map_fjall_err)?;
        let names_rev = db
            .keyspace(KS_NAMES_REV, KeyspaceCreateOptions::default)
            .map_err(map_fjall_err)?;
        let meta = db
            .keyspace(KS_META, KeyspaceCreateOptions::default)
            .map_err(map_fjall_err)?;

        // First-open: stamp the schema version. Subsequent opens
        // verify it matches what this binary supports — a future
        // version > supported is fail-closed (operators wipe &
        // re-hydrate from Raft).
        let stamped = meta
            .get(meta_keys::SCHEMA_VERSION)
            .map_err(map_fjall_err)?;
        match stamped {
            Some(slice) => {
                let &v = slice.first().ok_or_else(|| {
                    PersistentStoreError::Decode("schema_version row empty".into())
                })?;
                if v > COMPOSITION_RECORD_SCHEMA_VERSION {
                    return Err(PersistentStoreError::SchemaTooNew {
                        found: v,
                        supported: COMPOSITION_RECORD_SCHEMA_VERSION,
                    });
                }
            }
            None => {
                meta.insert(
                    meta_keys::SCHEMA_VERSION,
                    [COMPOSITION_RECORD_SCHEMA_VERSION].as_slice(),
                )
                .map_err(map_fjall_err)?;
                db.persist(PersistMode::SyncAll).map_err(map_fjall_err)?;
            }
        }

        Ok(Self {
            db,
            comps,
            names,
            names_rev,
            meta,
            sync_per_write: true,
        })
    }

    /// Configure inline-fsync vs buffered-write durability.
    ///
    /// `true` (default) → every write fsyncs (`PersistMode::SyncAll`).
    /// `false` → writes return after WAL append (`PersistMode::Buffer`);
    /// the runtime is expected to drive a periodic [`Self::flush`]
    /// (default 100 ms) to fsync the WAL.
    #[must_use]
    pub fn with_eventual_durability(mut self, eventual: bool) -> Self {
        self.sync_per_write = !eventual;
        self
    }

    /// Force an fsync of the WAL to disk. Used by the runtime's
    /// periodic flusher and by the gateway's `fsync_pending` hook
    /// (FUSE / NFS `fsync(2)`).
    pub fn flush(&self) -> Result<(), PersistentStoreError> {
        self.db
            .persist(PersistMode::SyncAll)
            .map_err(map_fjall_err)?;
        Ok(())
    }

    /// Cheap clonable handle to the underlying `Keyspace` for off-
    /// thread fsync. The handle does **not** participate in the
    /// `CompositionStorage` mutex chain, so the runtime's periodic
    /// flusher and the gateway's `fsync_pending` hook can drive a
    /// real fsync without contending with concurrent writers. Same
    /// shape as the previous `RedbFlusher`.
    #[must_use]
    pub fn flusher(&self) -> FjallFlusher {
        FjallFlusher {
            db: self.db.clone(),
        }
    }

    fn persist_after_write(&self) -> Result<(), PersistentStoreError> {
        if self.sync_per_write {
            // Per-write fsync (POSIX semantics on the eventual-
            // durability=false path).
            self.db
                .persist(PersistMode::SyncAll)
                .map_err(map_fjall_err)?;
        }
        // Eventual-durability path: nothing to do here. The WAL
        // bytes are already in the journal from the preceding
        // `WriteBatch::commit`; the runtime's periodic flusher
        // (`KISEKI_COMPOSITION_FLUSH_INTERVAL_MS`) drives the
        // actual fsync, and the gateway's `fsync_pending` hook
        // can force one on demand. A `persist(Buffer)` call here
        // would only re-acquire the journal mutex to enqueue an
        // async fsync request — pure contention with no extra
        // durability.
        Ok(())
    }
}

fn map_fjall_err(e: fjall::Error) -> PersistentStoreError {
    PersistentStoreError::Decode(format!("fjall: {e}"))
}

/// Off-thread fsync handle. Calling [`Self::flush`] issues a
/// `PersistMode::SyncAll` against the shared `Database` — the same
/// fsync the inline write path would have done.
#[derive(Clone)]
pub struct FjallFlusher {
    db: Database,
}

impl FjallFlusher {
    /// Drive an fsync of the WAL. Returns `Ok(())` once the WAL bytes
    /// are durable on disk.
    pub fn flush(&self) -> Result<(), PersistentStoreError> {
        self.db
            .persist(PersistMode::SyncAll)
            .map_err(map_fjall_err)?;
        Ok(())
    }
}

impl CompositionStorage for FjallStorage {
    fn get(&self, id: CompositionId) -> Result<Option<Composition>, PersistentStoreError> {
        match self
            .comps
            .get(id.0.as_bytes())
            .map_err(map_fjall_err)?
        {
            None => Ok(None),
            Some(slice) => Ok(Some(decode_composition(slice.as_ref())?)),
        }
    }

    fn count(&self) -> Result<u64, PersistentStoreError> {
        // fjall's `len()` walks the partition; matches redb's
        // `Table::len()` cost. Acceptable for the operator-only
        // /metrics gauge that calls this.
        Ok(self.comps.len().map_err(map_fjall_err)? as u64)
    }

    fn list_in_namespace(
        &self,
        ns: NamespaceId,
    ) -> Result<Vec<Composition>, PersistentStoreError> {
        // No secondary index by namespace yet (same as redb impl) —
        // full scan + filter. ADR-040 calls this out as a future
        // optimization; volume is bounded by per-tenant namespace
        // size which is operator-shaped.
        let mut out = Vec::new();
        for entry in self.comps.iter() {
            let (_k, v) = entry.into_inner().map_err(map_fjall_err)?;
            let comp = decode_composition(v.as_ref())?;
            if comp.namespace_id == ns {
                out.push(comp);
            }
        }
        Ok(out)
    }

    fn put(&mut self, comp: Composition) -> Result<(), PersistentStoreError> {
        let bytes = encode_composition(&comp)?;
        self.comps
            .insert(comp.id.0.as_bytes(), bytes.as_slice())
            .map_err(map_fjall_err)?;
        self.persist_after_write()
    }

    fn remove(&mut self, id: CompositionId) -> Result<bool, PersistentStoreError> {
        // Atomic batch: drop the comp row + cascade-drop the name
        // binding so a future PUT-by-name doesn't resolve to a
        // dangling composition_id. Mirrors the redb path.
        let existed = self
            .comps
            .get(id.0.as_bytes())
            .map_err(map_fjall_err)?
            .is_some();
        let prior_name = self
            .names_rev
            .get(id.0.as_bytes())
            .map_err(map_fjall_err)?
            .map(|s| s.to_vec());

        let mut batch = self.db.batch();
        batch.remove(&self.comps, id.0.as_bytes().to_vec());
        if let Some(ref name_key_bytes) = prior_name {
            batch.remove(&self.names, name_key_bytes.clone());
            batch.remove(&self.names_rev, id.0.as_bytes().to_vec());
        }
        batch.commit().map_err(map_fjall_err)?;
        self.persist_after_write()?;
        Ok(existed)
    }

    fn name_lookup(
        &self,
        ns: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, PersistentStoreError> {
        let key = name_key(ns, name);
        match self.names.get(&key).map_err(map_fjall_err)? {
            None => Ok(None),
            Some(slice) => {
                let bytes = slice.as_ref();
                if bytes.len() != 16 {
                    return Err(PersistentStoreError::Decode(format!(
                        "name forward value has wrong length: {}",
                        bytes.len()
                    )));
                }
                let mut buf = [0u8; 16];
                buf.copy_from_slice(bytes);
                Ok(Some(CompositionId(uuid::Uuid::from_bytes(buf))))
            }
        }
    }

    fn name_for(
        &self,
        id: CompositionId,
    ) -> Result<Option<(NamespaceId, String)>, PersistentStoreError> {
        match self
            .names_rev
            .get(id.0.as_bytes())
            .map_err(map_fjall_err)?
        {
            None => Ok(None),
            Some(slice) => {
                let bytes = slice.as_ref();
                if bytes.len() < 16 {
                    return Err(PersistentStoreError::Decode(format!(
                        "name reverse value too short: {}",
                        bytes.len()
                    )));
                }
                let mut ns_buf = [0u8; 16];
                ns_buf.copy_from_slice(&bytes[..16]);
                let name = std::str::from_utf8(&bytes[16..])
                    .map_err(|e| {
                        PersistentStoreError::Decode(format!("name reverse utf8: {e}"))
                    })?
                    .to_owned();
                Ok(Some((NamespaceId(uuid::Uuid::from_bytes(ns_buf)), name)))
            }
        }
    }

    fn name_insert(
        &mut self,
        ns: NamespaceId,
        name: String,
        id: CompositionId,
    ) -> Result<(), PersistentStoreError> {
        let new_key = name_key(ns, &name);
        let id_bytes = id.0.as_bytes();

        // Overwrite-replace semantics matching redb: if the new
        // (ns,name) was bound to a different id, drop that id's
        // reverse entry; if id had a different prior name, drop
        // the old forward entry. Pre-flight conditional checks are
        // the caller's job.
        let prev_id = self
            .names
            .get(&new_key)
            .map_err(map_fjall_err)?
            .map(|s| s.to_vec());
        let old_rev_for_id = self
            .names_rev
            .get(id_bytes)
            .map_err(map_fjall_err)?
            .map(|s| s.to_vec());

        let mut batch = self.db.batch();
        if let Some(prev) = prev_id {
            if prev.as_slice() != id_bytes.as_slice() {
                batch.remove(&self.names_rev, prev);
            }
        }
        if let Some(old_rev) = old_rev_for_id {
            if old_rev != new_key {
                batch.remove(&self.names, old_rev);
            }
        }
        batch.insert(&self.names, new_key.clone(), id_bytes.to_vec());
        batch.insert(&self.names_rev, id_bytes.to_vec(), new_key);
        batch.commit().map_err(map_fjall_err)?;
        self.persist_after_write()
    }

    fn name_remove(
        &mut self,
        ns: NamespaceId,
        name: &str,
    ) -> Result<bool, PersistentStoreError> {
        let key = name_key(ns, name);
        let existed_id = self.names.get(&key).map_err(map_fjall_err)?.map(|s| s.to_vec());
        let mut batch = self.db.batch();
        let removed = if let Some(id_bytes) = existed_id {
            batch.remove(&self.names, key);
            batch.remove(&self.names_rev, id_bytes);
            true
        } else {
            false
        };
        batch.commit().map_err(map_fjall_err)?;
        self.persist_after_write()?;
        Ok(removed)
    }

    fn name_list(
        &self,
        ns: NamespaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, CompositionId)>, PersistentStoreError> {
        // Keys are 16-byte ns_id || name. Prefix-scan the namespace
        // segment (free range scan via the LSM ordering); filter
        // by `prefix` in-process — keys are short.
        let ns_prefix = ns.0.as_bytes();
        let mut out: Vec<(String, CompositionId)> = Vec::new();
        for entry in self.names.prefix(ns_prefix) {
            let (k, v) = entry.into_inner().map_err(map_fjall_err)?;
            let kbytes = k.as_ref();
            if kbytes.len() < 16 {
                continue;
            }
            let name = std::str::from_utf8(&kbytes[16..]).map_err(|e| {
                PersistentStoreError::Decode(format!("name forward utf8: {e}"))
            })?;
            if let Some(p) = prefix {
                if !name.starts_with(p) {
                    continue;
                }
            }
            let vbytes = v.as_ref();
            if vbytes.len() != 16 {
                return Err(PersistentStoreError::Decode(format!(
                    "name forward value has wrong length: {}",
                    vbytes.len()
                )));
            }
            let mut id_buf = [0u8; 16];
            id_buf.copy_from_slice(vbytes);
            out.push((name.to_owned(), CompositionId(uuid::Uuid::from_bytes(id_buf))));
        }
        // Stable order — S3 LIST ordering is alphabetical. The LSM
        // iterator yields key-sorted output, but the merge-sort
        // across memtables can interleave entries with the same
        // logical position; sort defensively.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn last_applied_seq(&self) -> Result<SequenceNumber, PersistentStoreError> {
        match self
            .meta
            .get(meta_keys::LAST_APPLIED_SEQ)
            .map_err(map_fjall_err)?
        {
            None => Ok(SequenceNumber(0)),
            Some(slice) => {
                let bytes = slice.as_ref();
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
        }
    }

    fn stuck_state(&self) -> Result<Option<(SequenceNumber, u32)>, PersistentStoreError> {
        match self
            .meta
            .get(meta_keys::STUCK_STATE)
            .map_err(map_fjall_err)?
        {
            None => Ok(None),
            Some(slice) => decode_stuck_state(slice.as_ref()),
        }
    }

    fn halted(&self) -> Result<bool, PersistentStoreError> {
        match self.meta.get(meta_keys::HALTED).map_err(map_fjall_err)? {
            None => Ok(false),
            Some(slice) => Ok(slice.first().is_some_and(|&b| b != 0)),
        }
    }

    fn apply_hydration_batch(
        &mut self,
        batch: HydrationBatch,
    ) -> Result<(), PersistentStoreError> {
        // One fjall batch covers every mutation in the hydration
        // tick atomically — same I-CP1 contract as redb.
        let mut wb = self.db.batch();

        for comp in batch.puts {
            let bytes = encode_composition(&comp)?;
            wb.insert(&self.comps, comp.id.0.as_bytes().to_vec(), bytes);
        }
        for id in batch.removes {
            // Cascade-drop name binding (same as `remove`).
            let prior_name = self
                .names_rev
                .get(id.0.as_bytes())
                .map_err(map_fjall_err)?
                .map(|s| s.to_vec());
            wb.remove(&self.comps, id.0.as_bytes().to_vec());
            if let Some(name_key_bytes) = prior_name {
                wb.remove(&self.names, name_key_bytes);
                wb.remove(&self.names_rev, id.0.as_bytes().to_vec());
            }
        }
        for (ns, name, id) in batch.name_inserts {
            let new_key = name_key(ns, &name);
            let id_bytes = id.0.as_bytes();
            // Overwrite-replace cascade — match `name_insert`.
            let prev_id = self
                .names
                .get(&new_key)
                .map_err(map_fjall_err)?
                .map(|s| s.to_vec());
            let old_rev = self
                .names_rev
                .get(id_bytes)
                .map_err(map_fjall_err)?
                .map(|s| s.to_vec());
            if let Some(prev) = prev_id {
                if prev.as_slice() != id_bytes.as_slice() {
                    wb.remove(&self.names_rev, prev);
                }
            }
            if let Some(old) = old_rev {
                if old != new_key {
                    wb.remove(&self.names, old);
                }
            }
            wb.insert(&self.names, new_key.clone(), id_bytes.to_vec());
            wb.insert(&self.names_rev, id_bytes.to_vec(), new_key);
        }
        for (ns, name) in batch.name_removes {
            let key = name_key(ns, &name);
            let existed_id = self.names.get(&key).map_err(map_fjall_err)?.map(|s| s.to_vec());
            wb.remove(&self.names, key);
            if let Some(id_bytes) = existed_id {
                wb.remove(&self.names_rev, id_bytes);
            }
        }

        // Meta updates ride the same batch so the apply is atomic
        // wrt the rest of the hydration writes (I-CP1).
        wb.insert(
            &self.meta,
            meta_keys::LAST_APPLIED_SEQ.to_vec(),
            batch.new_last_applied_seq.0.to_le_bytes().to_vec(),
        );
        if let Some(stuck) = batch.stuck_state {
            wb.insert(&self.meta, meta_keys::STUCK_STATE.to_vec(), encode_stuck_state(stuck));
        }
        if let Some(halted) = batch.halted {
            wb.insert(
                &self.meta,
                meta_keys::HALTED.to_vec(),
                vec![u8::from(halted)],
            );
        }
        wb.commit().map_err(map_fjall_err)?;
        // Hydration batch always fsyncs — the durability barrier is
        // load-bearing for `last_applied_seq` correctness on
        // restart.
        self.db
            .persist(PersistMode::SyncAll)
            .map_err(map_fjall_err)?;
        Ok(())
    }

    fn put_with_name(
        &mut self,
        comp: Composition,
        ns: NamespaceId,
        name: String,
        prior_id: Option<CompositionId>,
    ) -> Result<(), PersistentStoreError> {
        // One `WriteBatch` covers the composition row + the forward
        // name binding + the reverse name binding atomically. Cuts
        // the gateway PUT path's fjall journal-mutex acquisitions
        // from 4 (two `commit` + two `persist`) to 1 — the rev-2
        // hot bottleneck on the in-process-persistent floor.
        //
        // **Skipped read** vs a `put` + `name_insert` pair: the
        // reverse-name pre-flight on `comp.id`. Caller guarantees
        // freshly-minted comp_id, so the reverse entry is `None`.
        //
        // **Forward cascade** uses `prior_id` when supplied (the
        // gateway path holds the storage lock across its own
        // lookup + this call so the binding state is consistent),
        // falling back to a fresh fjall.get only when the caller
        // didn't have the result handy.
        let id = comp.id;
        let id_bytes = id.0.as_bytes();
        let new_key = name_key(ns, &name);
        let comp_bytes = encode_composition(&comp)?;

        // Resolve the forward cascade: prefer caller-supplied
        // prior_id, fall back to a fjall.get.
        let prev_id_bytes: Option<Vec<u8>> = match prior_id {
            Some(id) => Some(id.0.as_bytes().to_vec()),
            None => self
                .names
                .get(&new_key)
                .map_err(map_fjall_err)?
                .map(|s| s.to_vec()),
        };

        let mut batch = self.db.batch();
        batch.insert(&self.comps, id_bytes.to_vec(), comp_bytes);
        if let Some(prev) = prev_id_bytes {
            if prev.as_slice() != id_bytes.as_slice() {
                batch.remove(&self.names_rev, prev);
            }
        }
        batch.insert(&self.names, new_key.clone(), id_bytes.to_vec());
        batch.insert(&self.names_rev, id_bytes.to_vec(), new_key);
        batch.commit().map_err(map_fjall_err)?;
        self.persist_after_write()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::OrgId;

    fn make_comp(idx: u8) -> Composition {
        use kiseki_common::ids::ChunkId;
        use kiseki_common::ids::ShardId;
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
    fn put_get_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FjallStorage::open(dir.path()).unwrap();
        let comp = make_comp(7);
        store.put(comp.clone()).unwrap();
        assert_eq!(store.get(comp.id).unwrap(), Some(comp));
    }

    #[test]
    fn name_insert_lookup_remove() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FjallStorage::open(dir.path()).unwrap();
        let id = CompositionId(uuid::Uuid::from_u128(42));
        let ns = NamespaceId(uuid::Uuid::from_u128(2));
        store.name_insert(ns, "alpha".into(), id).unwrap();
        assert_eq!(store.name_lookup(ns, "alpha").unwrap(), Some(id));
        assert_eq!(
            store.name_for(id).unwrap(),
            Some((ns, "alpha".to_string()))
        );
        assert!(store.name_remove(ns, "alpha").unwrap());
        assert_eq!(store.name_lookup(ns, "alpha").unwrap(), None);
        assert_eq!(store.name_for(id).unwrap(), None);
    }

    #[test]
    fn name_list_prefix_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FjallStorage::open(dir.path()).unwrap();
        let ns = NamespaceId(uuid::Uuid::from_u128(2));
        for (i, n) in ["alpha", "beta", "alphabet"].iter().enumerate() {
            let id = CompositionId(uuid::Uuid::from_u128(i as u128 + 100));
            store.name_insert(ns, (*n).to_string(), id).unwrap();
        }
        let all = store.name_list(ns, None).unwrap();
        assert_eq!(all.len(), 3);
        let alpha_only = store.name_list(ns, Some("alpha")).unwrap();
        assert_eq!(alpha_only.len(), 2);
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let comp = make_comp(11);
        {
            let mut store = FjallStorage::open(dir.path()).unwrap();
            store.put(comp.clone()).unwrap();
        }
        let store = FjallStorage::open(dir.path()).unwrap();
        assert_eq!(store.get(comp.id).unwrap(), Some(comp));
    }
}
