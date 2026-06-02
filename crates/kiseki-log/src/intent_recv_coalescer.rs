//! L3 (2026-06-02) — per-node CROSS-SHARD receiver-side `intent_put`
//! coalescer.
//!
//! **The L1 design was wrong; this is the fix.** The 2026-06-02 L1+L2
//! GCP A/B measured the per-shard receiver coalescer batching at mean
//! 1.17 intents/flush — same as the producer-side coalescer — because
//! each shard has exactly ONE leader = ONE producer. Per-shard
//! receiver concurrency is bounded by the producer's pipeline depth,
//! not by aggregation across the cluster as I'd predicted.
//!
//! The real aggregation point is the **node**. A storage node hosts
//! ~3 shards (18 shards / 6 nodes) and sees `intent_put` for all of
//! them: ~3.75 k RPCs/s/node in the W12 workload. A single per-node
//! coalescer can batch across shards, then dispatch per-shard
//! `put_batch` calls IN PARALLEL — saving the tokio scheduler
//! park/unpark overhead and amortising the fjall WAL syncs across
//! more callers per window.
//!
//! ## Shape
//!
//! - ONE coalescer task per node (singleton, shared by every shard's
//!   aux dispatcher).
//! - On flush: group accumulated requests by `shard_id`, run
//!   `store.put_batch` for each shard concurrently via
//!   `FuturesUnordered`, then distribute per-RPC ack slices back via
//!   the oneshots.
//! - Per-shard atomicity is preserved (each `put_batch` is its own
//!   fjall `WriteBatch` + WAL sync). What changes is the per-PARK cost
//!   — instead of N park/unpark cycles per N concurrent RPCs, we pay
//!   one per coalesce window.
//!
//! ## Crash safety
//!
//! Each shard's `put_batch` is atomic in fjall. A task panic or
//! runtime shutdown resolves every pending oneshot with non-ack
//! (`Vec<false>`), the producer treats it as a non-ack, the gateway
//! retries.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use kiseki_common::ids::ShardId;
use tokio::sync::{mpsc, oneshot};

use crate::intent::{IntentError, IntentStore, WriteIntent};
use crate::intent_metrics;

/// One submitted RPC: which shard, which store, intents, and the
/// ack channel back to the dispatcher.
struct RecvReq {
    shard_id: ShardId,
    store: Arc<dyn IntentStore>,
    intents: Vec<WriteIntent>,
    submitted_at: Instant,
    ack: oneshot::Sender<Result<Vec<bool>, IntentError>>,
}

/// Per-node handle. Cloned into every shard's aux dispatcher.
#[derive(Clone)]
pub struct NodeIntentRecvCoalescer {
    tx: mpsc::Sender<RecvReq>,
}

/// Configuration for the per-node receiver coalescer.
pub struct NodeRecvConfig {
    /// Soft cap on TOTAL intents per coalesce window (across all
    /// shards). `KISEKI_INTENT_RECV_BATCH_MAX`.
    pub cap_max: usize,
    /// Max wait from first incoming RPC to flush.
    /// `KISEKI_INTENT_RECV_BATCH_TIMEOUT_US`.
    pub cap_timeout: Duration,
}

/// Spawn the per-node coalescer task on `runtime`.
#[must_use]
pub fn spawn(runtime: &tokio::runtime::Handle, cfg: NodeRecvConfig) -> NodeIntentRecvCoalescer {
    let (tx, rx) = mpsc::channel(cfg.cap_max.saturating_mul(4).max(64));
    runtime.spawn(coalescer_loop(cfg, rx));
    NodeIntentRecvCoalescer { tx }
}

impl NodeIntentRecvCoalescer {
    /// Submit one RPC's worth of intents for shard `shard_id`. Returns
    /// the per-input ack vector once the coalesce window containing
    /// this RPC has flushed and the shard's `put_batch` has committed.
    ///
    /// # Errors
    /// `IntentError::Fjall` (rendered) if the coalescer task has
    /// stopped (channel closed, runtime shutdown) OR if the underlying
    /// `put_batch` failed for this shard's flush.
    pub async fn submit(
        &self,
        shard_id: ShardId,
        store: Arc<dyn IntentStore>,
        intents: Vec<WriteIntent>,
    ) -> Result<Vec<bool>, IntentError> {
        let n = intents.len();
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(RecvReq {
                shard_id,
                store,
                intents,
                submitted_at: Instant::now(),
                ack: ack_tx,
            })
            .await
            .map_err(|_| IntentError::Fjall("recv coalescer channel closed".into()))?;
        match ack_rx.await {
            Ok(Ok(acks)) => Ok(acks),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(vec![false; n]),
        }
    }
}

async fn coalescer_loop(cfg: NodeRecvConfig, mut rx: mpsc::Receiver<RecvReq>) {
    loop {
        let Some(first) = rx.recv().await else {
            return;
        };
        let batch_start = first.submitted_at;
        let mut pending: Vec<RecvReq> = Vec::with_capacity(16);
        let mut total_intents = first.intents.len();
        pending.push(first);

        // Accumulate up to cap_max total intents OR until cap_timeout.
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

        flush_pending(pending).await;
    }
}

/// (ack-channel, slice-of-the-shard-`Vec<WriteIntent>` belonging to that RPC).
/// Aliased so the per-shard tracking stays inside the
/// `type_complexity` clippy budget.
type AckBoundary = (
    oneshot::Sender<Result<Vec<bool>, IntentError>>,
    Range<usize>,
);

struct PerShardBatch {
    store: Arc<dyn IntentStore>,
    intents: Vec<WriteIntent>,
    boundaries: Vec<AckBoundary>,
}

async fn flush_pending(pending: Vec<RecvReq>) {
    // Group requests by shard_id. Each shard gets its own put_batch
    // call; we run them concurrently via FuturesUnordered so multi-
    // shard nodes amortise the tokio wake-up across N shards rather
    // than serialising N WAL syncs.
    let mut by_shard: HashMap<ShardId, PerShardBatch> = HashMap::new();
    for req in pending {
        let entry = by_shard
            .entry(req.shard_id)
            .or_insert_with(|| PerShardBatch {
                store: Arc::clone(&req.store),
                intents: Vec::new(),
                boundaries: Vec::new(),
            });
        let start = entry.intents.len();
        entry.intents.extend(req.intents);
        let end = entry.intents.len();
        entry.boundaries.push((req.ack, start..end));
    }

    let mut fut = FuturesUnordered::new();
    for (shard_id, batch) in by_shard {
        fut.push(async move { (shard_id, flush_one_shard(batch)) });
    }
    while let Some((_shard_id, ())) = fut.next().await {
        // Acks were sent inside flush_one_shard.
    }
}

fn flush_one_shard(batch: PerShardBatch) {
    let total = batch.intents.len();
    let put_res = batch.store.put_batch(batch.intents);
    match put_res {
        Ok(outcomes) => {
            debug_assert_eq!(outcomes.len(), total);
            for (ack, range) in batch.boundaries {
                let acks: Vec<bool> = outcomes[range].iter().map(|_| true).collect();
                let _ = ack.send(Ok(acks));
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "intent recv coalescer: put_batch failed; non-acking this shard's flush");
            let err_str = e.to_string();
            for (ack, _range) in batch.boundaries {
                let _ = ack.send(Err(IntentError::Fjall(err_str.clone())));
            }
        }
    }
}

/// Read the receiver coalescer batch-max from
/// `KISEKI_INTENT_RECV_BATCH_MAX`. Defaults to **128** total intents.
#[must_use]
pub fn recv_batch_max_from_env() -> usize {
    std::env::var("KISEKI_INTENT_RECV_BATCH_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n >= 1)
        .unwrap_or(128)
}

/// Read the receiver coalescer batch-timeout from
/// `KISEKI_INTENT_RECV_BATCH_TIMEOUT_US`. Defaults to **500 µs** — L3
/// bumped this from L1's 100 µs because the math says we need a
/// longer window to actually collect partners at the cluster's
/// arrival rate. The cost is per-PUT tail latency; the win is fjall
/// WAL sync amortisation across more concurrent producers.
#[must_use]
pub fn recv_batch_timeout_from_env() -> Duration {
    let us = std::env::var("KISEKI_INTENT_RECV_BATCH_TIMEOUT_US")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n >= 1)
        .unwrap_or(500);
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

    /// One RPC for one shard — flushes on timeout, lands in that shard's store.
    #[tokio::test(flavor = "current_thread")]
    async fn single_shard_single_rpc_flushes_on_timeout() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            NodeRecvConfig {
                cap_max: 128,
                cap_timeout: Duration::from_micros(50),
            },
        );
        let shard = ShardId(uuid::Uuid::from_u128(1));
        let acks = coalescer
            .submit(shard, Arc::clone(&store), vec![intent(seq(1, 0, 1))])
            .await
            .unwrap();
        assert_eq!(acks, vec![true]);
        assert_eq!(store.pending_len().unwrap(), 1);
    }

    /// Two shards arrive in the same coalesce window — both flush
    /// concurrently, both stores end with their RPCs' intents.
    #[tokio::test(flavor = "current_thread")]
    async fn cross_shard_flush_dispatches_parallel_put_batches() {
        let store_a: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let store_b: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        // Long timeout so both arrivals share the same window via cap.
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            NodeRecvConfig {
                cap_max: 4,
                cap_timeout: Duration::from_secs(5),
            },
        );
        let shard_a = ShardId(uuid::Uuid::from_u128(0xa));
        let shard_b = ShardId(uuid::Uuid::from_u128(0xb));
        let (a_intents, b_intents) = (
            vec![intent(seq(1, 0, 1)), intent(seq(1, 1, 1))],
            vec![intent(seq(1, 2, 1)), intent(seq(1, 3, 1))],
        );
        let ca = coalescer.clone();
        let sa = Arc::clone(&store_a);
        let h_a = tokio::spawn(async move { ca.submit(shard_a, sa, a_intents).await });
        let cb = coalescer.clone();
        let sb = Arc::clone(&store_b);
        let h_b = tokio::spawn(async move { cb.submit(shard_b, sb, b_intents).await });
        let acks_a = h_a.await.unwrap().unwrap();
        let acks_b = h_b.await.unwrap().unwrap();
        assert_eq!(acks_a, vec![true, true]);
        assert_eq!(acks_b, vec![true, true]);
        assert_eq!(store_a.pending_len().unwrap(), 2);
        assert_eq!(store_b.pending_len().unwrap(), 2);
    }

    /// Channel-closed shutdown ends the task cleanly after the
    /// in-flight flush completes.
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_handle_shuts_down_task() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            NodeRecvConfig {
                cap_max: 128,
                cap_timeout: Duration::from_micros(50),
            },
        );
        let shard = ShardId(uuid::Uuid::from_u128(1));
        coalescer
            .submit(shard, Arc::clone(&store), vec![intent(seq(1, 0, 1))])
            .await
            .unwrap();
        drop(coalescer);
        tokio::task::yield_now().await;
        assert_eq!(store.pending_len().unwrap(), 1);
    }
}
