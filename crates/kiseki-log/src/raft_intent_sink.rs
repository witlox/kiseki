//! ADR-047 PART 8 — the real [`IncorporationSink`] over the live Raft log.
//!
//! Drains committed intents into the log via [`IntentLogAppender::append_intents`]
//! — one batched [`crate::raft_store::LogCommand::IncorporateIntents`] per
//! `incorporate` call. PART 8 deletes the global-floor read from this layer;
//! the SM apply gate (`recent_incorporated` + ancient cutoff) is the
//! authoritative dedup.
//!
//! The async log API is abstracted behind [`IntentLogAppender`] so the sink is
//! unit-testable without a live raft node (the tests fake the appender).
//! Dispatch is generic, not `dyn` — the trait's `async fn` has no object-safe
//! form and none is needed.

use tokio::runtime::Handle;

use crate::error::LogError;
use crate::intent::IntentError;
use crate::intent_committer::IncorporationSink;
use crate::raft_store::IncorporateItem;

/// The async seam the [`RaftLogIncorporationSink`] needs from the log:
/// append a batched [`crate::raft_store::LogCommand::IncorporateIntents`].
///
/// Implemented by [`crate::raft::openraft_store::OpenRaftLogStore`] in
/// production; faked in tests so the sink is exercised without a live raft
/// node. Used via **generic dispatch** (`A: IntentLogAppender`), so the
/// `async fn` form is fine — there is no `dyn IntentLogAppender`.
#[allow(async_fn_in_trait)]
pub trait IntentLogAppender: Send + Sync {
    /// Append a batched `IncorporateIntents` command to the Raft log.
    /// All items in `items` are submitted as a single Raft round; the SM
    /// applies each through the PART 8 per-item gate.
    ///
    /// # Errors
    /// [`LogError`] on any append / Raft client-write failure.
    async fn append_intents(&self, items: Vec<IncorporateItem>) -> Result<(), LogError>;
}

/// The real [`IncorporationSink`]: drains committed intents into the Raft log
/// via [`IntentLogAppender::append_intents`] in batches.
///
/// Bridges the **synchronous** [`IncorporationSink`] seam (the committer drives
/// it synchronously) to the **async** log API via [`Handle::block_on`].
///
/// # THREADING CONTRACT
/// The per-shard committer task that drives this sink **MUST** run on its own
/// dedicated thread — a `std::thread` holding a runtime [`Handle`] — **NOT** a
/// shared tokio worker thread. Two call shapes are supported, picked at
/// runtime:
///
/// - **Outside any runtime context** (a bare `std::thread`, or a `#[test]`
///   thread): a plain [`Handle::block_on`].
/// - **Inside a runtime context** (the `LeaderSink` committer supervisor thread
///   drives the drain loop via `Handle::block_on`, so the sync `drain_local` →
///   `incorporate` runs *while* a `block_on` is active):
///   [`tokio::task::block_in_place`] wraps the inner `block_on`, telling tokio
///   this worker is about to block so it can move other tasks off it.
///   `block_in_place` requires the multi-threaded Raft runtime.
///
/// A plain nested `Handle::block_on` would panic ("Cannot start a runtime from
/// within a runtime"); the [`block_on_maybe_in_place`] helper avoids that.
pub struct RaftLogIncorporationSink<A: IntentLogAppender> {
    appender: A,
    handle: Handle,
}

impl<A: IntentLogAppender> RaftLogIncorporationSink<A> {
    /// Build a sink over `appender`.
    #[must_use]
    pub fn new(appender: A, handle: Handle) -> Self {
        Self { appender, handle }
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
    /// Append `ordered` (already ascending) into the Raft log as ONE batched
    /// `IncorporateIntents` command. The committer caps each `ordered` slice
    /// at `DRAIN_BATCH_CAP` (PART 8 §U) so a single Raft round absorbs at
    /// most that many items; the SM applies each through the per-item gate.
    fn incorporate(&mut self, ordered: &[crate::intent::WriteIntent]) -> Result<(), IntentError> {
        if ordered.is_empty() {
            return Ok(());
        }
        let items: Vec<IncorporateItem> = ordered
            .iter()
            .map(|intent| IncorporateItem {
                tenant_id_bytes: *intent.append.delta.tenant_id.0.as_bytes(),
                operation: crate::raft::openraft_store::op_to_u8_pub(intent.append.delta.operation),
                hashed_key: intent.append.delta.hashed_key,
                chunk_refs: intent.append.delta.chunk_refs.iter().map(|c| c.0).collect(),
                payload: intent.append.delta.payload.clone(),
                has_inline_data: intent.append.delta.has_inline_data,
                new_chunks: intent.append.new_chunks.clone(),
                perspective_seq: intent.perspective_seq.0,
                inline_payloads: intent
                    .append
                    .inline_payloads
                    .iter()
                    .map(|(c, b)| (c.0, b.clone()))
                    .collect(),
            })
            .collect();
        block_on_maybe_in_place(&self.handle, self.appender.append_intents(items))
            .map_err(|e| IntentError::Incorporate(e.to_string()))?;
        Ok(())
    }
}

impl IntentLogAppender for crate::raft::openraft_store::OpenRaftLogStore {
    async fn append_intents(&self, items: Vec<IncorporateItem>) -> Result<(), LogError> {
        crate::raft::openraft_store::OpenRaftLogStore::append_intents(self, items)
            .await
            .map(|_seq| ())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use kiseki_common::ids::{NodeId, OrgId, ShardId};
    use kiseki_common::locks::LockOrDie;
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};

    use crate::delta::OperationType;
    use crate::intent::{
        IdempotencyKey, InMemIntentStore, IntentStore, PerspectiveSeq, PutOutcome, WriteIntent,
    };
    use crate::intent_committer::Committer;
    use crate::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};

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
                inline_payloads: vec![],
            },
        }
    }

    /// A fake [`IntentLogAppender`]: records each batched append in order. No
    /// raft node needed. PART 8 — no floor cache; the SM gate is authoritative.
    struct RecordingAppender {
        appends: Mutex<Vec<Vec<IncorporateItem>>>,
    }

    impl RecordingAppender {
        fn new() -> Self {
            Self {
                appends: Mutex::new(Vec::new()),
            }
        }

        fn batches(&self) -> Vec<Vec<HybridLogicalClock>> {
            self.appends
                .lock()
                .lock_or_die("recording_appender.appends")
                .iter()
                .map(|b| b.iter().map(|i| i.perspective_seq).collect())
                .collect()
        }
    }

    impl IntentLogAppender for RecordingAppender {
        async fn append_intents(&self, items: Vec<IncorporateItem>) -> Result<(), LogError> {
            self.appends
                .lock()
                .lock_or_die("recording_appender.appends")
                .push(items);
            Ok(())
        }
    }

    fn fill(store: &InMemIntentStore, seqs: &[PerspectiveSeq]) {
        for s in seqs {
            assert_eq!(store.put(intent(*s, None)).unwrap(), PutOutcome::Recorded);
        }
    }

    #[test]
    fn sink_incorporates_in_perspective_order_as_one_batch() {
        // A local runtime: block_on works because the #[test] thread is not
        // itself a tokio worker (the threading contract the doc spells out).
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let store = InMemIntentStore::new();
        // Inserted out of order; the appender must see ascending perspective seq.
        fill(&store, &[seq(3, 0, 1), seq(1, 0, 1), seq(2, 0, 1)]);

        let mut sink = RaftLogIncorporationSink::new(RecordingAppender::new(), handle);
        let n = Committer::run(&store, &mut sink).unwrap();
        assert_eq!(n, 3);
        let batches = sink.appender.batches();
        assert_eq!(batches.len(), 1, "all 3 items fit in one batch under cap");
        assert_eq!(
            batches[0],
            vec![seq(1, 0, 1).0, seq(2, 0, 1).0, seq(3, 0, 1).0],
            "appender received ascending perspective order",
        );
    }

    #[test]
    fn sink_runs_with_empty_store_is_noop() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let store = InMemIntentStore::new();
        let mut sink = RaftLogIncorporationSink::new(RecordingAppender::new(), handle);
        assert_eq!(Committer::run(&store, &mut sink).unwrap(), 0);
        assert!(sink.appender.batches().is_empty());
    }
}
