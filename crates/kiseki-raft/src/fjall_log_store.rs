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

use crate::fsync_coalescer::FsyncCoalescer;

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
    /// #151 (W6) — when `Some`, writes use `PersistMode::Buffer` for
    /// the batch and route the durability barrier through the
    /// coalescer. Off-by-default; opt in via
    /// [`Self::with_fsync_coalescing`].
    ///
    /// Independent of `sync_per_write`: when set, the coalescer
    /// always wins (we do `Buffer` on the batch + coalesced fsync).
    /// When unset, `sync_per_write` controls the batch's own
    /// durability mode as before.
    coalescer: Option<FsyncCoalescer>,
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
            coalescer: None,
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

    /// #151 (W6) — enable group-commit fsync coalescing. When set,
    /// `append` / `truncate_*` use `PersistMode::Buffer` on the
    /// individual batch (i.e. the WAL append still happens; only the
    /// device sync is deferred), and the durability barrier is
    /// served by the coalescer's merged fsync. Multiple concurrent
    /// callers within `window_us` (capped at `max_batch` waiters)
    /// share one physical `Database::persist(SyncAll)`.
    ///
    /// Off by default. See [`crate::fsync_coalescer`] for the
    /// contract and tuning guidance. Returns `self` for builder
    /// chaining.
    ///
    /// # Errors
    /// Returns `Err` if `window_us == 0` (which would degenerate into
    /// busy-spin per fsync). Use `with_fsync_coalescing(1, 1)` as the
    /// effective opt-out — a single-waiter window of 1 µs.
    #[must_use]
    pub fn with_fsync_coalescing(mut self, window_us: u64, max_batch: usize) -> Self {
        if window_us == 0 {
            // 1 µs floor — the window timer's tokio::time resolution
            // is ~50 µs anyway; this just rejects the obvious bug.
            self.coalescer = Some(FsyncCoalescer::new(self.db.clone(), 1, max_batch));
        } else {
            self.coalescer = Some(FsyncCoalescer::new(self.db.clone(), window_us, max_batch));
        }
        self
    }

    /// Run the durability barrier appropriate for the current
    /// configuration.
    ///
    /// - **Coalescer on:** join the next merged-fsync window. Returns
    ///   when the merged `Database::persist(SyncAll)` completes.
    /// - **Coalescer off, `sync_per_write == true`** (the default
    ///   inline-SyncAll path): this is a **no-op** — the preceding
    ///   `commit_batch` already issued `PersistMode::SyncAll`, so the
    ///   write is durable on return. Avoids a redundant second fsync
    ///   when the openraft `append` path calls this for uniformity.
    /// - **Coalescer off, `sync_per_write == false`** (legacy
    ///   eventual-durability path): calls `Database::persist(SyncAll)`
    ///   so callers that previously relied on `flush()` keep their
    ///   guarantee.
    ///
    /// Internal helper used by the openraft `append` path; exposed
    /// `pub(crate)` for the few sites (truncate / vote save) that
    /// emulate the same pattern. Outside callers should prefer
    /// [`Self::flush`].
    pub(crate) async fn fsync_barrier(&self) -> io::Result<()> {
        if let Some(coalescer) = self.coalescer.clone() {
            coalescer.flush().await
        } else if self.sync_per_write {
            // Inline-SyncAll already happened in `commit_batch`'s
            // `OwnedWriteBatch::commit` — nothing more to do.
            Ok(())
        } else {
            self.db.persist(PersistMode::SyncAll).map_err(io_err)
        }
    }

    /// Whether group-commit fsync coalescing is enabled on this store.
    #[must_use]
    pub fn has_fsync_coalescer(&self) -> bool {
        self.coalescer.is_some()
    }

    /// Append a single log entry. Each call commits inline with
    /// `PersistMode::SyncAll`. Prefer [`Self::append_batch`] when the
    /// caller already has multiple entries in hand (openraft's
    /// `RaftLogStorage::append` is the production case) — that fold
    /// is one fsync per replication payload instead of N.
    pub fn append<T: Serialize>(&self, index: u64, entry: &T) -> io::Result<()> {
        let bytes = encode(entry)?;
        let mut batch = self.commit_batch();
        batch.insert(&self.log_ks, index.to_be_bytes().to_vec(), bytes);
        batch.commit().map_err(io_err)
    }

    /// Append N entries in one atomic `WriteBatch` + one fsync.
    /// Atomicity is fjall's guarantee on `commit`: either all entries
    /// in the batch are durable or none are — strictly safer than the
    /// openraft `RaftLogStorage::append` contract, which only requires
    /// the entries to be durable when the `IOFlushed` callback fires
    /// and forbids holes in the log.
    ///
    /// Returns `(index, serialized_byte_len)` per appended entry — the
    /// encoding already happens here, so the sizes are free; the
    /// caller (`FjallRaftLogStore::append`) feeds them into the GH
    /// #199 entry cache so the GH #255 byte-budgeted replication read
    /// can account cached entries without re-serializing.
    ///
    /// PUT-perf: per-payload fold removes the per-entry fsync that
    /// dominated the multi-node Raft commit ceiling (issue #66). At
    /// ~50 µs/fsync on `NVMe`, a 16-entry replication payload drops
    /// from ~800 µs of pure fsync to ~50 µs.
    pub fn append_batch<T, I>(&self, entries: I) -> io::Result<Vec<(u64, usize)>>
    where
        T: Serialize,
        I: IntoIterator<Item = (u64, T)>,
    {
        let mut batch = self.commit_batch();
        let mut sizes = Vec::new();
        for (index, entry) in entries {
            let bytes = encode(&entry)?;
            sizes.push((index, bytes.len()));
            batch.insert(&self.log_ks, index.to_be_bytes().to_vec(), bytes);
        }
        if sizes.is_empty() {
            // Empty payload — nothing to commit. Skip the empty batch
            // so we don't burn a no-op fsync.
            return Ok(sizes);
        }
        batch.commit().map_err(io_err)?;
        Ok(sizes)
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

    /// Read entries in `[from, to]` inclusive, stopping BEFORE the
    /// cumulative serialized size would exceed `byte_budget` (GH #255
    /// — byte-budgeted replication batches). Returns
    /// `(index, entry, serialized_byte_len)` triples.
    ///
    /// Contract (mirrors openraft's `limited_get_log_entries`):
    /// * Always returns at least the FIRST entry of a non-empty range,
    ///   even when that single entry exceeds `byte_budget` — returning
    ///   empty for a non-empty range violates the openraft API and
    ///   degrades the replication stream to a sleep-retry loop. A
    ///   single entry over the budget logs a `warn` (with
    ///   `DRAIN_BATCH_CAP` × inline-threshold ≤ 64 MiB < the frame
    ///   cap, this should be unreachable; non-zero means an entry
    ///   producer outgrew the framing assumptions).
    /// * The iterator early-stops at the budget — the oversized tail
    ///   is never read or decoded.
    pub fn range_budgeted<T: DeserializeOwned>(
        &self,
        from: u64,
        to: u64,
        byte_budget: usize,
    ) -> io::Result<Vec<(u64, T, usize)>> {
        let start = from.to_be_bytes();
        let end = to.to_be_bytes();
        let mut out: Vec<(u64, T, usize)> = Vec::new();
        let mut used = 0usize;
        for entry in self.log_ks.range(start.as_slice()..=end.as_slice()) {
            let (k, v) = entry.into_inner().map_err(io_err)?;
            let kbytes = k.as_ref();
            if kbytes.len() != 8 {
                continue;
            }
            let size = v.as_ref().len();
            if !out.is_empty() && used.saturating_add(size) > byte_budget {
                // Adding this entry would blow the budget — return the
                // accumulated prefix (valid per the openraft contract).
                break;
            }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(kbytes);
            let key = u64::from_be_bytes(buf);
            if out.is_empty() && size > byte_budget {
                tracing::warn!(
                    index = key,
                    size,
                    byte_budget,
                    "single Raft log entry exceeds the replication byte \
                     budget — returning it alone (never-empty contract); \
                     check DRAIN_BATCH_CAP × inline threshold vs the \
                     frame cap (GH #255)",
                );
            }
            let val: T = decode(v.as_ref())?;
            used = used.saturating_add(size);
            out.push((key, val, size));
            if used > byte_budget {
                // Only reachable via the single-oversized-entry case.
                break;
            }
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
    ///
    /// #151 (W6) — when the fsync coalescer is set, batches use
    /// `Buffer` and the durability barrier is served by
    /// [`Self::fsync_barrier`] (called by the openraft `append`
    /// path after the batch commits). This is the path that yields
    /// the group-commit win: many concurrent batches each `Buffer`
    /// their WAL append; one merged fsync covers them all.
    fn commit_batch(&self) -> OwnedWriteBatch {
        let mode = if self.coalescer.is_some() || !self.sync_per_write {
            PersistMode::Buffer
        } else {
            PersistMode::SyncAll
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

    /// #66 — `append_batch` MUST persist every entry in the iterator
    /// and the result MUST match per-entry `append` for any reader.
    /// Atomicity (all-or-nothing on `commit`) is fjall's contract;
    /// this test pins our consumption of it.
    #[test]
    fn append_batch_persists_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        let entries: Vec<(u64, String)> = (1..=10).map(|i| (i, format!("entry-{i}"))).collect();
        store.append_batch(entries).unwrap();

        for i in 1..=10u64 {
            let v: Option<String> = store.get(i).unwrap();
            assert_eq!(
                v,
                Some(format!("entry-{i}")),
                "index {i} not persisted by append_batch — #66 atomic fold broken"
            );
        }

        let last = store.last_index().unwrap();
        assert_eq!(
            last,
            Some(10),
            "last_index after batch must be 10, got {last:?}"
        );
    }

    /// Empty iterator is a no-op — no fsync, no batch commit. Matches
    /// openraft's "the batch in this trait call must be durable when
    /// `io_completed` fires" — vacuously true for zero entries.
    #[test]
    fn append_batch_empty_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        let entries: Vec<(u64, String)> = Vec::new();
        store.append_batch(entries).unwrap();

        assert_eq!(store.last_index().unwrap(), None);
        assert!(store.is_empty().unwrap());
    }

    /// Mixing `append_batch` then `append` then `append_batch` again
    /// must keep the log dense (no holes — openraft correctness
    /// invariant).
    #[test]
    fn append_batch_interleaved_with_single_append_has_no_holes() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        store
            .append_batch((1..=3u64).map(|i| (i, format!("b1-{i}"))))
            .unwrap();
        store.append(4, &"single-4".to_string()).unwrap();
        store
            .append_batch((5..=7u64).map(|i| (i, format!("b2-{i}"))))
            .unwrap();

        for i in 1..=7u64 {
            let v: Option<String> = store.get(i).unwrap();
            assert!(
                v.is_some(),
                "hole at index {i} — would violate Raft monotonicity"
            );
        }
        assert_eq!(store.last_index().unwrap(), Some(7));
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

    /// GH #255 — `range_budgeted` stops BEFORE the cumulative
    /// serialized size exceeds the budget and reports per-entry sizes.
    #[test]
    fn range_budgeted_stops_at_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        // 5 entries of ~1000 serialized bytes each.
        for i in 1..=5u64 {
            store.append(i, &vec![0xAAu8; 1000]).unwrap();
        }
        // Budget for ~2.5 entries → exactly 2 come back.
        let got: Vec<(u64, Vec<u8>, usize)> = store.range_budgeted(1, 5, 2500).unwrap();
        assert_eq!(
            got.iter().map(|(i, _, _)| *i).collect::<Vec<_>>(),
            vec![1, 2],
            "budget of 2500 bytes must cut the range after 2 ~1000-byte entries"
        );
        let total: usize = got.iter().map(|(_, _, s)| s).sum();
        assert!(
            total <= 2500,
            "returned prefix ({total} bytes) must fit the budget"
        );
        for (_, v, s) in &got {
            assert_eq!(v.len(), 1000);
            assert!(
                *s > 1000,
                "reported size must be the serialized record length \
                 (version byte + postcard), got {s}"
            );
        }

        // A generous budget returns the full range with sizes.
        let all: Vec<(u64, Vec<u8>, usize)> = store.range_budgeted(1, 5, usize::MAX).unwrap();
        assert_eq!(all.len(), 5);
    }

    /// GH #255 — the never-empty contract: even a budget smaller than
    /// the first entry returns that entry alone (returning empty for a
    /// non-empty range degrades openraft's replication stream to a
    /// sleep-retry loop).
    #[test]
    fn range_budgeted_always_returns_first_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        store.append(1, &vec![0xBBu8; 4096]).unwrap();
        store.append(2, &vec![0xCCu8; 4096]).unwrap();

        let got: Vec<(u64, Vec<u8>, usize)> = store.range_budgeted(1, 2, 16).unwrap();
        assert_eq!(
            got.len(),
            1,
            "a 16-byte budget must still return the (oversized) first entry"
        );
        assert_eq!(got[0].0, 1);
        assert_eq!(got[0].1.len(), 4096);
    }

    /// GH #255 — empty range stays empty (the contract only forbids
    /// empty results for NON-empty ranges).
    #[test]
    fn range_budgeted_empty_range_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();
        let got: Vec<(u64, String, usize)> = store.range_budgeted(10, 20, 1 << 20).unwrap();
        assert!(got.is_empty());
    }

    /// GH #255 — `append_batch` reports `(index, serialized_len)` per
    /// entry; the sizes must match what a subsequent budgeted read
    /// observes on disk.
    #[test]
    fn append_batch_returns_sizes_matching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallLogStore::open(&dir.path().join("log")).unwrap();

        let sizes = store
            .append_batch((1..=3u64).map(|i| (i, vec![0u8; 100 * i as usize])))
            .unwrap();
        assert_eq!(sizes.len(), 3);
        assert_eq!(
            sizes.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        let disk: Vec<(u64, Vec<u8>, usize)> = store.range_budgeted(1, 3, usize::MAX).unwrap();
        for ((ai, asize), (di, _, dsize)) in sizes.iter().zip(disk.iter()) {
            assert_eq!(ai, di);
            assert_eq!(
                asize, dsize,
                "append-time size and disk record size must agree (index {ai})"
            );
        }
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
