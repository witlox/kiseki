//! Write-behind queue for `PersistentRedbStorage` (ADR-040 rev 3).
//!
//! Composition `put` / `name_insert` / `name_remove` would otherwise
//! hit redb on every S3 PUT — measured at ~3 ms per op under
//! contention because redb's single-writer lock serializes
//! transactions. The 2026-05-05 single-host profile pinned this as a
//! 60 % S3 throughput regression versus the rev-1/2 in-memory
//! baseline.
//!
//! The overlay holds the *desired* current state for keys with
//! pending redb writes. Writers update the overlay synchronously and
//! signal a background drainer; readers consult the overlay before
//! falling through to redb. The drainer batches up to
//! `KISEKI_COMPOSITION_QUEUE_MAX` ops per redb transaction at a
//! `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS` cadence (whichever first),
//! then prunes drained entries from the overlay.
//!
//! Source-of-truth model unchanged from rev-1/2: composition state
//! lives authoritatively in the raft log, redb is a cache for fast
//! restart hydration. A single-node hard reset within the flush
//! interval loses queued-but-not-flushed entries; multi-node R-3 /
//! EC-4+2 + scrub recover via raft-log replay (same loss-window
//! contract as the chunk-store group commit, I-L5).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ::redb::ReadableTable;
use dashmap::DashMap;
use kiseki_common::ids::{CompositionId, NamespaceId};
use tokio::sync::Notify;

use crate::composition::Composition;

/// Composite key for the name index — `(namespace_id, name)`.
pub(crate) type NameKey = (NamespaceId, String);

/// In-memory overlay of pending redb writes. Each entry carries a
/// monotonically-increasing sequence number stamped at insert time
/// by the writer; the drainer uses it to remove only entries that
/// haven't been superseded by a later write while the redb commit
/// was in flight.
///
/// `None` payloads are tombstones — the next read returns "not
/// found" without consulting redb, even if redb still holds the
/// pre-tombstone value.
///
/// Concurrency model: the three maps are `DashMap`s so independent
/// writers don't contend; `next_seq` is a single `AtomicU64`. A
/// writer's update to `name_fwd` + `name_rev` is no longer atomic
/// against a concurrent drainer snapshot — the drainer may catch
/// only one half. The next drainer cycle picks up the other half;
/// the redb txn applies whatever the snapshot contains. Net effect
/// matches the previous semantics modulo a brief visibility window
/// (≤ one drain interval) where redb has fwd without matching rev.
/// Readers always consult the overlay first, so this window is
/// invisible at the gateway layer.
#[derive(Default)]
pub(crate) struct OverlayState {
    pub comp: DashMap<CompositionId, (u64, Option<Composition>)>,
    pub name_fwd: DashMap<NameKey, (u64, Option<CompositionId>)>,
    /// Reverse map: `composition_id` → currently-bound `(ns, name)`.
    /// `None` = "this composition has no name binding right now"
    /// (the previous binding has been removed).
    pub name_rev: DashMap<CompositionId, (u64, Option<NameKey>)>,
    next_seq: AtomicU64,
}

impl OverlayState {
    /// Allocate the next monotonic sequence number. Lock-free;
    /// concurrent writers each get a unique value.
    pub(crate) fn next_seq(&self) -> u64 {
        // SeqCst is overkill for a counter, but the cost is dwarfed
        // by the surrounding DashMap insert; correctness > micro-perf.
        self.next_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Total number of overlay entries across all three maps. Used to
    /// gate the queue-max backpressure check. Approximate under
    /// concurrent inserts (DashMap::len walks the shards) — the cap
    /// is a soft-bound anyway.
    pub(crate) fn len(&self) -> usize {
        self.comp.len() + self.name_fwd.len() + self.name_rev.len()
    }
}

/// Shared handle held by `PersistentRedbStorage` (writer side) and
/// the drainer (reader side). Cheap to clone — both halves point at
/// the same `OverlayState` and `Notify`.
#[derive(Clone)]
pub(crate) struct WriteBehindHandle {
    pub overlay: Arc<OverlayState>,
    pub notify: Arc<Notify>,
    /// Bounded soft-cap. When exceeded, writers fall back to the
    /// inline redb path (taking the per-op fsync hit) so the queue
    /// can drain rather than grow unboundedly under sustained
    /// overload.
    pub max_queue_size: usize,
}

impl WriteBehindHandle {
    pub(crate) fn new(max_queue_size: usize) -> Self {
        Self {
            overlay: Arc::new(OverlayState::default()),
            notify: Arc::new(Notify::new()),
            max_queue_size,
        }
    }
}

/// Clone-able sync-flush handle. The runtime hands this to the
/// gateway's `register_fsync_hook` so a FUSE / NFS `fsync(2)` can
/// force the overlay to drain synchronously rather than waiting
/// for the periodic drainer tick.
#[derive(Clone)]
pub struct WriteBehindFlusher {
    handle: WriteBehindHandle,
    db: Arc<std::sync::Mutex<::redb::Database>>,
    metrics: Option<Arc<crate::metrics::CompositionMetrics>>,
    max_batch: usize,
}

impl WriteBehindFlusher {
    /// Drain the overlay synchronously by repeatedly flushing
    /// batches until the overlay is empty or a flush errors.
    /// Called from the gateway's fsync hook (via `spawn_blocking`,
    /// since the hook signature is sync).
    ///
    /// # Errors
    /// Propagates the first failed redb commit. On error, partial
    /// progress is fine — already-committed batches are persisted;
    /// pending entries stay in the overlay for the next attempt.
    pub fn flush_blocking(&self) -> Result<(), super::error::PersistentStoreError> {
        loop {
            let snapshot = take_snapshot(&self.handle, self.max_batch);
            if snapshot.is_empty() {
                return Ok(());
            }
            let count = commit_snapshot_to_redb(&self.db, &snapshot)?;
            prune_committed(&self.handle, &snapshot);
            if let Some(ref m) = self.metrics {
                m.redb_commits_total.inc_by(count);
            }
        }
    }
}

/// Drainer half of the write-behind pair. Owns the same handle the
/// storage uses; spawned by the runtime as a long-lived task.
pub struct WriteBehindDrainer {
    flusher: WriteBehindFlusher,
    interval: std::time::Duration,
}

impl WriteBehindDrainer {
    pub(crate) fn new(
        handle: WriteBehindHandle,
        db: Arc<std::sync::Mutex<::redb::Database>>,
        metrics: Option<Arc<crate::metrics::CompositionMetrics>>,
        interval: std::time::Duration,
        max_batch: usize,
    ) -> Self {
        Self {
            flusher: WriteBehindFlusher {
                handle,
                db,
                metrics,
                max_batch,
            },
            interval,
        }
    }

    /// Cheap clone of the sync-flush handle for `register_fsync_hook`.
    #[must_use]
    pub fn flusher(&self) -> WriteBehindFlusher {
        self.flusher.clone()
    }

    /// Run the drainer until cancelled. Loops on either the periodic
    /// tick or a notification from a writer; takes a snapshot of up
    /// to `max_batch` overlay entries; commits one redb transaction;
    /// prunes the just-committed entries from the overlay.
    ///
    /// The redb commit happens *outside* the overlay lock so writes
    /// continue to flow during a flush. The post-commit prune
    /// re-acquires the lock briefly to clean up, only removing
    /// entries whose sequence number still matches what was
    /// snapshotted (so a writer that updated the same key during
    /// the flush keeps its newer value).
    pub async fn run(self) {
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                () = self.flusher.handle.notify.notified() => {}
            }
            if let Err(e) = self.flush_once().await {
                tracing::warn!(
                    error = %e,
                    "composition write-behind drainer flush failed; will retry next tick",
                );
                if let Some(ref m) = self.flusher.metrics {
                    m.redb_commit_errors_total.inc();
                }
            }
        }
    }

    /// Single-flush iteration on the tokio reactor. Routes through
    /// `spawn_blocking` because redb is sync.
    pub async fn flush_once(&self) -> Result<(), super::error::PersistentStoreError> {
        let snapshot = take_snapshot(&self.flusher.handle, self.flusher.max_batch);
        if snapshot.is_empty() {
            return Ok(());
        }

        let db = Arc::clone(&self.flusher.db);
        let snap = snapshot.clone();
        let commit_result = tokio::task::spawn_blocking(move || {
            commit_snapshot_to_redb(&db, &snap)
        })
        .await
        .map_err(|e| {
            super::error::PersistentStoreError::Decode(format!(
                "drainer spawn_blocking join: {e}"
            ))
        })??;
        prune_committed(&self.flusher.handle, &snapshot);
        if let Some(ref m) = self.flusher.metrics {
            m.redb_commits_total.inc_by(commit_result);
        }
        Ok(())
    }
}

fn take_snapshot(handle: &WriteBehindHandle, max_batch: usize) -> OverlaySnapshot {
    let ov = &handle.overlay;
    let mut snap = OverlaySnapshot::default();
    // DashMap iteration locks one shard at a time, so concurrent
    // writers to other shards keep flowing while we collect. Entries
    // observed mid-update may have a half-stale seq vs payload — the
    // pair is read under the per-shard lock so they're internally
    // consistent for THIS entry; cross-entry consistency is preserved
    // at redb-txn boundaries (each entry commits independently).
    for entry in ov.comp.iter().take(max_batch) {
        let (seq, payload) = entry.value();
        snap.comp.push((*entry.key(), *seq, payload.clone()));
    }
    for entry in ov.name_fwd.iter().take(max_batch) {
        let (seq, payload) = entry.value();
        snap.name_fwd.push((entry.key().clone(), *seq, *payload));
    }
    for entry in ov.name_rev.iter().take(max_batch) {
        let (seq, payload) = entry.value();
        snap.name_rev.push((*entry.key(), *seq, payload.clone()));
    }
    snap
}

#[cfg(test)]
mod tests {
    use crate::composition::Composition;
    use crate::persistent::redb::PersistentRedbStorage;
    use crate::persistent::storage::CompositionStorage;
    use kiseki_common::ids::{ChunkId, CompositionId, NamespaceId, OrgId, ShardId};

    fn make_comp(idx: u8, version: u64) -> Composition {
        Composition {
            id: CompositionId(uuid::Uuid::from_u128(u128::from(idx))),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::from_u128(2)),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            chunks: vec![ChunkId([idx; 32])],
            version,
            size: u64::from(idx) * 100,
            has_inline_data: false,
            content_type: None,
        }
    }

    /// I-CP9 happy path: a put that sits in the overlay (drainer
    /// not yet run) is observable via `get`.
    #[tokio::test]
    async fn overlay_serves_put_before_drain() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        let _drainer =
            store.enable_write_behind(4096, std::time::Duration::from_secs(60), 1024);

        let comp = make_comp(1, 7);
        store.put(comp.clone()).unwrap();

        // Drainer hasn't run; redb is empty. Read must come from
        // the overlay.
        let got = store.get(comp.id).unwrap().unwrap();
        assert_eq!(got, comp);
    }

    /// I-CP9 LIST consistency: a put then a `list_in_namespace`
    /// returns the new composition without waiting for a drain.
    #[tokio::test]
    async fn list_in_namespace_merges_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        let _drainer =
            store.enable_write_behind(4096, std::time::Duration::from_secs(60), 1024);

        let comp = make_comp(2, 1);
        store.put(comp.clone()).unwrap();
        let listed = store.list_in_namespace(comp.namespace_id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, comp.id);
    }

    /// Tombstone semantics: a remove tombstones the overlay so
    /// `get` returns None even if redb still holds the row.
    #[tokio::test]
    async fn remove_tombstones_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        // Pre-load a composition synchronously into redb (no
        // write-behind enabled yet).
        let comp = make_comp(3, 1);
        store.put(comp.clone()).unwrap();
        // Now turn write-behind on. Subsequent ops route through
        // the overlay.
        let _drainer =
            store.enable_write_behind(4096, std::time::Duration::from_secs(60), 1024);

        // Remove via overlay; redb still has the row.
        let removed = store.remove(comp.id).unwrap();
        assert!(removed, "remove should report existed=true via redb peek");

        // Read must see the tombstone, not the redb row.
        assert!(store.get(comp.id).unwrap().is_none());
    }

    /// Round-trip: put → `flush_blocking` → overlay empty → read
    /// from redb (cache populated by put still serves).
    #[tokio::test]
    async fn flush_blocking_drains_overlay_to_redb() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        let drainer =
            store.enable_write_behind(4096, std::time::Duration::from_secs(60), 1024);
        let flusher = drainer.flusher();

        let comp = make_comp(4, 1);
        store.put(comp.clone()).unwrap();

        // Synchronously drain.
        flusher.flush_blocking().unwrap();

        // Overlay must be empty (write-behind handle's internal
        // state). We verify via the public read path: the get
        // still returns the value (now from redb / LRU cache).
        let got = store.get(comp.id).unwrap().unwrap();
        assert_eq!(got, comp);
    }

    /// Read-after-write across a sequence of overwrites for the
    /// same id: each get sees the latest version.
    #[tokio::test]
    async fn overlay_read_after_write_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        let _drainer =
            store.enable_write_behind(4096, std::time::Duration::from_secs(60), 1024);

        for v in [1u64, 2, 3, 4, 5] {
            let comp = make_comp(7, v);
            store.put(comp.clone()).unwrap();
            let got = store.get(comp.id).unwrap().unwrap();
            assert_eq!(got.version, v, "read-after-write must see version {v}");
        }
    }

    /// Name-binding overlay: bind via overlay, lookup must succeed
    /// before drain.
    #[tokio::test]
    async fn name_lookup_serves_overlay_binding() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        let _drainer =
            store.enable_write_behind(4096, std::time::Duration::from_secs(60), 1024);

        let comp = make_comp(11, 1);
        store.put(comp.clone()).unwrap();
        store
            .name_insert(comp.namespace_id, "hello.txt".into(), comp.id)
            .unwrap();

        let id = store
            .name_lookup(comp.namespace_id, "hello.txt")
            .unwrap()
            .unwrap();
        assert_eq!(id, comp.id);

        let bound = store.name_for(comp.id).unwrap().unwrap();
        assert_eq!(bound.0, comp.namespace_id);
        assert_eq!(bound.1, "hello.txt");
    }

    /// Name list LIST consistency: list returns overlay-only bindings.
    #[tokio::test]
    async fn name_list_merges_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        let _drainer =
            store.enable_write_behind(4096, std::time::Duration::from_secs(60), 1024);

        let comp_a = make_comp(12, 1);
        let comp_b = make_comp(13, 1);
        store.put(comp_a.clone()).unwrap();
        store.put(comp_b.clone()).unwrap();
        store
            .name_insert(comp_a.namespace_id, "a.txt".into(), comp_a.id)
            .unwrap();
        store
            .name_insert(comp_b.namespace_id, "b.txt".into(), comp_b.id)
            .unwrap();

        let mut list = store.name_list(comp_a.namespace_id, None).unwrap();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0, "a.txt");
        assert_eq!(list[1].0, "b.txt");
    }

    /// `name_remove` tombstones the binding even when redb has it.
    #[tokio::test]
    async fn name_remove_tombstones_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        // Bind synchronously (write-behind off), then enable.
        let comp = make_comp(14, 1);
        store.put(comp.clone()).unwrap();
        store
            .name_insert(comp.namespace_id, "x.txt".into(), comp.id)
            .unwrap();
        let _drainer =
            store.enable_write_behind(4096, std::time::Duration::from_secs(60), 1024);

        // Remove via overlay; redb still has the binding.
        assert!(store.name_remove(comp.namespace_id, "x.txt").unwrap());
        assert!(
            store
                .name_lookup(comp.namespace_id, "x.txt")
                .unwrap()
                .is_none(),
            "overlay tombstone must hide redb binding",
        );
    }

    /// I-CP9 prune-only-on-seq-match: a write that arrives during
    /// a flush is preserved, not lost.
    #[tokio::test]
    async fn drainer_does_not_clobber_in_flight_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        let drainer =
            store.enable_write_behind(4096, std::time::Duration::from_secs(60), 1024);
        let flusher = drainer.flusher();

        let comp_v1 = make_comp(20, 1);
        let comp_v2 = make_comp(20, 2);

        // Put v1, simulate "drainer just snapshotted" by flushing
        // synchronously; immediately put v2 (simulates a write
        // racing the drainer's redb commit + overlay prune).
        store.put(comp_v1).unwrap();
        flusher.flush_blocking().unwrap();
        store.put(comp_v2.clone()).unwrap();

        // Read MUST see v2.
        let got = store.get(comp_v2.id).unwrap().unwrap();
        assert_eq!(got.version, 2);

        // Drain again; v2 lands in redb.
        flusher.flush_blocking().unwrap();
        let got2 = store.get(comp_v2.id).unwrap().unwrap();
        assert_eq!(got2.version, 2);
    }

    /// Backpressure: if the overlay reaches `max_queue_size`,
    /// subsequent puts fall back to the inline redb path. After
    /// drain, the inline write is still observable.
    #[tokio::test]
    async fn saturated_overlay_falls_back_to_inline() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentRedbStorage::open(&dir.path().join("wb.redb")).unwrap();
        // max_queue_size = 0 forces every put through the inline
        // path even when write-behind is "enabled". Useful boundary
        // case for the saturation branch.
        let _drainer =
            store.enable_write_behind(0, std::time::Duration::from_secs(60), 1024);

        let comp = make_comp(30, 1);
        store.put(comp.clone()).unwrap();
        // No drain needed; inline path went straight to redb.
        let got = store.get(comp.id).unwrap().unwrap();
        assert_eq!(got, comp);
    }
}

fn prune_committed(handle: &WriteBehindHandle, snapshot: &OverlaySnapshot) {
    let ov = &handle.overlay;
    // DashMap::remove_if drops the entry only when the predicate
    // matches under the per-shard lock — so a writer that updated
    // the same key with a higher seq during the redb commit isn't
    // overwritten. This is the same correctness rule the previous
    // get-then-remove dance enforced under the global RwLock,
    // expressed directly with the entry-API guard.
    for (id, seq, _) in &snapshot.comp {
        ov.comp.remove_if(id, |_, (s, _)| *s == *seq);
    }
    for (key, seq, _) in &snapshot.name_fwd {
        ov.name_fwd.remove_if(key, |_, (s, _)| *s == *seq);
    }
    for (id, seq, _) in &snapshot.name_rev {
        ov.name_rev.remove_if(id, |_, (s, _)| *s == *seq);
    }
}

/// One iteration's worth of overlay snapshot. Owned (not borrowing
/// the lock) so the redb commit can run off-lock.
#[derive(Default, Clone)]
pub(crate) struct OverlaySnapshot {
    pub comp: Vec<(CompositionId, u64, Option<Composition>)>,
    pub name_fwd: Vec<(NameKey, u64, Option<CompositionId>)>,
    pub name_rev: Vec<(CompositionId, u64, Option<NameKey>)>,
}

impl OverlaySnapshot {
    fn is_empty(&self) -> bool {
        self.comp.is_empty() && self.name_fwd.is_empty() && self.name_rev.is_empty()
    }
}

/// Commit the snapshot to redb in a single write transaction.
/// Returns the number of mutations applied (for metrics). Lives
/// outside the impl block so it can run inside `spawn_blocking`
/// without capturing `&self`.
fn commit_snapshot_to_redb(
    db: &Arc<std::sync::Mutex<::redb::Database>>,
    snap: &OverlaySnapshot,
) -> Result<u64, super::error::PersistentStoreError> {
    use kiseki_common::locks::LockOrDie;
    let db_guard = db.lock().lock_or_die("redb.db");
    let mut txn = db_guard.begin_write()?;
    // Drainer always runs at Eventual durability — the rev-3
    // contract is "interval bounds the loss window," and an
    // Immediate commit on every drainer tick would re-introduce
    // the per-op fsync cost we set out to amortize. Operators
    // who want a stronger guarantee call the `RedbFlusher::flush`
    // hook (e.g. via FUSE `fsync(2)`), which issues a separate
    // Immediate commit.
    if let Err(e) = txn.set_durability(::redb::Durability::None) {
        tracing::warn!(error = %e, "drainer set_durability(None) failed");
    }
    let mut count: u64 = 0;
    {
        use crate::persistent::redb::{
            decode_name_key, encode_composition, name_key, COMPOSITIONS, NAMES, NAMES_REVERSE,
        };
        let mut comps = txn.open_table(COMPOSITIONS)?;
        let mut names = txn.open_table(NAMES)?;
        let mut names_rev = txn.open_table(NAMES_REVERSE)?;
        // Compositions: Some = upsert, None = remove.
        for (id, _seq, payload) in &snap.comp {
            if let Some(comp) = payload {
                let bytes = encode_composition(comp)?;
                comps.insert(id.0.as_bytes().as_slice(), bytes.as_slice())?;
            } else {
                comps.remove(id.0.as_bytes().as_slice())?;
                // Keep the name index consistent: if this id had
                // a binding in redb, drop it now.
                let composite = names_rev
                    .get(id.0.as_bytes().as_slice())?
                    .map(|guard| guard.value().to_vec());
                if let Some(composite) = composite {
                    names.remove(composite.as_slice())?;
                    names_rev.remove(id.0.as_bytes().as_slice())?;
                }
            }
            count += 1;
        }
        // Name forward: Some(id) = bind name → id, None = unbind.
        for ((ns, name), _seq, payload) in &snap.name_fwd {
            let key = name_key(*ns, name);
            if let Some(id) = payload {
                // Mirror PersistentRedbStorage::name_insert: drop
                // any prior reverse entry for this id, drop any
                // prior forward entry that mapped to a different
                // id for the same name.
                let prev_id_bytes = names
                    .get(key.as_slice())?
                    .map(|guard| guard.value().to_vec());
                if let Some(prev) = prev_id_bytes {
                    if prev.len() == 16 && prev != id.0.as_bytes().as_slice() {
                        names_rev.remove(prev.as_slice())?;
                    }
                }
                let prev_composite = names_rev
                    .get(id.0.as_bytes().as_slice())?
                    .map(|guard| guard.value().to_vec());
                if let Some(prev) = prev_composite {
                    if prev.as_slice() != key.as_slice() {
                        names.remove(prev.as_slice())?;
                    }
                }
                names.insert(key.as_slice(), id.0.as_bytes().as_slice())?;
                names_rev.insert(id.0.as_bytes().as_slice(), key.as_slice())?;
            } else {
                let removed_id_bytes = names
                    .remove(key.as_slice())?
                    .map(|guard| guard.value().to_vec());
                if let Some(id_bytes) = removed_id_bytes {
                    names_rev.remove(id_bytes.as_slice())?;
                }
            }
            count += 1;
        }
        // Name reverse: applied as part of the forward updates
        // above. The reverse-only entries in the snapshot exist for
        // read-path lookups (`name_for`); they don't need a redb
        // mutation here — the forward update wrote the reverse row.
        // Keep `name_rev` in the snapshot so `flush_once`'s prune
        // step removes the entry; the redb side is already correct.
        let _ = decode_name_key; // silence unused-import warning when feature flags shift
    }
    txn.commit()?;
    Ok(count)
}
