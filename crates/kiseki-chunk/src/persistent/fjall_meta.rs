//! Fjall-backed write-through for chunk + fragment metadata
//! (ADR-022 rev-4).
//!
//! Replaces the pre-rev-4 `save_meta` / `save_frag_meta` JSON
//! rewrite-the-world pattern. Every chunk-meta or fragment-meta
//! mutation lands as ONE record write to a fjall keyspace — O(1)
//! per op instead of O(N) in store size.
//!
//! ## Schema
//!
//! Two keyspaces inside one fjall database:
//!
//! - `chunks`    — `chunk_id` (32 B raw) → encoded `ChunkRecord`
//! - `fragments` — `chunk_id || fragment_index_be` (36 B) → encoded
//!   `FragmentRecord`
//!
//! `delete_chunk_force` and other multi-keyspace mutations commit
//! across both in one fjall `WriteBatch`, so cross-keyspace
//! atomicity is preserved (matches the prior JSON pair where both
//! files were rewritten under one chunks-mutex critical section).
//!
//! ## Durability
//!
//! Mirrors `PersistentChunkStore::sync_per_write`:
//!
//! - `sync_per_write = true`  → `WriteBatch.durability(Some(SyncAll))`
//!   (per-write fsync; POSIX-immediate semantics)
//! - `sync_per_write = false` → `WriteBatch` default durability
//!   (no fsync per commit; the runtime's periodic flush task
//!   forces the WAL fsync at a bounded cadence)
//!
//! The gateway's `fsync_pending` hook gets a [`FjallMetaFlusher`]
//! handle clone so explicit `fsync(2)` from FUSE / NFS clients
//! drives a real fjall fsync without contending with concurrent
//! writers.

#![allow(clippy::missing_errors_doc)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use fjall::{Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch, PersistMode};
use kiseki_common::ids::ChunkId;

use super::encoding::{
    chunk_key, decode_chunk, decode_fragment, decode_fragment_key, encode_chunk, encode_fragment,
    fragment_key, ChunkRecord, FragmentRecord,
};
use crate::error::ChunkError;

const KS_CHUNKS: &str = "chunks";
const KS_FRAGMENTS: &str = "fragments";

/// Fjall-backed meta store.
///
/// Internally cheap to clone (`Database` and `Keyspace` are
/// `Arc`-shaped); same handle is shared between the
/// [`crate::PersistentChunkStore`] write path and the
/// [`FjallMetaFlusher`] used by the gateway's `fsync_pending` hook.
pub struct FjallMetaStore {
    db: Database,
    chunks_ks: Keyspace,
    fragments_ks: Keyspace,
    /// Mirrors `PersistentChunkStore::sync_per_write`. Reads
    /// per-write on the commit path, set via
    /// `PersistentChunkStore::set_sync_per_write`.
    sync_per_write: AtomicBool,
}

impl FjallMetaStore {
    /// Open or create a fjall database at `path`. The path is a
    /// directory (fjall keyspace layout); pre-rev-4 callers passing
    /// a `*.json` file path migrated to a sibling directory name
    /// without an extension.
    pub fn open(path: &Path) -> Result<Self, ChunkError> {
        let db = Database::builder(path).open()?;
        let chunks_ks = db.keyspace(KS_CHUNKS, KeyspaceCreateOptions::default)?;
        let fragments_ks = db.keyspace(KS_FRAGMENTS, KeyspaceCreateOptions::default)?;
        Ok(Self {
            db,
            chunks_ks,
            fragments_ks,
            sync_per_write: AtomicBool::new(true),
        })
    }

    /// Toggle inline-fsync vs buffered durability. Defaults to
    /// `true` so a freshly-opened store carries POSIX-immediate
    /// semantics until the runtime explicitly relaxes it (group-
    /// commit mode for the perf path).
    pub fn set_sync_per_write(&self, enabled: bool) {
        self.sync_per_write.store(enabled, Ordering::Relaxed);
    }

    /// Force an fsync of the WAL. Used by the runtime's periodic
    /// flusher and the gateway's `fsync_pending` hook.
    pub fn flush(&self) -> Result<(), ChunkError> {
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    /// Cheap clonable handle to the underlying database for off-
    /// thread fsync. Matches the
    /// `kiseki_composition::persistent::FjallFlusher` shape so the
    /// gateway can register a chunk-meta fsync hook the same way it
    /// already does for the composition store.
    #[must_use]
    pub fn flusher(&self) -> FjallMetaFlusher {
        FjallMetaFlusher {
            db: self.db.clone(),
        }
    }

    /// Build a fresh batch with the durability mode the
    /// `sync_per_write` flag dictates. `None` durability =
    /// "queue the WAL bytes; periodic flusher fsyncs".
    fn batch_for_write(&self) -> OwnedWriteBatch {
        let durability = if self.sync_per_write.load(Ordering::Relaxed) {
            Some(PersistMode::SyncAll)
        } else {
            None
        };
        self.db.batch().durability(durability)
    }

    // -- Chunk-record operations -------------------------------------

    /// Insert or replace a chunk record. Used by every mutating op
    /// on the chunk store path that touches `envelope_meta`
    /// (`write_chunk` new write, increment, decrement, retention
    /// holds, etc.). Caller updates the in-memory cache first; this
    /// is the WAL behind it.
    ///
    /// # Errors
    /// [`ChunkError::Fjall`] if the underlying batch commit fails;
    /// [`ChunkError::Io`] (via [`encode_chunk`]) on encoder fault.
    pub fn put_chunk(&self, record: &ChunkRecord) -> Result<(), ChunkError> {
        let bytes = encode_chunk(record)?;
        let key = chunk_key(&ChunkId(record.chunk_id));
        let mut batch = self.batch_for_write();
        batch.insert(&self.chunks_ks, key.to_vec(), bytes);
        batch.commit().map_err(ChunkError::from)
    }

    /// Remove a chunk record. Used by `gc` and `delete_chunk_force`.
    pub fn remove_chunk(&self, id: &ChunkId) -> Result<(), ChunkError> {
        let key = chunk_key(id);
        let mut batch = self.batch_for_write();
        batch.remove(&self.chunks_ks, key.to_vec());
        batch.commit().map_err(ChunkError::from)
    }

    /// Drain every persisted chunk record into memory. Called once
    /// from `PersistentChunkStore::open` to seed the in-memory
    /// cache; no callers on the hot path.
    pub fn iter_chunks(&self) -> Result<Vec<ChunkRecord>, ChunkError> {
        let mut out = Vec::new();
        for entry in self.chunks_ks.iter() {
            let (_k, v) = entry.into_inner()?;
            out.push(decode_chunk(v.as_ref())?);
        }
        Ok(out)
    }

    // -- Fragment-record operations ----------------------------------

    /// Insert or replace a fragment record. Used on the EC write
    /// path whenever a `(chunk_id, fragment_index)` tuple lands.
    /// Caller updates the in-memory `fragments` cache first; this
    /// is the WAL behind it.
    ///
    /// # Errors
    /// [`ChunkError::Fjall`] if the underlying batch commit fails;
    /// [`ChunkError::Io`] (via [`encode_fragment`]) on encoder fault.
    pub fn put_fragment(&self, record: &FragmentRecord) -> Result<(), ChunkError> {
        let bytes = encode_fragment(record)?;
        let key = fragment_key(&ChunkId(record.chunk_id), record.fragment_index);
        let mut batch = self.batch_for_write();
        batch.insert(&self.fragments_ks, key.to_vec(), bytes);
        batch.commit().map_err(ChunkError::from)
    }

    /// Remove a single `(chunk_id, fragment_index)` row. Used by the
    /// scrub when an orphan fragment is reaped.
    ///
    /// # Errors
    /// [`ChunkError::Fjall`] if the batch commit fails.
    pub fn remove_fragment(&self, id: &ChunkId, fragment_index: u32) -> Result<(), ChunkError> {
        let key = fragment_key(id, fragment_index);
        let mut batch = self.batch_for_write();
        batch.remove(&self.fragments_ks, key.to_vec());
        batch.commit().map_err(ChunkError::from)
    }

    /// Atomically remove the chunk record AND every fragment under
    /// the same `chunk_id`. Used by `delete_chunk_force` so the
    /// (chunk, fragment) pair stays consistent across crash even if
    /// the test-only forced-delete is interrupted between two
    /// keyspaces. Caller passes the list of fragment indices it
    /// already enumerated from the in-memory cache.
    pub fn remove_chunk_and_fragments(
        &self,
        id: &ChunkId,
        fragment_indices: &[u32],
    ) -> Result<(), ChunkError> {
        let mut batch = self.batch_for_write();
        batch.remove(&self.chunks_ks, chunk_key(id).to_vec());
        for &idx in fragment_indices {
            batch.remove(&self.fragments_ks, fragment_key(id, idx).to_vec());
        }
        batch.commit().map_err(ChunkError::from)
    }

    /// Drain every persisted fragment record into memory. Called
    /// once from `PersistentChunkStore::open` to seed the in-memory
    /// `fragments` cache; no callers on the hot path. Validates the
    /// key shape so a corrupted dir surfaces a decode error rather
    /// than loading garbage.
    ///
    /// # Errors
    /// [`ChunkError::Fjall`] on iterator I/O; [`ChunkError::Io`] on
    /// key/payload decode failure.
    pub fn iter_fragments(&self) -> Result<Vec<FragmentRecord>, ChunkError> {
        let mut out = Vec::new();
        for entry in self.fragments_ks.iter() {
            let (k, v) = entry.into_inner()?;
            // Validate the key shape so a corrupted dir doesn't
            // silently load garbage records.
            decode_fragment_key(k.as_ref())?;
            out.push(decode_fragment(v.as_ref())?);
        }
        Ok(out)
    }
}

/// Off-thread fsync handle for the gateway `fsync_pending` hook.
///
/// `Database::clone` is cheap (Arc-shaped). The handle drives a
/// `PersistMode::SyncAll` against the shared database — the same
/// fsync the `sync_per_write=true` write path would have done, but
/// callable from outside the `PersistentChunkStore` mutex chain so
/// FUSE / NFS `fsync(2)` from one thread doesn't queue behind a
/// concurrent `write_chunk` on another.
#[derive(Clone)]
pub struct FjallMetaFlusher {
    db: Database,
}

impl FjallMetaFlusher {
    /// Force a `PersistMode::SyncAll` against the underlying
    /// database — drives the WAL fsync that the per-write path
    /// would otherwise skip when `sync_per_write = false`.
    ///
    /// # Errors
    /// [`ChunkError::Fjall`] if the persist call fails.
    pub fn flush(&self) -> Result<(), ChunkError> {
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: u8) -> ChunkRecord {
        ChunkRecord {
            chunk_id: [id; 32],
            refcount: 1,
            retention_holds: Vec::new(),
            pool_name: "default".into(),
            stored_bytes: 4096,
            data_bytes: 4000,
            extent_offset: u64::from(id) * 4096,
            extent_length: 4096,
            extra_extents: Vec::new(),
            nonce: [0; 12],
            auth_tag: [0; 16],
            system_epoch: 1,
            tenant_epoch: None,
            tenant_wrapped_material: None,
        }
    }

    #[test]
    fn chunk_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallMetaStore::open(&dir.path().join("meta")).unwrap();
        let r = sample(7);
        store.put_chunk(&r).unwrap();
        let back = store.iter_chunks().unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0], r);
    }

    #[test]
    fn chunk_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallMetaStore::open(&dir.path().join("meta")).unwrap();
        let r = sample(11);
        store.put_chunk(&r).unwrap();
        store.remove_chunk(&ChunkId(r.chunk_id)).unwrap();
        assert!(store.iter_chunks().unwrap().is_empty());
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta");
        let r = sample(13);
        {
            let store = FjallMetaStore::open(&path).unwrap();
            store.put_chunk(&r).unwrap();
            store.flush().unwrap();
        }
        let store = FjallMetaStore::open(&path).unwrap();
        let back = store.iter_chunks().unwrap();
        assert_eq!(back, vec![r]);
    }

    #[test]
    fn fragment_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallMetaStore::open(&dir.path().join("meta")).unwrap();
        let f = FragmentRecord {
            chunk_id: [0xAA; 32],
            fragment_index: 3,
            extent_offset: 1024,
            extent_length: 256,
            data_bytes: 200,
        };
        store.put_fragment(&f).unwrap();
        let back = store.iter_fragments().unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0], f);
    }

    #[test]
    fn remove_chunk_and_fragments_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallMetaStore::open(&dir.path().join("meta")).unwrap();
        let id = ChunkId([0xCC; 32]);
        let mut record = sample(0xCC);
        record.chunk_id = id.0;
        store.put_chunk(&record).unwrap();
        for idx in 0..4u32 {
            store
                .put_fragment(&FragmentRecord {
                    chunk_id: id.0,
                    fragment_index: idx,
                    extent_offset: u64::from(idx) * 1024,
                    extent_length: 1024,
                    data_bytes: 1000,
                })
                .unwrap();
        }
        store
            .remove_chunk_and_fragments(&id, &[0, 1, 2, 3])
            .unwrap();
        assert!(store.iter_chunks().unwrap().is_empty());
        assert!(store.iter_fragments().unwrap().is_empty());
    }
}
