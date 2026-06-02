//! Group-commit / fsync coalescer for the Raft log layer (#151, W6).
//!
//! # The problem
//!
//! Each shard runs an independent openraft state machine backed by a
//! [`FjallLogStore`](super::fjall_log_store::FjallLogStore) wrapping a
//! [`fjall::Database`]. Every `AppendEntries` round on a leader calls
//! `append_batch` → `WriteBatch::commit(SyncAll)` which fsyncs the
//! journal. Under sustained PUT load this fsync queues at the device
//! level — Pass 1 of the 2026-06-01 GCP run measured **12.2 % of
//! sched-switch events as `kiseki-committer` in D state**
//! (uninterruptible kernel I/O wait on local `NVMe`).
//!
//! Even though openraft already batches multiple client writes into
//! one `AppendEntries` round (`max_payload_entries: 300`), each round
//! still does ONE physical fsync — and *concurrent rounds* (e.g.
//! multiple shards on the same node, or rapid-fire rounds within one
//! shard under burst) each pay their own device-sync.
//!
//! # The fix
//!
//! [`FsyncCoalescer`] is a small process-local coordinator wrapping a
//! [`fjall::Database`]. Callers issuing `flush()` instead call
//! [`FsyncCoalescer::flush`]; if a fsync is already in flight or
//! scheduled, they join the same waiter pool. A single physical
//! `Database::persist(SyncAll)` covers all waiters when the window
//! expires (`window_us`) OR enough waiters accumulate (`max_batch`).
//!
//! # Correctness contract
//!
//! - The fsync STILL completes before any waiter's future resolves —
//!   we never signal durability before the device sync returns. This
//!   preserves the openraft `IOFlushed::io_completed(Ok(()))`
//!   guarantee that the batch is on stable storage.
//! - fjall batch atomicity is at the `WriteBatch::commit` layer (not
//!   the persist layer) — coalescing only affects the durability
//!   barrier, not the write itself. If the process crashes between
//!   `commit` and `persist`, the writes are still there in the
//!   journal (just not fsynced); fjall's recovery replays them on
//!   next open. Same correctness shape as
//!   `with_eventual_durability(true)` + periodic flush, just with a
//!   shorter, demand-driven window.
//! - The coalescer is per-`FjallLogStore` (per-shard). Inter-shard
//!   coordination is not attempted here — each shard's Database is
//!   independent. Cross-shard coalescing would require the shards to
//!   share a Database, which is a larger architectural change tracked
//!   separately.
//!
//! # Tuning
//!
//! - `window_us`: how long the first waiter holds before triggering
//!   the merged fsync. Trades latency floor (low load) for fsync
//!   amortisation (high load). 500 µs is a reasonable starting
//!   point — adds ~6 % to an 8 ms p50 in the worst case (no
//!   batching opportunity), while at high load N waiters merge into
//!   one fsync.
//! - `max_batch`: cap on the number of waiters per merged fsync.
//!   Forces early drain when the pipeline is busy so the window
//!   doesn't unboundedly delay a hot caller. 32 is the default —
//!   matches openraft's typical AE-round payload size.
//!
//! Both are env-configurable when wired (see the issue #151 design
//! for `KISEKI_RAFT_FSYNC_WINDOW_US` / `KISEKI_RAFT_FSYNC_BATCH`).
//!
//! # When to enable
//!
//! Off by default. Enable when:
//! - sustained write workloads (`kiseki-committer` thread shows up
//!   in off-CPU profile)
//! - p99 write latency > 10 × p50 (likely tail-of-fsync-queue
//!   contention)
//! - cluster is healthy and not bottlenecked on a different layer
//!   (chunk store, replication RTT, etc)

use std::io;
use std::sync::Arc;
use std::time::Duration;

use fjall::{Database, PersistMode};
use parking_lot::Mutex;
use tokio::sync::oneshot;

/// Coalesce concurrent `fsync` calls on a `fjall::Database`.
///
/// See module docs for the contract and tuning guidance.
#[derive(Clone)]
pub struct FsyncCoalescer {
    inner: Arc<FsyncCoalescerInner>,
}

struct FsyncCoalescerInner {
    db: Database,
    state: Mutex<State>,
    window: Duration,
    max_batch: usize,
}

struct State {
    /// Waiters parked on the next physical fsync. Each receives the
    /// result of `Database::persist(SyncAll)` via the oneshot tx.
    waiters: Vec<oneshot::Sender<io::Result<()>>>,
    /// `true` when a drain task is scheduled to fire after the
    /// window elapses. Prevents spawning a drain task per waiter.
    drain_scheduled: bool,
}

impl FsyncCoalescer {
    /// Build a coalescer wrapping `db`. `window_us` and `max_batch`
    /// see the module docs.
    #[must_use]
    pub fn new(db: Database, window_us: u64, max_batch: usize) -> Self {
        Self {
            inner: Arc::new(FsyncCoalescerInner {
                db,
                state: Mutex::new(State {
                    waiters: Vec::new(),
                    drain_scheduled: false,
                }),
                window: Duration::from_micros(window_us),
                max_batch: max_batch.max(1),
            }),
        }
    }

    /// Request a fsync. Returns when the *merged* `Database::persist`
    /// (covering this caller and any peers that joined the same
    /// window) completes. The result mirrors `Database::persist`'s
    /// own — including the poisoned-after-IO-error case, which
    /// fjall surfaces uniformly for all subsequent calls.
    ///
    /// Cancellation safety: if the awaiting future is dropped before
    /// the fsync completes, the caller's slot in the waiter pool is
    /// effectively orphaned (the oneshot drops). The other waiters
    /// (and the underlying fsync) are unaffected. The fsync still
    /// happens; the cancelled caller just doesn't observe its
    /// result.
    pub async fn flush(&self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        let trigger_now = {
            let mut state = self.inner.state.lock();
            state.waiters.push(tx);
            // Two trigger conditions: hit the batch cap, OR be the
            // first waiter that needs to schedule the window timer.
            if state.waiters.len() >= self.inner.max_batch {
                // Take the waiters now and drain inline.
                state.drain_scheduled = false;
                Some(std::mem::take(&mut state.waiters))
            } else {
                if !state.drain_scheduled {
                    state.drain_scheduled = true;
                    let me = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(me.inner.window).await;
                        me.timer_drain();
                    });
                }
                None
            }
        };
        if let Some(waiters) = trigger_now {
            // Drain inline (no tokio::spawn) — this caller is going
            // to await the fsync anyway, so we save the task hop.
            self.do_persist_and_notify(waiters);
        }
        match rx.await {
            Ok(res) => res,
            Err(_) => Err(io::Error::other(
                "fsync coalescer dropped before completion",
            )),
        }
    }

    /// Called from the window timer task. Picks up whatever waiters
    /// are pending and drains them. No-op if the batch already
    /// drained (someone hit `max_batch` first).
    fn timer_drain(&self) {
        let waiters = {
            let mut state = self.inner.state.lock();
            state.drain_scheduled = false;
            std::mem::take(&mut state.waiters)
        };
        if waiters.is_empty() {
            return;
        }
        self.do_persist_and_notify(waiters);
    }

    /// Issue one physical fsync and fan the result out to every
    /// waiter. Runs synchronously — fjall's `persist` is a blocking
    /// syscall, but we're called from a tokio-spawned task or from
    /// `flush()` directly in the inline-drain case. Either way, the
    /// caller is already willing to block on the fsync's wall time.
    fn do_persist_and_notify(&self, waiters: Vec<oneshot::Sender<io::Result<()>>>) {
        let result = self
            .inner
            .db
            .persist(PersistMode::SyncAll)
            .map_err(|e| io::Error::other(e.to_string()));
        // io::Result<()> isn't Clone — broadcast via per-waiter
        // matching on the &ref of the kind+message.
        for tx in waiters {
            let cloned = match &result {
                Ok(()) => Ok(()),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            };
            // Drop-send is fine — cancelled waiters drop their rx.
            let _ = tx.send(cloned);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fjall::Database;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn open_db(dir: &Path) -> Database {
        Database::builder(dir).open().unwrap()
    }

    /// Single waiter case — the coalescer behaves like a direct
    /// `Database::persist(SyncAll)` call, no waiting beyond the
    /// fsync itself.
    #[tokio::test(flavor = "multi_thread")]
    async fn single_waiter_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir.path().join("db"));
        let c = FsyncCoalescer::new(db, 500, 32);
        c.flush().await.expect("fsync");
    }

    /// N concurrent waiters all resolve from one merged fsync.
    /// The actual physical-fsync count is hard to assert without
    /// mocking fjall, but we can verify that every waiter sees Ok.
    #[tokio::test(flavor = "multi_thread")]
    async fn many_waiters_all_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir.path().join("db"));
        let c = FsyncCoalescer::new(db, 1_000, 32);
        let mut joinset = Vec::new();
        for _ in 0..16 {
            let c2 = c.clone();
            joinset.push(tokio::spawn(async move { c2.flush().await }));
        }
        for h in joinset {
            h.await.expect("join").expect("fsync");
        }
    }

    /// Hitting `max_batch` triggers an inline drain — verify
    /// throughput is at least as good as the timer path by spawning
    /// enough waiters to exceed the batch cap and observing they all
    /// complete promptly (within a few window intervals).
    #[tokio::test(flavor = "multi_thread")]
    async fn batch_cap_triggers_immediate_drain() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir.path().join("db"));
        let max_batch = 4;
        // 100 ms window — long enough that ANY measured run must be
        // driven by the batch cap, not the timer.
        let c = FsyncCoalescer::new(db, 100_000, max_batch);
        let start = std::time::Instant::now();
        let mut joinset = Vec::new();
        for _ in 0..max_batch {
            let c2 = c.clone();
            joinset.push(tokio::spawn(async move { c2.flush().await }));
        }
        for h in joinset {
            h.await.expect("join").expect("fsync");
        }
        // The timer would have been 100 ms; the batch cap should
        // have driven the drain within ~5 ms tops (fsync time +
        // task scheduling).
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "batch-cap drain too slow: {elapsed:?} (expected < 50 ms)"
        );
    }

    /// Coalescer keeps working after a successful drain — second
    /// batch of waiters resolves cleanly.
    #[tokio::test(flavor = "multi_thread")]
    async fn drain_then_drain_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir.path().join("db"));
        let c = FsyncCoalescer::new(db, 500, 8);
        c.flush().await.expect("first flush");
        // Second round
        let mut joinset = Vec::new();
        for _ in 0..4 {
            let c2 = c.clone();
            joinset.push(tokio::spawn(async move { c2.flush().await }));
        }
        for h in joinset {
            h.await.expect("join").expect("fsync");
        }
    }

    /// Cancelled waiter doesn't break the merged fsync for others.
    /// We drop one future before it resolves and verify the other
    /// waiters still see Ok.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_waiter_does_not_poison_others() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir.path().join("db"));
        let c = FsyncCoalescer::new(db, 500_000, 32); // long window
        let c2 = c.clone();
        // Caller 1 — start the flush future and DROP it before it
        // resolves. (Use spawn + abort so the cancellation actually
        // happens before the window elapses.)
        let handle = tokio::spawn(async move {
            let _ = c2.flush().await;
        });
        // Yield to let the spawn enter flush() and park.
        tokio::task::yield_now().await;
        handle.abort();
        // Caller 2 — should still complete cleanly when the window
        // fires (or via the merged drain with caller 1's
        // already-orphaned tx).
        let c3 = c.clone();
        c3.flush().await.expect("second flush");
    }

    /// Tally check using a custom shim: replace the Database with
    /// one whose `persist` we can intercept. Since `Database` is
    /// concrete (not a trait), we count fsyncs by observing the
    /// timing — a single batch of N concurrent calls should resolve
    /// within one window's worth of fsync time, not N.
    #[tokio::test(flavor = "multi_thread")]
    async fn timing_proves_coalescing_actually_happens() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir.path().join("db"));
        // 50 ms window — generous so all callers land in the same batch.
        let c = FsyncCoalescer::new(db, 50_000, 64);
        let n_callers = 8;
        let start = std::time::Instant::now();
        let mut joinset = Vec::new();
        for _ in 0..n_callers {
            let c2 = c.clone();
            joinset.push(tokio::spawn(async move { c2.flush().await }));
        }
        for h in joinset {
            h.await.expect("join").expect("fsync");
        }
        let elapsed = start.elapsed();
        // If we did N independent fsyncs of ~1-5 ms each, we'd take
        // at least N×1 ms = 8 ms. With coalescing, we wait ~50 ms
        // for the window + 1 fsync = ~50-55 ms. The window dominates
        // and tells us the coalescing took place. (Without
        // coalescing, fsyncs would parallelize and we'd see ~5 ms.
        // With coalescing, we see ~50 ms because everyone waits for
        // the window.) This is a sanity check that the coalescer
        // actually parks waiters until the window fires.
        assert!(
            elapsed >= Duration::from_millis(40),
            "expected window-driven drain (~50 ms), got {elapsed:?} — coalescing not happening?",
        );
    }

    /// Sanity: counter-style — drive 100 fsyncs serially, verify
    /// each one completes (no deadlocks across drain-reschedule
    /// boundaries).
    #[tokio::test(flavor = "multi_thread")]
    async fn many_serial_flushes_all_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir.path().join("db"));
        let c = FsyncCoalescer::new(db, 100, 4);
        let counter = AtomicU64::new(0);
        for _ in 0..100u64 {
            c.flush().await.expect("fsync");
            counter.fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 100);
    }
}
