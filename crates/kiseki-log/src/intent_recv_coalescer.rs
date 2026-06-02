//! Lever 1 (2026-06-02) — receiver-side `intent_put` fan coalescer.
//!
//! Mirror of [`crate::intent_fan_coalescer`] on the RECEIVER side. The
//! 2026-06-02 W12 GCP A/B (#172) measured the producer-side coalescer
//! batching at mean **1.17 intents/fan** because the bench's per-shard
//! arrival rate (~5.3 in-flight × 5.3 ms latency = ~1 PUT/ms/shard)
//! cannot feed batches of 4-8 within a sub-millisecond window.
//!
//! Receiver-side concurrency is much higher: a storage node accepts
//! `intent_put` from every shard-leader it follows. At 6 nodes ×
//! 18 shards × ~5 in-flight per shard, each receiver sees dozens of
//! concurrent RPCs at any moment. That's the right batching point.
//!
//! This module accumulates incoming RPCs' `Vec<WriteIntent>` payloads
//! into a single `store.put_batch` per coalesce window, then distributes
//! per-RPC acks back via the oneshots each dispatcher submitted with.
//! Wire format is unchanged: each RPC still carries `Vec<WireIntent>`
//! in and `Vec<bool>` out — only the *local* batching point changes.
//!
//! ## Why a spawned task and not a Mutex<State>
//!
//! Same reason as the producer-side coalescer: the dispatcher closures
//! just `send`+`await`, no shared mutex on the hot path. The task owns
//! the batch state without locking and uses `tokio::select!` for the
//! intent-count-vs-timeout race cleanly. One task per shard.
//!
//! ## Crash safety
//!
//! `put_batch` is atomic in fjall — either every intent in the
//! coalesced flush commits or none. On error every submitter's
//! oneshot resolves with a non-ack (`Vec<false>`); the producer's
//! `aux.handle_intent_put_total` path encodes that as `ParseError`,
//! which the producer-side coalescer treats as a non-ack and the
//! eventual gateway path retries.

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kiseki_common::ids::ShardId;
use tokio::sync::{mpsc, oneshot};

use crate::intent::{IntentError, IntentStore, WriteIntent};
use crate::intent_metrics;

/// One submitted RPC's payload + ack channel.
struct RecvReq {
    intents: Vec<WriteIntent>,
    submitted_at: Instant,
    /// Resolves with one bool per input intent (true = durable on this
    /// replica). On `put_batch` error, every position is `false`.
    ack: oneshot::Sender<Result<Vec<bool>, IntentError>>,
}

/// Handle for the `INTENT_PUT_TAG` dispatcher to submit incoming
/// `Vec<WireIntent>` payloads and await per-input acks.
#[derive(Clone)]
pub struct IntentRecvCoalescer {
    tx: mpsc::Sender<RecvReq>,
}

/// Configuration for spawning a receiver coalescer — mirrors
/// [`crate::intent_fan_coalescer::CoalescerConfig`].
pub struct RecvConfig {
    /// The shard this coalescer flushes for.
    pub shard_id: ShardId,
    /// Per-shard durable intent store. The flusher calls `put_batch`
    /// directly on this.
    pub store: Arc<dyn IntentStore>,
    /// Max intents per coalesced flush (`KISEKI_INTENT_RECV_BATCH_MAX`).
    pub cap_max: usize,
    /// Max wait from first incoming RPC to flush
    /// (`KISEKI_INTENT_RECV_BATCH_TIMEOUT_US`).
    pub cap_timeout: Duration,
}

/// Spawn the coalescer task on `runtime` and return the submission
/// handle. The task lives until either:
///
/// - The handle (and all clones) are dropped — closes the channel.
/// - The runtime shuts down.
#[must_use]
pub fn spawn(runtime: &tokio::runtime::Handle, cfg: RecvConfig) -> IntentRecvCoalescer {
    let (tx, rx) = mpsc::channel(cfg.cap_max.saturating_mul(4).max(64));
    runtime.spawn(coalescer_loop(cfg, rx));
    IntentRecvCoalescer { tx }
}

impl IntentRecvCoalescer {
    /// Submit one RPC's worth of intents. Returns once the coalesce
    /// window containing this RPC has flushed.
    ///
    /// # Errors
    ///
    /// `IntentError::Fjall` (rendered) if the coalescer task has
    /// stopped (channel closed, runtime shutdown) OR if the underlying
    /// `put_batch` failed for the whole flush. In both cases the
    /// dispatcher should map to `DispatchOutcome::ParseError` so the
    /// producer treats it as a non-ack.
    pub async fn submit(&self, intents: Vec<WriteIntent>) -> Result<Vec<bool>, IntentError> {
        let n = intents.len();
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(RecvReq {
                intents,
                submitted_at: Instant::now(),
                ack: ack_tx,
            })
            .await
            .map_err(|_| IntentError::Fjall("recv coalescer channel closed".into()))?;
        match ack_rx.await {
            Ok(Ok(acks)) => Ok(acks),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // Task dropped without sending — treat as all-non-ack so
                // the producer retries every intent in the original RPC.
                Ok(vec![false; n])
            }
        }
    }
}

async fn coalescer_loop(cfg: RecvConfig, mut rx: mpsc::Receiver<RecvReq>) {
    loop {
        // Block until the first request.
        let Some(first) = rx.recv().await else {
            return;
        };
        let batch_start = first.submitted_at;
        let mut pending: Vec<RecvReq> = Vec::with_capacity(8);
        let mut total_intents = first.intents.len();
        pending.push(first);

        // Accumulate up to cap_max TOTAL intents OR until cap_timeout
        // elapses since the first RPC. A request never spans two
        // flushes — cap_max is a soft cap on the *intent count*, not
        // RPC count, and we always include a full incoming RPC.
        while total_intents < cfg.cap_max {
            let deadline = batch_start + cfg.cap_timeout;
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            tokio::select! {
                next = rx.recv() => {
                    match next {
                        Some(req) => {
                            total_intents += req.intents.len();
                            pending.push(req);
                        }
                        None => break,
                    }
                }
                () = tokio::time::sleep(remaining) => break,
            }
        }

        // Observe per-RPC wait + the flush size BEFORE the commit.
        let flushed_at = Instant::now();
        for req in &pending {
            intent_metrics::observe_intent_recv_coalesce_wait(
                flushed_at.saturating_duration_since(req.submitted_at),
            );
        }
        intent_metrics::observe_intent_recv_batch_size(total_intents);

        flush_pending(&cfg, pending, total_intents);
    }
}

/// (ack-channel, slice-of-`all_intents` belonging to that RPC).
/// Aliased so the `flush_pending` body stays inside the
/// `type_complexity` clippy budget.
type AckBoundary = (
    oneshot::Sender<Result<Vec<bool>, IntentError>>,
    Range<usize>,
);

fn flush_pending(cfg: &RecvConfig, pending: Vec<RecvReq>, total: usize) {
    // Flatten every RPC's intents into one Vec for the single `put_batch`
    // call. Remember each RPC's slice so we can split the result back.
    let mut all_intents: Vec<WriteIntent> = Vec::with_capacity(total);
    let mut boundaries: Vec<AckBoundary> = Vec::with_capacity(pending.len());
    for req in pending {
        let start = all_intents.len();
        all_intents.extend(req.intents);
        let end = all_intents.len();
        boundaries.push((req.ack, start..end));
    }

    // One fjall WAL sync for the whole coalesced flush.
    let put_res = cfg.store.put_batch(all_intents);

    match put_res {
        Ok(outcomes) => {
            debug_assert_eq!(
                outcomes.len(),
                total,
                "put_batch must return one outcome per input"
            );
            for (ack, range) in boundaries {
                // Every PutOutcome (Recorded or Duplicate) means the intent
                // is now durable — ack uniformly.
                let acks: Vec<bool> = outcomes[range].iter().map(|_| true).collect();
                let _ = ack.send(Ok(acks));
            }
        }
        Err(e) => {
            tracing::warn!(
                shard_id = %cfg.shard_id.0,
                error = %e,
                "intent recv coalescer: put_batch failed; non-acking the flush",
            );
            // Clone the error per-RPC. IntentError is not Clone, so render
            // to string and reconstruct as the Fjall variant.
            let err_str = e.to_string();
            for (ack, _range) in boundaries {
                let _ = ack.send(Err(IntentError::Fjall(err_str.clone())));
            }
        }
    }
}

/// Read the receiver coalescer batch-max from
/// `KISEKI_INTENT_RECV_BATCH_MAX`. Defaults to **128** — much higher
/// than the producer-side default because the receiver naturally
/// aggregates much higher concurrency.
#[must_use]
pub fn recv_batch_max_from_env() -> usize {
    std::env::var("KISEKI_INTENT_RECV_BATCH_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n >= 1)
        .unwrap_or(128)
}

/// Read the receiver coalescer batch-timeout from
/// `KISEKI_INTENT_RECV_BATCH_TIMEOUT_US`. Defaults to **100 µs** — the
/// same value as the producer-side default after Lever 2 (#174). The
/// receiver typically fills the cap well before the timeout fires.
#[must_use]
pub fn recv_batch_timeout_from_env() -> Duration {
    let us = std::env::var("KISEKI_INTENT_RECV_BATCH_TIMEOUT_US")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n >= 1)
        .unwrap_or(100);
    Duration::from_micros(us)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::intent::{InMemIntentStore, PerspectiveSeq};
    use crate::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};
    use kiseki_common::ids::{NodeId, OrgId};
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};

    fn seq(physical_ms: u64, logical: u32, node: u64) -> PerspectiveSeq {
        PerspectiveSeq(HybridLogicalClock {
            physical_ms,
            logical,
            node_id: NodeId(node),
        })
    }

    fn intent(s: PerspectiveSeq) -> WriteIntent {
        WriteIntent {
            perspective_seq: s,
            idempotency_key: None,
            append: AppendChunkAndDeltaRequest {
                delta: AppendDeltaRequest {
                    shard_id: ShardId(uuid::Uuid::from_u128(1)),
                    tenant_id: OrgId(uuid::Uuid::from_u128(100)),
                    operation: crate::delta::OperationType::Create,
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

    /// One RPC carrying N intents is acked correctly even with no
    /// partners arriving — the timer flushes it.
    #[tokio::test(flavor = "current_thread")]
    async fn single_rpc_flushes_on_timeout() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            RecvConfig {
                shard_id: ShardId(uuid::Uuid::from_u128(1)),
                store: Arc::clone(&store),
                cap_max: 128,
                cap_timeout: Duration::from_micros(50),
            },
        );
        let intents = vec![intent(seq(1, 0, 1)), intent(seq(1, 1, 1))];
        let acks = coalescer.submit(intents).await.unwrap();
        assert_eq!(acks, vec![true, true]);
        assert_eq!(store.pending_len().unwrap(), 2);
    }

    /// Multiple concurrent RPCs are coalesced into one `put_batch` —
    /// every submitter sees the per-RPC ack slice in input order, and
    /// the store ends with all intents from all RPCs present.
    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_rpcs_coalesce_into_one_put_batch() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        // Long timeout so we test the cap-reached path. cap_max=8 with
        // 4 RPCs × 2 intents each = 8 intents = exactly cap.
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            RecvConfig {
                shard_id: ShardId(uuid::Uuid::from_u128(2)),
                store: Arc::clone(&store),
                cap_max: 8,
                cap_timeout: Duration::from_secs(5),
            },
        );
        let mut handles = Vec::new();
        for rpc_idx in 0..4u32 {
            let c = coalescer.clone();
            handles.push(tokio::spawn(async move {
                let intents = vec![
                    intent(seq(2, rpc_idx * 2, 1)),
                    intent(seq(2, rpc_idx * 2 + 1, 1)),
                ];
                c.submit(intents).await
            }));
        }
        for h in handles {
            let acks = h.await.unwrap().unwrap();
            assert_eq!(acks.len(), 2);
            assert!(acks.iter().all(|b| *b));
        }
        assert_eq!(
            store.pending_len().unwrap(),
            8,
            "all four RPCs landed via one coalesced put_batch"
        );
    }

    /// Channel-closed shutdown: dropping the handle ends the task
    /// cleanly AFTER the in-flight flush completes.
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_handle_shuts_down_task() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            RecvConfig {
                shard_id: ShardId(uuid::Uuid::from_u128(3)),
                store: Arc::clone(&store),
                cap_max: 128,
                cap_timeout: Duration::from_micros(50),
            },
        );
        let acks = coalescer.submit(vec![intent(seq(3, 0, 1))]).await.unwrap();
        assert_eq!(acks, vec![true]);
        drop(coalescer);
        tokio::task::yield_now().await;
        assert_eq!(store.pending_len().unwrap(), 1);
    }
}
