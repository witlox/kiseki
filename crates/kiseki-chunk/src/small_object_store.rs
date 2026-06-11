//! Small object store — fjall-backed KV for inline file content
//! (ADR-022 rev-5).
//!
//! Stores encrypted content for files below the inline threshold
//! (ADR-030). Keyed by `ChunkId` (32 bytes). Content is the encrypted
//! envelope ciphertext.
//!
//! ## Backend (ADR-022 rev-5)
//!
//! Was `redb::Database` with `Mutex<Database>` + per-write
//! `begin_write` → `txn.commit` (fsync). Under sustained PUT load
//! that's a serialized + per-write fsync bottleneck — exactly the
//! shape ADR-022 rev-2/3/4 moved every other hot path off
//! (35k → 125k op/s for the in-process-persistent path). Rev-5
//! finishes the sweep for ADR-030's inline path.
//!
//! New shape (mirrors `kiseki-log::FjallIntentStore` rev-3 +
//! `FjallMetaStore` rev-4):
//!   - Single fjall database at the resolver-supplied path
//!     (`<resolved SmallObject mount>/kiseki/small-object`).
//!   - One keyspace, `objects` (`chunk_id` 32 B → encrypted envelope).
//!   - Optional `SmallObjectStore::set_sync_per_write(false)` to
//!     queue WAL bytes; a periodic flusher fsyncs at a bounded
//!     cadence (mirrors `PersistentChunkStore::sync_per_write`).
//!   - `InlineStore` trait shape is unchanged — every caller
//!     (`InMemoryGateway::small_store`, ADR-049 `inline_payloads`
//!     apply path) continues to work.
//!
//! ## Path shape
//!
//! Pre-rev-5: `<data_dir>/small/objects.redb` (a redb file).
//! Post-rev-5: `<resolved SmallObject mount>/kiseki/small-object/`
//! (a fjall directory). `runtime.rs` passes the resolver output
//! verbatim; phase 5a's I-CP-Move gate runs BEFORE this open.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};
use kiseki_common::ids::ChunkId;

const KS_OBJECTS: &str = "objects";

/// Fjall-backed store for inline small-file content (ADR-022 rev-5).
///
/// Internally cheap to clone via the underlying `Database` /
/// `Keyspace` (both Arc-shaped) when the runtime wires the
/// `fsync_pending` hook (ADR-046 group-commit chain) to a
/// `SmallObjectFlusher` handle.
pub struct SmallObjectStore {
    db: Database,
    objects: Keyspace,
    /// Mirrors `PersistentChunkStore::sync_per_write`. Defaults to
    /// `true` so POSIX-immediate durability holds out-of-the-box;
    /// the runtime opts into group-commit by calling
    /// `set_sync_per_write(false)` and registering the flusher hook.
    sync_per_write: AtomicBool,
    /// Approximate entry count, kept current via the put/delete
    /// hot paths so `len()` is O(1). Reset to `db_iter_count` at
    /// open. Used by ADR-030 §3 D10 observability gauges.
    approx_len: AtomicU64,
}

impl SmallObjectStore {
    /// Open or create a small object store at the given path.
    ///
    /// `path` is a **directory** (the fjall keyspace layout). Pre-rev-5
    /// callers passing `objects.redb` continue to work because the
    /// fjall directory is created at that name — the file extension
    /// is just a path segment that fjall treats as a directory name.
    /// Production callers (`runtime.rs` after phase 5b) pass the
    /// resolver-supplied path which is already a directory under
    /// `<mount>/kiseki/small-object/`.
    ///
    /// # Errors
    /// Returns the underlying `io::Error` on filesystem failure or
    /// keyspace open failure.
    pub fn open(path: &Path) -> io::Result<Self> {
        let db = Database::builder(path)
            .open()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let objects = db
            .keyspace(KS_OBJECTS, KeyspaceCreateOptions::default)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut initial_len = 0u64;
        for entry in objects.iter() {
            if entry.into_inner().is_ok() {
                initial_len += 1;
            }
        }
        let approx_len = AtomicU64::new(initial_len);
        Ok(Self {
            db,
            objects,
            sync_per_write: AtomicBool::new(true),
            approx_len,
        })
    }

    /// Toggle inline-fsync vs buffered durability. Defaults to
    /// `true` so a freshly-opened store carries POSIX-immediate
    /// semantics until the runtime explicitly relaxes it for the
    /// group-commit perf path. Mirrors `FjallMetaStore::set_sync_per_write`.
    pub fn set_sync_per_write(&self, enabled: bool) {
        self.sync_per_write.store(enabled, Ordering::Relaxed);
    }

    /// Force an fsync of the WAL. Used by the runtime's periodic
    /// flusher and the gateway's `fsync_pending` hook (ADR-046
    /// group-commit chain).
    ///
    /// # Errors
    /// Returns the underlying `io::Error` on persist failure.
    pub fn flush(&self) -> io::Result<()> {
        self.db
            .persist(PersistMode::SyncAll)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    /// Cheap clonable handle to the underlying database for off-
    /// thread fsync. Mirrors `FjallMetaFlusher` so the gateway can
    /// register a small-object fsync hook the same way it does for
    /// chunk meta.
    #[must_use]
    pub fn flusher(&self) -> SmallObjectFlusher {
        SmallObjectFlusher {
            db: self.db.clone(),
        }
    }

    fn batch_durability(&self) -> PersistMode {
        if self.sync_per_write.load(Ordering::Relaxed) {
            PersistMode::SyncAll
        } else {
            // Buffer, NOT None: `None` parks committed bytes in fjall's
            // user-space `BufWriter`, where a plain process crash loses
            // them. `Buffer` flushes to OS page cache, which is the
            // durability point I-L5 approves for group-commit stores
            // (survives process crash; the periodic flusher bounds the
            // power-loss window).
            PersistMode::Buffer
        }
    }

    /// Store inline content for a chunk.
    ///
    /// Returns `true` if this is a new entry, `false` if it already
    /// existed (dedup hit — content not overwritten).
    ///
    /// # Errors
    /// Returns the underlying `io::Error` on commit failure.
    pub fn put(&self, chunk_id: &ChunkId, data: &[u8]) -> io::Result<bool> {
        // Read-then-write under the same fjall snapshot: a concurrent
        // writer with the same chunk_id would either land before
        // (so we see Some, return false) or after (so we land Ok,
        // increment approx_len). Both branches are correctness-safe
        // even if approx_len drifts by one — it's advisory.
        let existed = self
            .objects
            .get(chunk_id.0.as_slice())
            .map_err(|e| io::Error::other(e.to_string()))?
            .is_some();
        if existed {
            return Ok(false);
        }
        let mut batch = self.db.batch().durability(Some(self.batch_durability()));
        batch.insert(&self.objects, chunk_id.0.to_vec(), data.to_vec());
        batch
            .commit()
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.approx_len.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    /// Batched [`SmallObjectStore::put`] (#212): one fjall batch + one
    /// commit for every new entry, instead of N independent commits.
    /// The Raft apply path submits all inline payloads of one entry
    /// here so the journal commit (and its fsync when
    /// `sync_per_write` is on) amortises across the batch.
    ///
    /// Existing keys are skipped (dedup, mirroring `put`'s `false`).
    /// Returns the number of newly-stored entries.
    ///
    /// # Errors
    /// Returns the underlying `io::Error` on read or commit failure.
    pub fn put_many(&self, items: &[(&[u8; 32], &[u8])]) -> io::Result<u64> {
        let mut batch = self.db.batch().durability(Some(self.batch_durability()));
        let mut new_count: u64 = 0;
        // Dedup within the batch too: the existence check below reads
        // committed state, so a key repeated in `items` (same content
        // → same content-addressed chunk_id) would otherwise double-
        // insert and drift `approx_len`.
        let mut staged: std::collections::HashSet<&[u8; 32]> = std::collections::HashSet::new();
        for (key, data) in items {
            if !staged.insert(key) {
                continue;
            }
            let existed = self
                .objects
                .get(key.as_slice())
                .map_err(|e| io::Error::other(e.to_string()))?
                .is_some();
            if existed {
                continue;
            }
            batch.insert(&self.objects, key.to_vec(), data.to_vec());
            new_count += 1;
        }
        if new_count == 0 {
            return Ok(0);
        }
        batch
            .commit()
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.approx_len.fetch_add(new_count, Ordering::Relaxed);
        Ok(new_count)
    }

    /// Retrieve inline content for a chunk.
    ///
    /// # Errors
    /// Returns the underlying `io::Error` on read failure.
    pub fn get(&self, chunk_id: &ChunkId) -> io::Result<Option<Vec<u8>>> {
        match self.objects.get(chunk_id.0.as_slice()) {
            Ok(Some(slice)) => Ok(Some(slice.to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }

    /// Delete inline content for a chunk (GC, I-SF6).
    ///
    /// Returns `true` if the entry existed and was removed.
    ///
    /// # Errors
    /// Returns the underlying `io::Error` on commit failure.
    pub fn delete(&self, chunk_id: &ChunkId) -> io::Result<bool> {
        let existed = self
            .objects
            .get(chunk_id.0.as_slice())
            .map_err(|e| io::Error::other(e.to_string()))?
            .is_some();
        if !existed {
            return Ok(false);
        }
        let mut batch = self.db.batch().durability(Some(self.batch_durability()));
        batch.remove(&self.objects, chunk_id.0.to_vec());
        batch
            .commit()
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.approx_len.fetch_sub(1, Ordering::Relaxed);
        Ok(true)
    }

    /// Batched delete — ONE fjall batch commit for the whole set
    /// (#226 100k attempt). The watermark-advance prune previously
    /// called [`Self::delete`] per inline delta entry: one point read
    /// plus one journal commit EACH, serialized inside the SM apply
    /// lock — ~100k+ commits per advance at 23k PUT/s. The existence
    /// reads stay (they keep `approx_len` honest) but the commit
    /// amortizes to one journal write.
    ///
    /// # Errors
    /// Returns the underlying `io::Error` on read or commit failure.
    pub fn delete_many(&self, keys: &[[u8; 32]]) -> io::Result<u64> {
        let mut batch = self.db.batch().durability(Some(self.batch_durability()));
        let mut removed: u64 = 0;
        for key in keys {
            let existed = self
                .objects
                .get(key.as_slice())
                .map_err(|e| io::Error::other(e.to_string()))?
                .is_some();
            if !existed {
                continue;
            }
            batch.remove(&self.objects, key.to_vec());
            removed += 1;
        }
        if removed == 0 {
            return Ok(0);
        }
        batch
            .commit()
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.approx_len.fetch_sub(removed, Ordering::Relaxed);
        Ok(removed)
    }

    /// Check if a chunk exists in the inline store.
    ///
    /// # Errors
    /// Returns the underlying `io::Error` on read failure.
    pub fn contains(&self, chunk_id: &ChunkId) -> io::Result<bool> {
        self.objects
            .get(chunk_id.0.as_slice())
            .map(|v| v.is_some())
            .map_err(|e| io::Error::other(e.to_string()))
    }

    /// Approximate count of inline objects. O(1); maintained on the
    /// put/delete hot paths.
    ///
    /// # Errors
    /// Never fails (Result kept for API stability with the redb
    /// implementation it replaces).
    #[allow(clippy::unnecessary_wraps)]
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.approx_len.load(Ordering::Relaxed))
    }

    /// Whether the store is empty.
    ///
    /// # Errors
    /// Never fails.
    #[allow(clippy::unnecessary_wraps)]
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.approx_len.load(Ordering::Relaxed) == 0)
    }
}

impl kiseki_common::inline_store::InlineStore for SmallObjectStore {
    fn put(&self, key: &[u8; 32], data: &[u8]) -> io::Result<bool> {
        self.put(&ChunkId(*key), data)
    }

    fn put_many(&self, items: &[(&[u8; 32], &[u8])]) -> io::Result<u64> {
        Self::put_many(self, items)
    }

    fn get(&self, key: &[u8; 32]) -> io::Result<Option<Vec<u8>>> {
        self.get(&ChunkId(*key))
    }

    fn delete(&self, key: &[u8; 32]) -> io::Result<bool> {
        self.delete(&ChunkId(*key))
    }

    fn delete_many(&self, keys: &[[u8; 32]]) -> io::Result<u64> {
        Self::delete_many(self, keys)
    }

    fn flush(&self) -> io::Result<()> {
        Self::flush(self)
    }
}

/// Off-thread fsync handle. Mirrors `FjallMetaFlusher` (ADR-022
/// rev-4) so the gateway's `fsync_pending` hook chain treats
/// inline + chunk-meta uniformly.
#[derive(Clone)]
pub struct SmallObjectFlusher {
    db: Database,
}

impl SmallObjectFlusher {
    /// Force a `PersistMode::SyncAll` against the underlying database
    /// — drives the WAL fsync that the per-write path would otherwise
    /// skip when `sync_per_write = false`.
    ///
    /// # Errors
    /// Returns the underlying `io::Error` on persist failure.
    pub fn flush(&self) -> io::Result<()> {
        self.db
            .persist(PersistMode::SyncAll)
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_chunk_id(val: u8) -> ChunkId {
        ChunkId([val; 32])
    }

    fn open_at(dir: &tempfile::TempDir, name: &str) -> SmallObjectStore {
        SmallObjectStore::open(&dir.path().join(name)).expect("open ok")
    }

    #[test]
    fn put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_at(&dir, "test-store");

        let id = test_chunk_id(0x01);
        let data = b"encrypted inline content";

        let is_new = store.put(&id, data).unwrap();
        assert!(is_new);

        let got = store.get(&id).unwrap();
        assert_eq!(got, Some(data.to_vec()));
    }

    #[test]
    fn dedup_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_at(&dir, "test-dedup");

        let id = test_chunk_id(0x02);
        assert!(store.put(&id, b"data").unwrap());
        assert!(!store.put(&id, b"data").unwrap()); // dedup
    }

    #[test]
    fn delete_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_at(&dir, "test-delete");

        let id = test_chunk_id(0x03);
        store.put(&id, b"data").unwrap();

        assert!(store.delete(&id).unwrap());
        assert!(!store.delete(&id).unwrap()); // already gone
        assert_eq!(store.get(&id).unwrap(), None);
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist-store");

        let id = test_chunk_id(0x04);
        {
            let store = SmallObjectStore::open(&path).expect("open ok");
            store.put(&id, b"persistent").unwrap();
            store.flush().unwrap(); // ensure WAL fsync survives drop
        }
        {
            let store = SmallObjectStore::open(&path).expect("reopen ok");
            let got = store.get(&id).unwrap();
            assert_eq!(got, Some(b"persistent".to_vec()));
        }
    }

    #[test]
    fn len_and_contains() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_at(&dir, "test-len");

        assert_eq!(store.len().unwrap(), 0);
        assert!(!store.contains(&test_chunk_id(0x05)).unwrap());

        store.put(&test_chunk_id(0x05), b"a").unwrap();
        store.put(&test_chunk_id(0x06), b"b").unwrap();

        assert_eq!(store.len().unwrap(), 2);
        assert!(store.contains(&test_chunk_id(0x05)).unwrap());
        assert!(store.contains(&test_chunk_id(0x06)).unwrap());
        assert!(!store.contains(&test_chunk_id(0x07)).unwrap());
    }

    #[test]
    fn set_sync_per_write_toggles_durability() {
        // Tests the API surface (not the on-disk fsync rate — that
        // needs a benchmark, not a unit test).
        let dir = tempfile::tempdir().unwrap();
        let store = open_at(&dir, "test-sync");
        store.set_sync_per_write(false);
        let id = test_chunk_id(0x08);
        store.put(&id, b"buffered").unwrap();
        // Explicit flush still fsyncs.
        store.flush().unwrap();
        assert_eq!(store.get(&id).unwrap(), Some(b"buffered".to_vec()));
    }

    #[test]
    fn flusher_handle_round_trips_via_explicit_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_at(&dir, "test-flusher");
        let flusher = store.flusher();
        let id = test_chunk_id(0x09);
        store.set_sync_per_write(false);
        store.put(&id, b"data").unwrap();
        flusher.flush().unwrap();
        // The flusher uses the shared Database handle; reads via
        // the original store still see the data.
        assert_eq!(store.get(&id).unwrap(), Some(b"data".to_vec()));
    }

    #[test]
    fn len_persists_across_reopen() {
        // approx_len is rebuilt from the iterator at open; verifies
        // the post-restart count is accurate.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-len-persist");
        {
            let store = SmallObjectStore::open(&path).expect("open ok");
            store.put(&test_chunk_id(0x10), b"a").unwrap();
            store.put(&test_chunk_id(0x11), b"b").unwrap();
            store.put(&test_chunk_id(0x12), b"c").unwrap();
            store.flush().unwrap();
        }
        {
            let store = SmallObjectStore::open(&path).expect("reopen ok");
            assert_eq!(store.len().unwrap(), 3);
        }
    }

    #[test]
    fn put_many_mixed_new_dup_and_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_at(&dir, "test-put-many");

        // Pre-existing entry: must be skipped (dedup).
        store.put(&test_chunk_id(0x20), b"existing").unwrap();

        let k_existing = [0x20u8; 32];
        let k_new_a = [0x21u8; 32];
        let k_new_b = [0x22u8; 32];
        let items: Vec<(&[u8; 32], &[u8])> = vec![
            (&k_existing, b"existing"),
            (&k_new_a, b"a"),
            (&k_new_b, b"b"),
            // Repeated within the same batch: counted once.
            (&k_new_a, b"a"),
        ];
        let new_count = store.put_many(&items).unwrap();
        assert_eq!(new_count, 2);
        assert_eq!(store.len().unwrap(), 3);
        assert_eq!(
            store.get(&test_chunk_id(0x21)).unwrap(),
            Some(b"a".to_vec())
        );
        assert_eq!(
            store.get(&test_chunk_id(0x22)).unwrap(),
            Some(b"b".to_vec())
        );
    }

    #[test]
    fn put_many_all_dups_commits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_at(&dir, "test-put-many-dups");
        store.put(&test_chunk_id(0x30), b"x").unwrap();
        let k = [0x30u8; 32];
        let items: Vec<(&[u8; 32], &[u8])> = vec![(&k, b"x")];
        assert_eq!(store.put_many(&items).unwrap(), 0);
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn buffered_put_many_survives_flush_and_reopen() {
        // Group-commit shape (#212): relaxed durability + explicit
        // flush must make the batch durable across reopen.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-buffered-reopen");
        let k_a = [0x40u8; 32];
        let k_b = [0x41u8; 32];
        {
            let store = SmallObjectStore::open(&path).expect("open ok");
            store.set_sync_per_write(false);
            let items: Vec<(&[u8; 32], &[u8])> = vec![(&k_a, b"a"), (&k_b, b"b")];
            assert_eq!(store.put_many(&items).unwrap(), 2);
            store.flush().unwrap();
        }
        {
            let store = SmallObjectStore::open(&path).expect("reopen ok");
            assert_eq!(
                store.get(&test_chunk_id(0x40)).unwrap(),
                Some(b"a".to_vec())
            );
            assert_eq!(
                store.get(&test_chunk_id(0x41)).unwrap(),
                Some(b"b".to_vec())
            );
            assert_eq!(store.len().unwrap(), 2);
        }
    }

    #[test]
    fn inline_store_trait_round_trips() {
        use kiseki_common::inline_store::InlineStore;
        let dir = tempfile::tempdir().unwrap();
        let store = open_at(&dir, "test-trait");
        let key: [u8; 32] = [0x42; 32];
        assert!(InlineStore::put(&store, &key, b"trait-data").unwrap());
        assert_eq!(
            InlineStore::get(&store, &key).unwrap(),
            Some(b"trait-data".to_vec())
        );
        assert!(InlineStore::delete(&store, &key).unwrap());
        assert!(!InlineStore::delete(&store, &key).unwrap());
    }
}
