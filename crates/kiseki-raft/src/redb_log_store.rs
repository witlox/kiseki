//! redb-backed Raft log store — persistent, ACID, pure Rust.
//!
//! Stores Raft log entries, vote state, and committed index in a
//! single redb database file. Crash-safe via copy-on-write B-tree.
//! Per ADR-022.

use std::io;
use std::path::Path;
use std::sync::Mutex;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{de::DeserializeOwned, Serialize};

/// Table for Raft log entries: key = log index (u64), value = JSON bytes.
const LOG_TABLE: TableDefinition<'_, u64, &[u8]> = TableDefinition::new("raft_log");

/// Table for Raft metadata: key = name string, value = JSON bytes.
const META_TABLE: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("raft_meta");

/// redb-backed Raft log store.
///
/// Thread-safe via internal `Mutex` on the redb `Database`.
/// Suitable for single-node Raft; multi-node would need the database
/// on a persistent volume.
pub struct RedbLogStore {
    db: Mutex<Database>,
}

impl RedbLogStore {
    /// Open or create a redb database at the given path.
    pub fn open(path: &Path) -> io::Result<Self> {
        let db = Database::create(path)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        // Ensure tables exist.
        let txn = db
            .begin_write()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        txn.open_table(LOG_TABLE)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        txn.open_table(META_TABLE)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        txn.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        Ok(Self { db: Mutex::new(db) })
    }

    /// Append a log entry (serialized as JSON).
    pub fn append<T: Serialize>(&self, index: u64, entry: &T) -> io::Result<()> {
        let db = self.db.lock().unwrap();
        let data =
            serde_json::to_vec(entry).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let txn = db
            .begin_write()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        {
            let mut table = txn
                .open_table(LOG_TABLE)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            table
                .insert(index, data.as_slice())
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        }
        txn.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }

    /// Read a log entry by index.
    pub fn get<T: DeserializeOwned>(&self, index: u64) -> io::Result<Option<T>> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_read()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let table = txn
            .open_table(LOG_TABLE)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        match table.get(index) {
            Ok(Some(guard)) => {
                let val: T = serde_json::from_slice(guard.value())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(val))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }
    }

    /// Read all entries in range [from, to] inclusive.
    pub fn range<T: DeserializeOwned>(&self, from: u64, to: u64) -> io::Result<Vec<(u64, T)>> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_read()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let table = txn
            .open_table(LOG_TABLE)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        let mut result = Vec::new();
        let range = table
            .range(from..=to)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        for entry in range {
            let (k, v) = entry.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            let val: T = serde_json::from_slice(v.value())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            result.push((k.value(), val));
        }
        Ok(result)
    }

    /// Truncate log entries before the given index (exclusive).
    pub fn truncate_before(&self, before: u64) -> io::Result<u64> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_write()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let mut count = 0u64;
        {
            let mut table = txn
                .open_table(LOG_TABLE)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            // Collect keys to remove.
            let keys: Vec<u64> = {
                let range = table
                    .range(0..before)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                range
                    .filter_map(|e| e.ok().map(|(k, _)| k.value()))
                    .collect()
            };
            for key in keys {
                table
                    .remove(key)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                count += 1;
            }
        }
        txn.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(count)
    }

    /// Store a metadata value (e.g., vote, committed index).
    pub fn set_meta<T: Serialize>(&self, key: &str, value: &T) -> io::Result<()> {
        let db = self.db.lock().unwrap();
        let data =
            serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let txn = db
            .begin_write()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        {
            let mut table = txn
                .open_table(META_TABLE)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            table
                .insert(key, data.as_slice())
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        }
        txn.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }

    /// Read a metadata value.
    pub fn get_meta<T: DeserializeOwned>(&self, key: &str) -> io::Result<Option<T>> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_read()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let table = txn
            .open_table(META_TABLE)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        match table.get(key) {
            Ok(Some(guard)) => {
                let val: T = serde_json::from_slice(guard.value())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(val))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }
    }

    /// Truncate log entries after the given index (exclusive).
    ///
    /// Removes all entries with index > `after`. Used by openraft's
    /// `truncate` operation when a leader overwrites a follower's
    /// conflicting log suffix.
    pub fn truncate_after(&self, after: u64) -> io::Result<u64> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_write()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let mut count = 0u64;
        {
            let mut table = txn
                .open_table(LOG_TABLE)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            let keys: Vec<u64> = {
                let range = table
                    .range((after + 1)..=u64::MAX)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                range
                    .filter_map(|e| e.ok().map(|(k, _)| k.value()))
                    .collect()
            };
            for key in keys {
                table
                    .remove(key)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                count += 1;
            }
        }
        txn.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(count)
    }

    /// Get the last (highest index) log entry key, or None if empty.
    pub fn last_index(&self) -> io::Result<Option<u64>> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_read()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let table = txn
            .open_table(LOG_TABLE)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let last = table
            .range(0..=u64::MAX)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?
            .last();
        match last {
            Some(Ok((k, _))) => Ok(Some(k.value())),
            Some(Err(e)) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
            None => Ok(None),
        }
    }

    /// Count of log entries.
    pub fn len(&self) -> io::Result<u64> {
        let db = self.db.lock().unwrap();
        let txn = db
            .begin_read()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let table = txn
            .open_table(LOG_TABLE)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let count = table
            .range(0..=u64::MAX)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?
            .count();
        Ok(count as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbLogStore::open(&dir.path().join("test.redb")).unwrap();

        store.append(1, &"entry-one").unwrap();
        store.append(2, &"entry-two").unwrap();

        let v1: Option<String> = store.get(1).unwrap();
        assert_eq!(v1, Some("entry-one".to_string()));

        let v2: Option<String> = store.get(2).unwrap();
        assert_eq!(v2, Some("entry-two".to_string()));

        let v3: Option<String> = store.get(3).unwrap();
        assert_eq!(v3, None);
    }

    #[test]
    fn range_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbLogStore::open(&dir.path().join("test.redb")).unwrap();

        for i in 1..=5 {
            store.append(i, &format!("entry-{i}")).unwrap();
        }

        let range: Vec<(u64, String)> = store.range(2, 4).unwrap();
        assert_eq!(range.len(), 3);
        assert_eq!(range[0], (2, "entry-2".to_string()));
        assert_eq!(range[2], (4, "entry-4".to_string()));
    }

    #[test]
    fn truncate_before() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbLogStore::open(&dir.path().join("test.redb")).unwrap();

        for i in 1..=5 {
            store.append(i, &format!("entry-{i}")).unwrap();
        }

        let removed = store.truncate_before(3).unwrap();
        assert_eq!(removed, 2); // entries 1, 2 removed

        assert_eq!(store.get::<String>(1).unwrap(), None);
        assert_eq!(store.get::<String>(2).unwrap(), None);
        assert_eq!(store.get::<String>(3).unwrap(), Some("entry-3".to_string()));
    }

    #[test]
    fn metadata_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbLogStore::open(&dir.path().join("test.redb")).unwrap();

        store.set_meta("vote", &42u64).unwrap();
        store.set_meta("term", &7u64).unwrap();

        let vote: Option<u64> = store.get_meta("vote").unwrap();
        assert_eq!(vote, Some(42));

        let term: Option<u64> = store.get_meta("term").unwrap();
        assert_eq!(term, Some(7));

        let missing: Option<u64> = store.get_meta("nope").unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist.redb");

        // Write data.
        {
            let store = RedbLogStore::open(&path).unwrap();
            store.append(1, &"persisted").unwrap();
            store.set_meta("committed", &1u64).unwrap();
        }

        // Reopen and read.
        {
            let store = RedbLogStore::open(&path).unwrap();
            let v: Option<String> = store.get(1).unwrap();
            assert_eq!(v, Some("persisted".to_string()));

            let committed: Option<u64> = store.get_meta("committed").unwrap();
            assert_eq!(committed, Some(1));
        }
    }

    #[test]
    fn len_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbLogStore::open(&dir.path().join("test.redb")).unwrap();

        assert_eq!(store.len().unwrap(), 0);
        store.append(1, &"a").unwrap();
        store.append(2, &"b").unwrap();
        assert_eq!(store.len().unwrap(), 2);
    }
}
