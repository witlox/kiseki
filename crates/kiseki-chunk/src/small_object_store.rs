//! Small object store — redb-backed KV for inline file content.
//!
//! Stores encrypted content for files below the inline threshold
//! (ADR-030). Keyed by `ChunkId` (32 bytes). Content is the encrypted
//! envelope ciphertext.
//!
//! Lives on the system disk metadata tier (`KISEKI_DATA_DIR/small/objects.redb`).
//! Capacity-managed: entries are GC'd when deltas are truncated (I-SF6).

use std::io;
use std::path::Path;
use std::sync::Mutex;

use kiseki_common::ids::ChunkId;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};

/// Table: `chunk_id` bytes (32) → encrypted content bytes.
const OBJECTS_TABLE: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("small_objects");

/// Redb-backed store for inline small-file content.
///
/// Thread-safe via `Mutex`. Designed for metadata-tier storage on
/// NVMe/SSD system disks.
pub struct SmallObjectStore {
    db: Mutex<Database>,
}

impl SmallObjectStore {
    /// Open or create a small object store at the given path.
    pub fn open(path: &Path) -> io::Result<Self> {
        let db = Database::create(path).map_err(|e| io::Error::other(e.to_string()))?;

        // Ensure table exists.
        let txn = db
            .begin_write()
            .map_err(|e| io::Error::other(e.to_string()))?;
        txn.open_table(OBJECTS_TABLE)
            .map_err(|e| io::Error::other(e.to_string()))?;
        txn.commit().map_err(|e| io::Error::other(e.to_string()))?;

        Ok(Self { db: Mutex::new(db) })
    }

    /// Store inline content for a chunk.
    ///
    /// Returns `true` if this is a new entry, `false` if it already existed
    /// (dedup hit — content not overwritten).
    pub fn put(&self, chunk_id: &ChunkId, data: &[u8]) -> io::Result<bool> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_write()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let is_new;
        {
            let mut table = txn
                .open_table(OBJECTS_TABLE)
                .map_err(|e| io::Error::other(e.to_string()))?;
            is_new = table
                .get(chunk_id.0.as_slice())
                .map_err(|e| io::Error::other(e.to_string()))?
                .is_none();
            if is_new {
                table
                    .insert(chunk_id.0.as_slice(), data)
                    .map_err(|e| io::Error::other(e.to_string()))?;
            }
        }
        txn.commit().map_err(|e| io::Error::other(e.to_string()))?;
        Ok(is_new)
    }

    /// Retrieve inline content for a chunk.
    pub fn get(&self, chunk_id: &ChunkId) -> io::Result<Option<Vec<u8>>> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_read()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let table = txn
            .open_table(OBJECTS_TABLE)
            .map_err(|e| io::Error::other(e.to_string()))?;
        match table.get(chunk_id.0.as_slice()) {
            Ok(Some(guard)) => Ok(Some(guard.value().to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }

    /// Delete inline content for a chunk (GC, I-SF6).
    ///
    /// Returns `true` if the entry existed and was removed.
    pub fn delete(&self, chunk_id: &ChunkId) -> io::Result<bool> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_write()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let existed;
        {
            let mut table = txn
                .open_table(OBJECTS_TABLE)
                .map_err(|e| io::Error::other(e.to_string()))?;
            existed = table
                .remove(chunk_id.0.as_slice())
                .map_err(|e| io::Error::other(e.to_string()))?
                .is_some();
        }
        txn.commit().map_err(|e| io::Error::other(e.to_string()))?;
        Ok(existed)
    }

    /// Check if a chunk exists in the inline store.
    pub fn contains(&self, chunk_id: &ChunkId) -> io::Result<bool> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_read()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let table = txn
            .open_table(OBJECTS_TABLE)
            .map_err(|e| io::Error::other(e.to_string()))?;
        match table.get(chunk_id.0.as_slice()) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }

    /// Count of inline objects.
    pub fn len(&self) -> io::Result<u64> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_read()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let table = txn
            .open_table(OBJECTS_TABLE)
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(table.len().unwrap_or(0))
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> io::Result<bool> {
        self.len().map(|n| n == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_chunk_id(val: u8) -> ChunkId {
        ChunkId([val; 32])
    }

    #[test]
    fn put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = SmallObjectStore::open(&dir.path().join("test.redb")).unwrap();

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
        let store = SmallObjectStore::open(&dir.path().join("test.redb")).unwrap();

        let id = test_chunk_id(0x02);
        assert!(store.put(&id, b"data").unwrap());
        assert!(!store.put(&id, b"data").unwrap()); // dedup
    }

    #[test]
    fn delete_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = SmallObjectStore::open(&dir.path().join("test.redb")).unwrap();

        let id = test_chunk_id(0x03);
        store.put(&id, b"data").unwrap();

        assert!(store.delete(&id).unwrap());
        assert!(!store.delete(&id).unwrap()); // already gone
        assert_eq!(store.get(&id).unwrap(), None);
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist.redb");

        let id = test_chunk_id(0x04);
        {
            let store = SmallObjectStore::open(&path).unwrap();
            store.put(&id, b"persistent").unwrap();
        }
        {
            let store = SmallObjectStore::open(&path).unwrap();
            let got = store.get(&id).unwrap();
            assert_eq!(got, Some(b"persistent".to_vec()));
        }
    }

    #[test]
    fn len_and_contains() {
        let dir = tempfile::tempdir().unwrap();
        let store = SmallObjectStore::open(&dir.path().join("test.redb")).unwrap();

        assert_eq!(store.len().unwrap(), 0);
        assert!(!store.contains(&test_chunk_id(0x05)).unwrap());

        store.put(&test_chunk_id(0x05), b"a").unwrap();
        store.put(&test_chunk_id(0x06), b"b").unwrap();

        assert_eq!(store.len().unwrap(), 2);
        assert!(store.contains(&test_chunk_id(0x05)).unwrap());
    }
}
