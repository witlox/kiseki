//! Persistent Raft log store backed by fjall.
//!
//! Wraps [`FjallLogStore`] and implements openraft's `RaftLogStorage`
//! + `RaftLogReader` traits. Raft state (log entries, vote, committed
//!   index, last purged) survives server restart.
//!
//! ADR-022 rev-2 successor: replaces the previous `RedbRaftLogStore`
//! 2026-05-06. Wire / serde-json format unchanged so a `git revert`
//! to the redb impl doesn't require re-encoding entries.

use std::fmt::Debug;
use std::io;
use std::num::NonZeroUsize;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use openraft::alias::{LogIdOf, VoteOf};
use openraft::entry::RaftEntry;
use openraft::storage::{IOFlushed, RaftLogReader, RaftLogStorage};
use openraft::{LogState, RaftTypeConfig};
use serde::{de::DeserializeOwned, Serialize};

use crate::fjall_log_store::FjallLogStore;

/// Default LRU capacity for the in-memory log entry cache (GH #199).
///
/// Sized so that under W12-batched intent fan (~100 intents per
/// `IncorporateIntents` Raft entry) at the target 300 ops/s, the
/// last ~10 seconds of entries stay resident — well beyond the
/// typical apply lag.
const DEFAULT_ENTRY_CACHE_CAP: usize = 4096;

/// Persistent Raft log store backed by fjall.
///
/// Stores log entries in the `raft_log` keyspace and metadata
/// (`vote`, `committed`, `last_purged`) in the `raft_meta` keyspace.
/// Thread-safe via `Arc` — `Clone` shares the underlying database.
///
/// ## GH #199: in-memory entry LRU
///
/// `entry_cache` holds the most-recent N (default 4096) entries as
/// already-deserialized `(Arc<C::Entry>, serialized_byte_len)` pairs,
/// populated on `append` (the leader has the typed entry in hand
/// before we ever serialize it; `append_batch` reports the encoded
/// size for free). Reads on the apply / replication paths look up the
/// cache first, falling back to fjall + postcard decode only on miss.
/// The cache is invalidated on `truncate_after` / `purge`.
///
/// The serialized size rides along so the GH #255 byte-budgeted
/// replication read (`limited_get_log_entries`) can account cached
/// entries against [`crate::tcp_transport::replication_byte_budget`]
/// without re-serializing them.
///
/// The values are wrapped in `Arc` so cache populate (`append` and
/// `try_get_log_entries` miss-fill) is a refcount bump, not a deep
/// copy of the ~4 KiB inline-payload-carrying entry. The read path
/// still has to materialize a fresh `C::Entry` for openraft's
/// `Vec<C::Entry>` return — one deep clone per returned entry —
/// but that's unavoidable as long as openraft's API takes entries
/// by value.
///
/// The cache is shared across clones via `Arc<Mutex<_>>`. The mutex
/// is held for a single hash lookup per get-or-insert; contention is
/// bounded.
#[derive(Clone)]
pub struct FjallRaftLogStore<C: RaftTypeConfig> {
    inner: Arc<FjallLogStore>,
    entry_cache: Arc<Mutex<SizedEntryCache<C>>>,
    _phantom: std::marker::PhantomData<C>,
}

/// GH #199 entry LRU value shape: the deserialized entry plus its
/// serialized record length (GH #255 byte-budget accounting).
type SizedEntryCache<C> = LruCache<u64, (Arc<<C as RaftTypeConfig>::Entry>, usize)>;

impl<C: RaftTypeConfig> FjallRaftLogStore<C> {
    /// Open or create a persistent Raft log store at `path`. The
    /// path is a directory (fjall layout); callers that previously
    /// passed a `*.redb` file path should pass a sibling directory
    /// name with no extension.
    pub fn open(path: &Path) -> io::Result<Self> {
        let inner = FjallLogStore::open(path)?;
        Ok(Self::with_inner(Arc::new(inner)))
    }

    /// #151 (W6) — Open with group-commit fsync coalescing on. See
    /// [`FjallLogStore::with_fsync_coalescing`] for the contract and
    /// tuning. `window_us == 0` is mapped to 1 µs internally.
    pub fn open_with_fsync_coalescing(
        path: &Path,
        window_us: u64,
        max_batch: usize,
    ) -> io::Result<Self> {
        let inner = FjallLogStore::open(path)?.with_fsync_coalescing(window_us, max_batch);
        Ok(Self::with_inner(Arc::new(inner)))
    }

    fn with_inner(inner: Arc<FjallLogStore>) -> Self {
        let cap = NonZeroUsize::new(DEFAULT_ENTRY_CACHE_CAP)
            .unwrap_or_else(|| NonZeroUsize::new(1).expect("1 is non-zero"));
        Self {
            inner,
            entry_cache: Arc::new(Mutex::new(LruCache::new(cap))),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Check whether this store has any persisted state (log entries
    /// or vote). Returns `true` if the store was previously used —
    /// the Raft node should NOT call `initialize()` on restart.
    pub fn has_state(&self) -> bool {
        !self.inner.is_empty().unwrap_or(true) || self.inner.meta_exists("vote").unwrap_or(false)
    }
}

impl<C: RaftTypeConfig> RaftLogReader<C> for FjallRaftLogStore<C>
where
    C::Entry: DeserializeOwned + Clone,
    VoteOf<C>: DeserializeOwned,
{
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug>(
        &mut self,
        range: RB,
    ) -> Result<Vec<C::Entry>, io::Error> {
        let start = match range.start_bound() {
            std::ops::Bound::Included(&s) => s,
            std::ops::Bound::Excluded(&s) => s + 1,
            std::ops::Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            std::ops::Bound::Included(&e) => e,
            std::ops::Bound::Excluded(&e) => e.saturating_sub(1),
            std::ops::Bound::Unbounded => u64::MAX,
        };

        // GH #199: try the deserialized-entry cache first. The
        // common case on the leader is a tight contiguous range
        // (the apply path requests entries by log index right
        // after they were appended); if every index in the range
        // is resident, we never touch fjall + postcard.
        //
        // Cache hit-then-miss in the middle of a range is the
        // tricky case — for simplicity we fall back to the fjall
        // range scan on ANY miss, but populate the cache with the
        // results on the way back so the next adjacent read is
        // fully cached. This trades a small redundancy on miss
        // for a simpler invariant on hit.
        let want = end.saturating_add(1).saturating_sub(start);
        if want > 0 && want <= u64::try_from(DEFAULT_ENTRY_CACHE_CAP).unwrap_or(u64::MAX) {
            // On all-hit we collect `Arc`s under the mutex (cheap
            // refcount bumps) and release the mutex before doing the
            // deep clones for openraft's return type. Keeps mutex
            // hold-time bounded by hash-lookup × N.
            let arcs: Option<Vec<Arc<C::Entry>>> = {
                let mut cache = self
                    .entry_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut out = Vec::with_capacity(usize::try_from(want).unwrap_or(0));
                let mut all_hit = true;
                for i in start..=end {
                    if let Some((arc, _size)) = cache.get(&i) {
                        out.push(Arc::clone(arc));
                    } else {
                        all_hit = false;
                        break;
                    }
                }
                if all_hit {
                    Some(out)
                } else {
                    None
                }
            };
            if let Some(arcs) = arcs {
                return Ok(arcs.iter().map(|a| (**a).clone()).collect());
            }
        }

        // Cache miss (or range too large to bother). Read through
        // fjall + postcard, then populate the cache so the next
        // adjacent read is hot. Building the result list by cloning
        // each entry once before moving the original into the cache
        // avoids the double-clone the original implementation paid.
        // The unbudgeted sized read (`usize::MAX`) keeps the GH #199
        // semantics while carrying each entry's serialized size into
        // the cache for the GH #255 budget accounting.
        let entries: Vec<(u64, C::Entry, usize)> =
            self.inner.range_budgeted(start, end, usize::MAX)?;
        let result: Vec<C::Entry> = entries.iter().map(|(_, e, _)| e.clone()).collect();
        {
            let mut cache = self
                .entry_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (idx, entry, size) in entries {
                cache.put(idx, (Arc::new(entry), size));
            }
        }
        Ok(result)
    }

    /// GH #255 — byte-budgeted replication read. openraft's
    /// replication stream calls this (NOT `try_get_log_entries`) when
    /// building an `AppendEntries` batch, and explicitly tolerates a
    /// PREFIX of `[start, end)`; the contract only forbids returning
    /// empty for a non-empty range. The default impl delegates to
    /// `try_get_log_entries` — unbudgeted, which is exactly how
    /// catch-up batches of 300 fat `IncorporateIntents` entries
    /// reached 178–190 MB > the 128 MiB frame cap and wedged the
    /// 2026-06-11 GCP cluster permanently.
    ///
    /// Budget: [`crate::tcp_transport::replication_byte_budget`]
    /// (default frame cap / 4 = 32 MiB).
    ///
    /// Walks the GH #199 cache from `start` while every index hits and
    /// the budget holds — steady-state replication (hot tail, small
    /// entries) keeps its cache win. On a miss at the FIRST index
    /// (catch-up = cold), falls through to the budgeted fjall read; on
    /// a miss after a non-empty accumulation, returns the accumulated
    /// prefix (valid per the contract — openraft re-requests the rest).
    async fn limited_get_log_entries(
        &mut self,
        start: u64,
        end: u64,
    ) -> Result<Vec<C::Entry>, io::Error> {
        self.limited_get_with_budget(start, end, crate::tcp_transport::replication_byte_budget())
    }

    async fn read_vote(&mut self) -> Result<Option<VoteOf<C>>, io::Error> {
        self.inner.get_meta("vote")
    }
}

impl<C: RaftTypeConfig> FjallRaftLogStore<C>
where
    C::Entry: DeserializeOwned + Clone,
{
    /// [`RaftLogReader::limited_get_log_entries`] body with an
    /// explicit `budget` — the trait method passes the env-backed
    /// [`crate::tcp_transport::replication_byte_budget`]; tests pass
    /// explicit budgets so they don't race on the process-global
    /// `OnceLock`.
    fn limited_get_with_budget(
        &self,
        start: u64,
        end: u64,
        budget: usize,
    ) -> Result<Vec<C::Entry>, io::Error> {
        if start >= end {
            return Ok(Vec::new());
        }

        // Cache walk: collect Arcs under the mutex (refcount bumps),
        // deep-clone for openraft's by-value API after release.
        let (arcs, first_missed) = {
            let mut cache = self
                .entry_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut out: Vec<Arc<C::Entry>> = Vec::new();
            let mut used = 0usize;
            let mut first_missed = false;
            for i in start..end {
                let Some((arc, size)) = cache.get(&i) else {
                    first_missed = out.is_empty();
                    break;
                };
                if !out.is_empty() && used.saturating_add(*size) > budget {
                    break;
                }
                if out.is_empty() && *size > budget {
                    tracing::warn!(
                        index = i,
                        size,
                        budget,
                        "single cached Raft log entry exceeds the replication \
                         byte budget — returning it alone (never-empty \
                         contract, GH #255)",
                    );
                    out.push(Arc::clone(arc));
                    break;
                }
                used = used.saturating_add(*size);
                out.push(Arc::clone(arc));
            }
            (out, first_missed)
        };
        if !arcs.is_empty() && !first_missed {
            return Ok(arcs.iter().map(|a| (**a).clone()).collect());
        }

        // Cold path (first index not cached — the catch-up shape):
        // budgeted read straight from fjall. The early-stop inside
        // `range_budgeted` means the oversized tail is never even
        // decoded. Don't populate the cache here — catch-up ranges
        // are historical and would evict the hot tail.
        let entries: Vec<(u64, C::Entry, usize)> =
            self.inner.range_budgeted(start, end - 1, budget)?;
        Ok(entries.into_iter().map(|(_, e, _)| e).collect())
    }
}

impl<C: RaftTypeConfig> RaftLogStorage<C> for FjallRaftLogStore<C>
where
    C::Entry: Serialize + DeserializeOwned + Clone,
    VoteOf<C>: Serialize + DeserializeOwned,
    LogIdOf<C>: Serialize + DeserializeOwned,
{
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<C>, io::Error> {
        let last_purged: Option<LogIdOf<C>> = self.inner.get_meta("last_purged")?;
        let last_index = self.inner.last_index()?;
        let last_log_id = if let Some(idx) = last_index {
            let entry: Option<C::Entry> = self.inner.get(idx)?;
            entry.map(|e| e.log_id())
        } else {
            last_purged.clone()
        };
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id,
        })
    }

    async fn save_committed(&mut self, committed: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        self.inner.set_meta("committed", &committed)
    }

    async fn read_committed(&mut self) -> Result<Option<LogIdOf<C>>, io::Error> {
        self.inner
            .get_meta::<Option<LogIdOf<C>>>("committed")
            .map(Option::flatten)
    }

    async fn save_vote(&mut self, vote: &VoteOf<C>) -> Result<(), io::Error> {
        self.inner.set_meta("vote", vote)
    }

    async fn append<I>(&mut self, entries: I, callback: IOFlushed<C>) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = C::Entry>,
    {
        // One fjall `WriteBatch` per openraft replication payload, one
        // fsync. fjall's `commit` is all-or-nothing on the batch, which
        // is strictly safer than the openraft `RaftLogStorage::append`
        // contract (entries durable when `IOFlushed` fires, no holes
        // in the log). Issue #66: pre-fix this loop called
        // `inner.append` per entry, burning N fsyncs per replication
        // payload — the dominant cost in the multi-node PUT ceiling
        // (1% of GET on GCP compact 2026-05-17).
        //
        // GH #199 — collect entries into a `Vec` once so we can both
        // hand them to `append_batch` AND populate the in-memory LRU
        // cache. The leader-side apply path that runs right after
        // this append will hit the cache instead of re-reading +
        // re-decoding from fjall.
        //
        // Cache values are `Arc<C::Entry>`. Population is a single
        // `Arc::new` per entry — no deep clone — so the cost on a
        // 100-entry W12 batch is bounded by allocator pressure
        // rather than ~400 KB of memcpy per batch. The first
        // implementation built an `indexed: Vec<(u64, C::Entry)>`
        // and clone-walked it, which was visible as ~14 % regression
        // on the local 3-node profile run.
        let entries: Vec<C::Entry> = entries.into_iter().collect();
        let sizes = self
            .inner
            .append_batch(entries.iter().map(|e| (e.index(), e)))?;
        debug_assert_eq!(sizes.len(), entries.len());
        {
            let mut cache = self
                .entry_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (entry, (idx, size)) in entries.into_iter().zip(sizes) {
                debug_assert_eq!(entry.index(), idx);
                cache.put(idx, (Arc::new(entry), size));
            }
        }
        // #151 (W6) — when the coalescer is on, `append_batch` only
        // appended to the WAL with `PersistMode::Buffer` (no device
        // sync yet). The durability barrier — and thus the
        // `IOFlushed::io_completed` signal — waits on the
        // coalescer's next merged fsync window. When the coalescer
        // is off, `append_batch` already did the SyncAll inline and
        // `fsync_barrier()` is a redundant-but-cheap second
        // persist. We always go through `fsync_barrier()` so the
        // openraft contract — *the callback fires when entries are
        // durable* — is uniform across both modes.
        self.inner.fsync_barrier().await?;
        callback.io_completed(Ok(()));
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        if let Some(ref log_id) = last_log_id {
            self.inner.truncate_after(log_id.index())?;
        } else {
            // Truncate everything — remove all entries.
            self.inner.truncate_before(u64::MAX)?;
        }
        // GH #199: invalidate the LRU. Truncate-after is rare (only
        // happens on leader change / log conflict) so the brute-force
        // clear is fine — the next reads will repopulate the cache.
        self.entry_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        Ok(())
    }

    async fn purge(&mut self, log_id: LogIdOf<C>) -> Result<(), io::Error> {
        // Remove entries up to and including log_id.index().
        self.inner.truncate_before(log_id.index() + 1)?;
        self.inner.set_meta("last_purged", &log_id)?;
        // GH #199: drop the purged prefix from the LRU. We don't have
        // a range-pop on `lru`, so iterate and remove. Purge is even
        // rarer than truncate_after (driven by snapshot policy, every
        // 1000 entries by default), so the linear scan is fine.
        {
            let mut cache = self
                .entry_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let purged_through = log_id.index();
            let to_drop: Vec<u64> = cache
                .iter()
                .filter_map(|(idx, _)| (*idx <= purged_through).then_some(*idx))
                .collect();
            for idx in to_drop {
                cache.pop(&idx);
            }
        }
        Ok(())
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct TestCmd(String);
    impl std::fmt::Display for TestCmd {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct TestResp;
    impl std::fmt::Display for TestResp {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "ok")
        }
    }

    openraft::declare_raft_types!(
        TestConfig:
            D = TestCmd,
            R = TestResp,
            NodeId = u64,
            Node = crate::node::KisekiNode,
            SnapshotData = std::io::Cursor<Vec<u8>>,
    );

    use openraft::alias::CommittedLeaderIdOf;
    use openraft::vote::RaftLeaderId;
    use openraft::{EntryPayload, LogId};

    type TestEntry = <TestConfig as RaftTypeConfig>::Entry;

    fn lid(index: u64) -> LogIdOf<TestConfig> {
        LogId::new(CommittedLeaderIdOf::<TestConfig>::new(1, 1), index)
    }

    /// A normal entry whose `TestCmd` payload is `payload_len` bytes —
    /// the serialized record is payload + small framing overhead, so
    /// byte-budget arithmetic in the tests can treat `payload_len` as
    /// the entry's approximate size.
    fn fat_entry(index: u64, payload_len: usize) -> TestEntry {
        RaftEntry::new(
            lid(index),
            EntryPayload::Normal(TestCmd("x".repeat(payload_len))),
        )
    }

    fn noop() -> IOFlushed<TestConfig> {
        IOFlushed::<TestConfig>::noop()
    }

    /// One ~1 MiB payload per entry — the production shape that wedged
    /// the 2026-06-11 GCP cluster (committer `IncorporateIntents`
    /// entries embedding inline payloads).
    const MIB: usize = 1024 * 1024;

    /// GH #255 (cold path) — a fresh store instance (empty GH #199
    /// cache, the follower-catch-up shape) must return a budget-bounded
    /// PREFIX from fjall when entries are large, and the full range
    /// when the budget is generous.
    #[tokio::test]
    async fn limited_get_budgets_cold_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budgeted-cold");

        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            let entries: Vec<TestEntry> = (1..=5).map(|i| fat_entry(i, MIB)).collect();
            store.append(entries, noop()).await.unwrap();
        }

        // Reopen → empty entry cache → the fjall `range_budgeted` path.
        let store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();

        // ~2.5 MiB budget cuts 5 × ~1 MiB entries to a 2-entry prefix.
        let got = store
            .limited_get_with_budget(1, 6, 2 * MIB + MIB / 2)
            .unwrap();
        assert_eq!(
            got.len(),
            2,
            "cold budgeted read must return a 2-entry prefix, got {}",
            got.len()
        );
        assert_eq!(got[0].index(), 1);
        assert_eq!(got[1].index(), 2);

        // Generous budget → full range.
        let all = store.limited_get_with_budget(1, 6, usize::MAX).unwrap();
        assert_eq!(all.len(), 5);

        // Budget below the first entry → never-empty contract: exactly
        // the first entry comes back.
        let one = store.limited_get_with_budget(1, 6, 16).unwrap();
        assert_eq!(one.len(), 1, "never-empty contract violated");
        assert_eq!(one[0].index(), 1);

        // Empty range stays empty.
        assert!(store
            .limited_get_with_budget(3, 3, usize::MAX)
            .unwrap()
            .is_empty());
    }

    /// GH #255 (hot path) — entries appended on this instance are in
    /// the GH #199 cache; the cache walk must honor the byte budget
    /// exactly like the cold path (pre-fix the cache had no sizes, so
    /// any budgeting would have forced a re-serialize).
    #[tokio::test]
    async fn limited_get_budgets_cache_hits() {
        let dir = tempfile::tempdir().unwrap();
        let mut store =
            FjallRaftLogStore::<TestConfig>::open(&dir.path().join("budgeted-hot")).unwrap();
        let entries: Vec<TestEntry> = (1..=5).map(|i| fat_entry(i, MIB)).collect();
        store.append(entries, noop()).await.unwrap();

        // All 5 are cache-resident (append populates the LRU).
        let got = store
            .limited_get_with_budget(1, 6, 2 * MIB + MIB / 2)
            .unwrap();
        assert_eq!(
            got.len(),
            2,
            "cache-hit budgeted read must return a 2-entry prefix, got {}",
            got.len()
        );
        assert_eq!(got[0].index(), 1);
        assert_eq!(got[1].index(), 2);

        // Generous budget → full range straight from cache.
        let all = store.limited_get_with_budget(1, 6, usize::MAX).unwrap();
        assert_eq!(all.len(), 5);

        // First cached entry alone over-budget → returned alone.
        let one = store.limited_get_with_budget(2, 6, 16).unwrap();
        assert_eq!(one.len(), 1, "never-empty contract violated on cache hit");
        assert_eq!(one[0].index(), 2);
    }

    /// GH #255 — small entries (steady-state replication shape) pass
    /// through the default budget untouched on both paths.
    #[tokio::test]
    async fn limited_get_small_entries_full_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budgeted-small");
        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            let entries: Vec<TestEntry> = (1..=20).map(|i| fat_entry(i, 256)).collect();
            store.append(entries, noop()).await.unwrap();
            // Hot: full range from cache.
            let hot = store
                .limited_get_with_budget(1, 21, crate::tcp_transport::replication_byte_budget())
                .unwrap();
            assert_eq!(hot.len(), 20);
        }
        // Cold: full range from fjall.
        let store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
        let cold = store
            .limited_get_with_budget(1, 21, crate::tcp_transport::replication_byte_budget())
            .unwrap();
        assert_eq!(cold.len(), 20);
    }

    #[tokio::test]
    async fn vote_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vote");

        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            let vote = openraft::Vote::new(1, 42);
            store.save_vote(&vote).await.unwrap();
        }

        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            let vote = store.read_vote().await.unwrap();
            assert!(vote.is_some());
        }
    }

    #[tokio::test]
    async fn has_state_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallRaftLogStore::<TestConfig>::open(&dir.path().join("empty")).unwrap();
        assert!(!store.has_state());
    }

    #[tokio::test]
    async fn entries_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist");

        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            let vote = openraft::Vote::new(1, 10);
            store.save_vote(&vote).await.unwrap();
            // Append entries via the underlying store directly —
            // creating proper Entry types requires internal openraft
            // constructors. We verify persistence at the inner layer.
            store.inner.append(1, &"entry-1").unwrap();
            store.inner.append(2, &"entry-2").unwrap();
            store.inner.append(3, &"entry-3").unwrap();
        }

        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            assert!(store.has_state(), "store should have state after reopen");
            assert_eq!(
                store.inner.get::<String>(1).unwrap(),
                Some("entry-1".to_string())
            );
            assert_eq!(
                store.inner.get::<String>(2).unwrap(),
                Some("entry-2".to_string())
            );
            assert_eq!(
                store.inner.get::<String>(3).unwrap(),
                Some("entry-3".to_string())
            );
            let vote = store.read_vote().await.unwrap();
            assert!(vote.is_some(), "vote should survive reopen");
        }
    }

    #[tokio::test]
    async fn has_state_after_vote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voted");
        let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
        let vote = openraft::Vote::new(1, 1);
        store.save_vote(&vote).await.unwrap();
        assert!(store.has_state());
    }
}
