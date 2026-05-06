//! Fjall-backed Raft log store — persistent, ACID, pure Rust.
//!
//! Stores Raft log entries, vote state, committed index, and
//! `last_purged` in a single fjall database. Crash-safe via WAL +
//! LSM memtable; writes commit with `PersistMode::SyncAll` so
//! openraft's `IOFlushed` callback signals true durability (RFC
//! Raft §5.4).
//!
//! ADR-022 rev-2 successor on the Raft log path: redb's per-write
//! COW B-tree commit cost dominated CPU at the rates the
//! multi-shard fabric drives; fjall's LSM batches the WAL append +
//! memtable insert into one journal write per `WriteBatch`.
//!
//! ## Schema
//!
//! Two keyspaces inside one fjall database:
//!
//! - `raft_log`  — `u64.to_be_bytes()` (log index) → JSON-encoded entry
//! - `raft_meta` — UTF-8 key (`vote`, `committed`, `last_purged`, …)
//!                 → JSON-encoded value
//!
//! Big-endian u64 keys give the LSM range iterator the same monotonic
//! traversal order redb's native `u64` ordering used to provide.
//!
//! ## Encoding
//!
//! `serde_json` for both entry payloads and meta values, matching the
//! redb impl byte-for-byte at the value layer. The Raft log is
//! cluster-internal; no operator inspects it directly, so format
//! efficiency is not load-bearing — keep parity with the redb path
//! so a `git revert` is mechanical.

use std::io;
use std::path::Path;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch, PersistMode};
use serde::{de::DeserializeOwned, Serialize};

const KS_LOG: &str = "raft_log";
const KS_META: &str = "raft_meta";

/// Fjall-backed Raft log store.
///
/// Thread-safe via fjall's internal locking — `Database` and
/// `Keyspace` are `Clone + Send + Sync`. The exposed `&self`
/// methods can be called concurrently; fjall serializes writers on
/// the journal mutex internally.
pub struct FjallLogStore {
    db: Database,
    log_ks: Keyspace,
    meta_ks: Keyspace,
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

fn invalid_data<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

impl FjallLogStore {
    /// Open or create a fjall database at `path`. The path is a
    /// directory (fjall's keyspace layout) — callers that previously
    /// passed a `*.redb` file path should pass the parent directory
    /// or a sibling directory name with no extension.
    pub fn open(path: &Path) -> io::Result<Self> {
        let db = Database::builder(path).open().map_err(io_err)?;
        let log_ks = db
            .keyspace(KS_LOG, KeyspaceCreateOptions::default)
            .map_err(io_err)?;
        let meta_ks = db
            .keyspace(KS_META, KeyspaceCreateOptions::default)
            .map_err(io_err)?;
        Ok(Self {
            db,
            log_ks,
            meta_ks,
        })
    }

    /// Append a single log entry. Each call commits inline with
    /// `PersistMode::SyncAll`; openraft's batching is at the
    /// `append` trait level (one batch per replication payload),
    /// not per-entry, so the per-entry fsync cost only hits when the
    /// caller is appending one-by-one (test paths, `save_vote`).
    pub fn append<T: Serialize>(&self, index: u64, entry: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(entry).map_err(io_err)?;
        let mut batch = self.commit_batch();
        batch.insert(&self.log_ks, index.to_be_bytes().to_vec(), bytes);
        batch.commit().map_err(io_err)
    }

    /// Read a log entry by index.
    pub fn get<T: DeserializeOwned>(&self, index: u64) -> io::Result<Option<T>> {
        match self.log_ks.get(index.to_be_bytes()).map_err(io_err)? {
            Some(slice) => {
                let val: T = serde_json::from_slice(slice.as_ref()).map_err(invalid_data)?;
                Ok(Some(val))
            }
            None => Ok(None),
        }
    }

    /// Read all entries in range [from, to] inclusive.
    pub fn range<T: DeserializeOwned>(&self, from: u64, to: u64) -> io::Result<Vec<(u64, T)>> {
        let start = from.to_be_bytes();
        let end = to.to_be_bytes();
        let mut out = Vec::new();
        for entry in self.log_ks.range(start.as_slice()..=end.as_slice()) {
            let (k, v) = entry.into_inner().map_err(io_err)?;
            let kbytes = k.as_ref();
            if kbytes.len() != 8 {
                continue;
            }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(kbytes);
            let key = u64::from_be_bytes(buf);
            let val: T = serde_json::from_slice(v.as_ref()).map_err(invalid_data)?;
            out.push((key, val));
        }
        Ok(out)
    }

    /// Remove all entries with `index < before`. Returns the count
    /// removed. Used by openraft's `purge` (snapshot truncation).
    pub fn truncate_before(&self, before: u64) -> io::Result<u64> {
        let end = before.to_be_bytes();
        // Collect keys first so we don't mutate while iterating.
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for entry in self.log_ks.range(..end.as_slice()) {
            let (k, _v) = entry.into_inner().map_err(io_err)?;
            keys.push(k.to_vec());
        }
        let count = keys.len() as u64;
        if count == 0 {
            return Ok(0);
        }
        let mut batch = self.commit_batch();
        for k in keys {
            batch.remove(&self.log_ks, k);
        }
        batch.commit().map_err(io_err)?;
        Ok(count)
    }

    /// Remove all entries with `index > after`. Returns the count
    /// removed. Used when a leader overwrites a follower's
    /// conflicting log suffix.
    pub fn truncate_after(&self, after: u64) -> io::Result<u64> {
        // [(after+1)..=u64::MAX]
        let start = after.saturating_add(1).to_be_bytes();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for entry in self.log_ks.range(start.as_slice()..) {
            let (k, _v) = entry.into_inner().map_err(io_err)?;
            keys.push(k.to_vec());
        }
        let count = keys.len() as u64;
        if count == 0 {
            return Ok(0);
        }
        let mut batch = self.commit_batch();
        for k in keys {
            batch.remove(&self.log_ks, k);
        }
        batch.commit().map_err(io_err)?;
        Ok(count)
    }

    /// Store a metadata value (e.g., vote, committed index,
    /// `last_purged`). Always commits with `PersistMode::SyncAll`
    /// — `save_vote` correctness depends on durability before the
    /// callback returns (Raft §5.4).
    pub fn set_meta<T: Serialize>(&self, key: &str, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io_err)?;
        let mut batch = self.commit_batch();
        batch.insert(&self.meta_ks, key.as_bytes().to_vec(), bytes);
        batch.commit().map_err(io_err)
    }

    /// Read a metadata value.
    pub fn get_meta<T: DeserializeOwned>(&self, key: &str) -> io::Result<Option<T>> {
        match self.meta_ks.get(key.as_bytes()).map_err(io_err)? {
            Some(slice) => {
                let val: T = serde_json::from_slice(slice.as_ref()).map_err(invalid_data)?;
                Ok(Some(val))
            }
            None => Ok(None),
        }
    }

    /// Highest log index, or `None` if the log is empty.
    pub fn last_index(&self) -> io::Result<Option<u64>> {
        match self.log_ks.last_key_value() {
            Some(guard) => {
                let (k, _v) = guard.into_inner().map_err(io_err)?;
                let kbytes = k.as_ref();
                if kbytes.len() != 8 {
                    return Ok(None);
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(kbytes);
                Ok(Some(u64::from_be_bytes(buf)))
            }
            None => Ok(None),
        }
    }

    /// Number of log entries currently stored.
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.log_ks.len().map_err(io_err)? as u64)
    }

    /// Empty if no log entries are stored.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.log_ks.is_empty().map_err(io_err)?)
    }

    /// Force a WAL fsync. Not normally needed — every public mutator
    /// already commits with `PersistMode::SyncAll`. Exposed so callers
    /// that batch external writes (e.g. openraft's multi-entry
    /// `append`) can drive a single fsync after building the batch.
    pub fn flush(&self) -> io::Result<()> {
        self.db.persist(PersistMode::SyncAll).map_err(io_err)
    }

    /// Build a fresh batch with `PersistMode::SyncAll` durability set.
    fn commit_batch(&self) -> OwnedWriteBatch {
        self.db.batch().durability(Some(PersistMode::SyncAll))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

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
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        for i in 1..=5 {
            store.append(i, &format!("entry-{i}")).unwrap();
        }

        let range: Vec<(u64, String)> = store.range(2, 4).unwrap();
        assert_eq!(range.len(), 3);
        assert_eq!(range[0], (2, "entry-2".to_string()));
        assert_eq!(range[2], (4, "entry-4".to_string()));
    }

    #[test]
    fn truncate_before_keeps_tail() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        for i in 1..=5 {
            store.append(i, &format!("entry-{i}")).unwrap();
        }

        let removed = store.truncate_before(3).unwrap();
        assert_eq!(removed, 2); // 1, 2 removed
        assert_eq!(store.get::<String>(1).unwrap(), None);
        assert_eq!(store.get::<String>(2).unwrap(), None);
        assert_eq!(store.get::<String>(3).unwrap(), Some("entry-3".to_string()));
    }

    #[test]
    fn truncate_after_keeps_head() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        for i in 1..=5 {
            store.append(i, &format!("entry-{i}")).unwrap();
        }

        let removed = store.truncate_after(2).unwrap();
        assert_eq!(removed, 3); // 3, 4, 5 removed
        assert_eq!(store.get::<String>(2).unwrap(), Some("entry-2".to_string()));
        assert_eq!(store.get::<String>(3).unwrap(), None);
    }

    #[test]
    fn metadata_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        store.set_meta("vote", &42u64).unwrap();
        store.set_meta("term", &7u64).unwrap();

        assert_eq!(store.get_meta::<u64>("vote").unwrap(), Some(42));
        assert_eq!(store.get_meta::<u64>("term").unwrap(), Some(7));
        assert_eq!(store.get_meta::<u64>("nope").unwrap(), None);
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist");

        {
            let store = FjallLogStore::open(&path).unwrap();
            store.append(1, &"persisted").unwrap();
            store.set_meta("committed", &1u64).unwrap();
        }
        {
            let store = FjallLogStore::open(&path).unwrap();
            assert_eq!(
                store.get::<String>(1).unwrap(),
                Some("persisted".to_string())
            );
            assert_eq!(store.get_meta::<u64>("committed").unwrap(), Some(1));
        }
    }

    #[test]
    fn last_index_and_len() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        assert_eq!(store.len().unwrap(), 0);
        assert_eq!(store.last_index().unwrap(), None);

        store.append(1, &"a").unwrap();
        store.append(7, &"g").unwrap();
        store.append(3, &"c").unwrap();

        assert_eq!(store.len().unwrap(), 3);
        assert_eq!(store.last_index().unwrap(), Some(7));
    }
}
