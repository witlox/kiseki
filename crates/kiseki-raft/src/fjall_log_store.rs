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
//! - `raft_log`  — `u64.to_be_bytes()` (log index) → `[1B version][postcard bytes]`
//! - `raft_meta` — UTF-8 key (`vote`, `committed`, `last_purged`, …)
//!   → `[1B version][postcard bytes]`
//!
//! Big-endian u64 keys give the LSM range iterator the same monotonic
//! traversal order redb's native `u64` ordering used to provide.
//!
//! ## Encoding
//!
//! `postcard` for both entry payloads and meta values. The native PUT
//! flame (rev-4) showed `serde_json::ser::to_vec` at 11.85% of the
//! server's on-CPU samples, all coming through this path. Postcard is
//! 5–10× faster on serde-derived types and same backend the
//! composition + chunk meta encodings use — single source of
//! truth across the workspace.
//!
//! Records carry a 1-byte schema-version prefix so a future
//! incompatible change is fail-closed (binary too old). Same shape
//! as `kiseki-composition::persistent::encoding` and
//! `kiseki-chunk::persistent::encoding`.
//!
//! Pre-1.0 wire format change — no on-disk migration tool ships;
//! operators wipe + re-replicate from peers.

use std::io;
use std::path::Path;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch, PersistMode};
use serde::{de::DeserializeOwned, Serialize};

const KS_LOG: &str = "raft_log";
const KS_META: &str = "raft_meta";

/// Wire-format schema version for both `raft_log` and `raft_meta`
/// records. Bumped on any incompatible change. Records carrying
/// `version > supported` fail open with [`io::Error`] (kind
/// `InvalidData`) so an operator running an older binary against a
/// newer data dir gets a clear surface instead of silent corruption.
const RECORD_SCHEMA_VERSION: u8 = 1;

/// `[1 byte: version][postcard bytes]` — same shape as the
/// composition + chunk meta encodings.
fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(64);
    out.push(RECORD_SCHEMA_VERSION);
    let payload = postcard::to_stdvec(value).map_err(io_err)?;
    out.extend_from_slice(&payload);
    Ok(out)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    let Some((&version, payload)) = bytes.split_first() else {
        return Err(invalid_data("empty raft record"));
    };
    if version > RECORD_SCHEMA_VERSION {
        return Err(invalid_data(format!(
            "raft record schema too new: found={version} supported={RECORD_SCHEMA_VERSION}"
        )));
    }
    postcard::from_bytes(payload).map_err(invalid_data)
}

/// Fjall-backed Raft log store.
///
/// Thread-safe via fjall's internal locking — `Database` and
/// `Keyspace` are `Clone + Send + Sync`. The exposed `&self`
/// methods can be called concurrently; fjall serializes writers on
/// the journal mutex internally.
///
/// Durability mode (`sync_per_write`):
/// * `true` (default): every `append` / `truncate_*` commits with
///   `PersistMode::SyncAll` — strict per-entry durability.
/// * `false`: writes commit with `PersistMode::Buffer` (memtable +
///   in-memory WAL append). A periodic flusher (driven by the
///   server runtime via [`Self::flush`]) drives the durability
///   barrier at a bounded cadence. Same contract as the composition
///   store's eventual-durability mode — multi-node deployments
///   recover the loss window via Raft replication on restart.
#[derive(Clone)]
pub struct FjallLogStore {
    db: Database,
    log_ks: Keyspace,
    meta_ks: Keyspace,
    /// When `false`, writes use `PersistMode::Buffer` instead of
    /// `SyncAll`. Toggle via [`Self::with_eventual_durability`].
    sync_per_write: bool,
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
            sync_per_write: true,
        })
    }

    /// Switch the store between `SyncAll` (default) and `Buffer`
    /// durability for `append` / `truncate_*` writes. When `eventual`
    /// is `true`, the caller is responsible for periodic
    /// [`Self::flush`] calls (and any explicit `fsync(2)` paths).
    /// Returns `self` for builder chaining.
    #[must_use]
    pub fn with_eventual_durability(mut self, eventual: bool) -> Self {
        self.sync_per_write = !eventual;
        self
    }

    /// Append a single log entry. Each call commits inline with
    /// `PersistMode::SyncAll`; openraft's batching is at the
    /// `append` trait level (one batch per replication payload),
    /// not per-entry, so the per-entry fsync cost only hits when the
    /// caller is appending one-by-one (test paths, `save_vote`).
    pub fn append<T: Serialize>(&self, index: u64, entry: &T) -> io::Result<()> {
        let bytes = encode(entry)?;
        let mut batch = self.commit_batch();
        batch.insert(&self.log_ks, index.to_be_bytes().to_vec(), bytes);
        batch.commit().map_err(io_err)
    }

    /// Read a log entry by index.
    pub fn get<T: DeserializeOwned>(&self, index: u64) -> io::Result<Option<T>> {
        match self.log_ks.get(index.to_be_bytes()).map_err(io_err)? {
            Some(slice) => {
                let val: T = decode(slice.as_ref())?;
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
            let val: T = decode(v.as_ref())?;
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
        let bytes = encode(value)?;
        let mut batch = self.commit_batch();
        batch.insert(&self.meta_ks, key.as_bytes().to_vec(), bytes);
        batch.commit().map_err(io_err)
    }

    /// Read a metadata value.
    pub fn get_meta<T: DeserializeOwned>(&self, key: &str) -> io::Result<Option<T>> {
        match self.meta_ks.get(key.as_bytes()).map_err(io_err)? {
            Some(slice) => {
                let val: T = decode(slice.as_ref())?;
                Ok(Some(val))
            }
            None => Ok(None),
        }
    }

    /// Presence check on a metadata key without decoding the value.
    /// Used by [`crate::FjallRaftLogStore::has_state`] to probe for a
    /// stored `vote` without needing to know the openraft `VoteOf<C>`
    /// concrete type. Pre-rev-4 the same probe used
    /// `get_meta::<serde_json::Value>` because JSON would parse any
    /// shape; postcard refuses to decode without a typed target,
    /// hence this presence helper.
    pub fn meta_exists(&self, key: &str) -> io::Result<bool> {
        Ok(self.meta_ks.get(key.as_bytes()).map_err(io_err)?.is_some())
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
        self.log_ks.is_empty().map_err(io_err)
    }

    /// Force a WAL fsync. Not normally needed — every public mutator
    /// already commits with `PersistMode::SyncAll`. Exposed so callers
    /// that batch external writes (e.g. openraft's multi-entry
    /// `append`) can drive a single fsync after building the batch.
    pub fn flush(&self) -> io::Result<()> {
        self.db.persist(PersistMode::SyncAll).map_err(io_err)
    }

    /// Build a fresh batch with the configured durability:
    /// `PersistMode::SyncAll` (default, fsync per write) or
    /// `PersistMode::Buffer` (eventual, periodic flusher drives
    /// durability).
    fn commit_batch(&self) -> OwnedWriteBatch {
        let mode = if self.sync_per_write {
            PersistMode::SyncAll
        } else {
            PersistMode::Buffer
        };
        self.db.batch().durability(Some(mode))
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
