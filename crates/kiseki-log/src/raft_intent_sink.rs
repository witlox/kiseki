//! ADR-047 phase 5a (option 2) — the real [`IncorporationSink`].
//!
//! [`crate::intent_committer`] owns the pure incorporation algorithm behind the
//! synchronous [`IncorporationSink`] seam; [`crate::intent`] owns the durable
//! intent record. This module bridges that seam to the **live Raft log**: it
//! drains committed intents into the log via `append_intent`, which stamps each
//! `ChunkAndDelta` command with its committer-assigned perspective-seq so the
//! state machine records `max_incorporated_seq` — the gate-1 **F-2** floor a
//! recovering leader reads to drop re-gathered intents (option 2: the floor is
//! recorded *in the state machine*, not derived from prune state).
//!
//! The async log API ([`OpenRaftLogStore::append_intent`] /
//! [`OpenRaftLogStore::max_incorporated_seq`]) is abstracted behind
//! [`IntentLogAppender`] so the sink is unit-testable without a live raft node
//! (the tests fake the appender). Dispatch is generic, not `dyn` — the trait's
//! `async fn`s have no object-safe form here and none is needed.
//!
//! **This module delivers the sink and its tests only.** Wiring the per-shard
//! committer task that drives it (on a dedicated thread per the threading
//! contract below) is phase 5b/5c — this layer spawns nothing.

use tokio::runtime::Handle;

use kiseki_common::time::HybridLogicalClock;

use crate::error::LogError;
use crate::intent::{IntentError, PerspectiveSeq};
use crate::intent_committer::IncorporationSink;
use crate::traits::AppendChunkAndDeltaRequest;

/// The async seam the [`RaftLogIncorporationSink`] needs from the log:
/// append an incorporated intent and read the recorded F-2 floor.
///
/// Implemented by [`crate::raft::openraft_store::OpenRaftLogStore`] in
/// production; faked in tests so the sink is exercised without a live raft
/// node. Used via **generic dispatch** (`A: IntentLogAppender`), so the
/// `async fn` form is fine — there is no `dyn IntentLogAppender`.
#[allow(async_fn_in_trait)]
pub trait IntentLogAppender: Send + Sync {
    /// Append one incorporated intent to the Raft log under `seq`, recording
    /// `seq` into the state machine's `max_incorporated_seq`.
    ///
    /// # Errors
    /// [`LogError`] on any append / Raft client-write failure.
    async fn append_intent(
        &self,
        req: AppendChunkAndDeltaRequest,
        seq: PerspectiveSeq,
    ) -> Result<(), LogError>;

    /// The highest perspective-seq already incorporated into the log — the
    /// recovery floor. `None` until the first intent is incorporated.
    async fn max_incorporated_seq(&self) -> Option<HybridLogicalClock>;
}

/// The real [`IncorporationSink`]: drains committed intents into the Raft log
/// via [`IntentLogAppender::append_intent`].
///
/// Bridges the **synchronous** [`IncorporationSink`] seam (the committer drives
/// it synchronously) to the **async** log API via [`Handle::block_on`]. The
/// cached `max_incorporated` is seeded from the log on construction so the
/// committer applies the F-2 floor from the first pass.
///
/// # THREADING CONTRACT
/// The per-shard committer task that drives this sink **MUST** run on its own
/// dedicated thread — a `std::thread` holding a runtime [`Handle`] — **NOT** a
/// shared tokio worker thread. Wiring that dedicated-thread committer is phase
/// 5c (`RaftShardStore::spawn_committer`).
///
/// The sink bridges sync → async by blocking that dedicated thread on the Raft
/// log append. Two call shapes are supported, picked at runtime:
///
/// - **Outside any runtime context** (a bare `std::thread`, or a `#[test]`
///   thread): a plain [`Handle::block_on`].
/// - **Inside a runtime context** (the phase-5c committer thread drives
///   [`run_committer_loop`](crate::shard_committer::run_committer_loop) via
///   `Handle::block_on`, so the sync `tick` → `incorporate` runs *while* a
///   `block_on` is active): [`tokio::task::block_in_place`] wraps the inner
///   `block_on`, telling tokio this worker is about to block so it can move
///   other tasks off it. `block_in_place` requires the multi-threaded Raft
///   runtime (it is — `RaftShardStore::new` builds `new_multi_thread`).
///
/// A plain nested `Handle::block_on` would panic ("Cannot start a runtime from
/// within a runtime"); the [`block_on_maybe_in_place`] helper avoids that.
pub struct RaftLogIncorporationSink<A: IntentLogAppender> {
    appender: A,
    handle: Handle,
    max_incorporated: Option<PerspectiveSeq>,
}

impl<A: IntentLogAppender> RaftLogIncorporationSink<A> {
    /// Build a sink over `appender`, blocking on `handle` to seed the recovery
    /// cache from the log's recorded `max_incorporated_seq`.
    ///
    /// Safe whether or not the caller is inside a runtime context (see the
    /// type's threading contract): uses [`block_on_maybe_in_place`].
    #[must_use]
    pub fn new(appender: A, handle: Handle) -> Self {
        let max_incorporated =
            block_on_maybe_in_place(&handle, appender.max_incorporated_seq()).map(PerspectiveSeq);
        Self {
            appender,
            handle,
            max_incorporated,
        }
    }
}

/// `block_on` `fut` on `handle`, choosing the call shape by whether the current
/// thread is already inside a runtime context:
///
/// - inside a runtime → [`tokio::task::block_in_place`] + `handle.block_on`
///   (the phase-5c committer thread, which drives the loop via `block_on`);
/// - outside → a plain `handle.block_on` (a bare `std::thread` / `#[test]`).
///
/// This is the one seam that lets the sync committer drive the async log append
/// from a thread that may itself be inside a `block_on` without the nested-
/// runtime panic. `block_in_place` requires the multi-threaded Raft runtime.
fn block_on_maybe_in_place<F: std::future::Future>(handle: &Handle, fut: F) -> F::Output {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| handle.block_on(fut))
    } else {
        handle.block_on(fut)
    }
}

impl<A: IntentLogAppender> IncorporationSink for RaftLogIncorporationSink<A> {
    fn max_incorporated_seq(&self) -> Option<PerspectiveSeq> {
        self.max_incorporated
    }

    /// Append `ordered` (already ascending, every element strictly above the
    /// current floor) into the Raft log, advancing the cached floor after each.
    ///
    /// Blocks the dedicated committer thread on each append via
    /// [`block_on_maybe_in_place`] — safe whether or not that thread is itself
    /// inside a `block_on` (the threading contract).
    fn incorporate(&mut self, ordered: &[crate::intent::WriteIntent]) -> Result<(), IntentError> {
        for intent in ordered {
            block_on_maybe_in_place(
                &self.handle,
                self.appender
                    .append_intent(intent.append.clone(), intent.perspective_seq),
            )
            .map_err(|e| IntentError::Incorporate(e.to_string()))?;
            self.max_incorporated = Some(intent.perspective_seq);
        }
        Ok(())
    }
}

impl IntentLogAppender for crate::raft::openraft_store::OpenRaftLogStore {
    async fn append_intent(
        &self,
        req: AppendChunkAndDeltaRequest,
        seq: PerspectiveSeq,
    ) -> Result<(), LogError> {
        // Drop the assigned SequenceNumber — the sink only needs success/fail;
        // the F-2 floor is recorded inside the state machine apply, not here.
        crate::raft::openraft_store::OpenRaftLogStore::append_intent(self, req, seq)
            .await
            .map(|_seq| ())
    }

    async fn max_incorporated_seq(&self) -> Option<HybridLogicalClock> {
        crate::raft::openraft_store::OpenRaftLogStore::max_incorporated_seq(self).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use kiseki_common::ids::{NodeId, OrgId, ShardId};
    use kiseki_common::locks::LockOrDie;
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, WallTime};

    use crate::delta::OperationType;
    use crate::intent::{IdempotencyKey, InMemIntentStore, IntentStore, PutOutcome, WriteIntent};
    use crate::intent_committer::Committer;
    use crate::traits::AppendDeltaRequest;

    fn seq(physical_ms: u64, logical: u32, node: u64) -> PerspectiveSeq {
        PerspectiveSeq(HybridLogicalClock {
            physical_ms,
            logical,
            node_id: NodeId(node),
        })
    }

    fn intent(s: PerspectiveSeq, key: Option<IdempotencyKey>) -> WriteIntent {
        WriteIntent {
            perspective_seq: s,
            idempotency_key: key,
            append: AppendChunkAndDeltaRequest {
                delta: AppendDeltaRequest {
                    shard_id: ShardId(uuid::Uuid::from_u128(1)),
                    tenant_id: OrgId(uuid::Uuid::from_u128(100)),
                    operation: OperationType::Create,
                    timestamp: DeltaTimestamp {
                        hlc: s.0,
                        wall: WallTime {
                            millis_since_epoch: s.0.physical_ms,
                            timezone: "UTC".into(),
                        },
                        quality: ClockQuality::Ntp,
                    },
                    hashed_key: [0u8; 32],
                    chunk_refs: vec![],
                    payload: vec![],
                    has_inline_data: false,
                },
                new_chunks: vec![],
            },
        }
    }

    /// A fake [`IntentLogAppender`]: records every append in order and reports a
    /// configurable initial `max_incorporated_seq`. No raft node needed.
    struct RecordingAppender {
        appends: Mutex<Vec<(PerspectiveSeq, AppendChunkAndDeltaRequest)>>,
        max_inc: Option<HybridLogicalClock>,
    }

    impl RecordingAppender {
        fn new(max_inc: Option<HybridLogicalClock>) -> Self {
            Self {
                appends: Mutex::new(Vec::new()),
                max_inc,
            }
        }

        fn recorded_seqs(&self) -> Vec<PerspectiveSeq> {
            self.appends
                .lock()
                .lock_or_die("recording_appender.appends")
                .iter()
                .map(|(s, _)| *s)
                .collect()
        }
    }

    impl IntentLogAppender for RecordingAppender {
        async fn append_intent(
            &self,
            req: AppendChunkAndDeltaRequest,
            seq: PerspectiveSeq,
        ) -> Result<(), LogError> {
            self.appends
                .lock()
                .lock_or_die("recording_appender.appends")
                .push((seq, req));
            Ok(())
        }

        async fn max_incorporated_seq(&self) -> Option<HybridLogicalClock> {
            self.max_inc
        }
    }

    fn fill(store: &InMemIntentStore, seqs: &[PerspectiveSeq]) {
        for s in seqs {
            assert_eq!(store.put(intent(*s, None)).unwrap(), PutOutcome::Recorded);
        }
    }

    #[test]
    fn sink_incorporates_in_perspective_order() {
        // A local runtime: block_on works because the #[test] thread is not
        // itself a tokio worker (the threading contract the doc spells out).
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let store = InMemIntentStore::new();
        // Inserted out of order; the appender must see ascending perspective seq.
        fill(&store, &[seq(3, 0, 1), seq(1, 0, 1), seq(2, 0, 1)]);

        let mut sink = RaftLogIncorporationSink::new(RecordingAppender::new(None), handle);
        // All-`None` reports → FullyClosed → no upper bound, incorporate all.
        let n = Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            sink.appender.recorded_seqs(),
            vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)],
            "appender recorded all intents in ascending perspective order"
        );
        // The committer prunes after incorporation.
        assert_eq!(store.pending_len().unwrap(), 0, "store pruned");
    }

    #[test]
    fn sink_recovery_cache_initialized_from_appender() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        // The log already incorporated up to seq(2): the appender reports it,
        // and `new` seeds the cache from it (the recovery path).
        let appender = RecordingAppender::new(Some(seq(2, 0, 1).0));
        let mut sink = RaftLogIncorporationSink::new(appender, handle);
        assert_eq!(
            sink.max_incorporated_seq(),
            Some(seq(2, 0, 1)),
            "recovery cache seeded from the appender's max_incorporated_seq"
        );

        // The store holds seq1, seq2, seq3; the F-2 floor must drop seq1+seq2.
        let store = InMemIntentStore::new();
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);
        let n = Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap();
        assert_eq!(n, 1, "only seq(3) is above the F-2 floor");
        assert_eq!(
            sink.appender.recorded_seqs(),
            vec![seq(3, 0, 1)],
            "only seq(3) appended; seq1+seq2 dropped by the floor"
        );
    }

    #[test]
    fn sink_cache_advances_after_incorporate() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let store = InMemIntentStore::new();
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1), seq(5, 0, 7)]);

        let mut sink = RaftLogIncorporationSink::new(RecordingAppender::new(None), handle);
        assert_eq!(
            sink.max_incorporated_seq(),
            None,
            "empty floor before any run"
        );
        let n = Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap();
        assert_eq!(n, 3);
        // After incorporating up to seqN, the cache reports seqN.
        assert_eq!(
            sink.max_incorporated_seq(),
            Some(seq(5, 0, 7)),
            "cache advanced to the highest incorporated seq"
        );
    }
}
