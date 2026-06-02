//! L4 (2026-06-02) — producer-side intent-fan coalescer, Mutex+Notify shape.
//!
//! Replaces the W12 spawned-task / mpsc design. The 2026-06-02 W12 GCP
//! A/B measured `kiseki_intent_coalesce_wait_seconds` at ~2.1 ms with
//! a 500 µs configured timeout. The L1/L2/L3 experiments confirmed
//! the floor is tokio scheduler park/unpark latency (~1.6 ms), not the
//! configured timeout or the per-shard vs per-node placement.
//!
//! This redesign attacks that floor by removing the spawned task
//! entirely. The first submitter to find no active flusher *becomes*
//! the flusher: it does the timeout-wait inline in its own task, then
//! runs the local `put_batch` + fan + ack distribution synchronously.
//!
//! ## Submitter paths
//!
//! - **Flusher path** (first arrival in a fresh batch):
//!   1. Acquire mutex, push intent, set `flusher_running = true`.
//!   2. Drop mutex.
//!   3. `select!` between `sleep(deadline)` and `cap_reached.notified()`.
//!   4. Re-acquire mutex, take pending batch, clear `flusher_running`.
//!   5. Drop mutex.
//!   6. Run local `put_batch` + fan; per-input ack via each
//!      submitter's oneshot.
//!   7. Await own oneshot for the result. (Note: the flusher sends
//!      itself an ack as part of step 6.)
//!
//! - **Waiter path** (joins an existing batch):
//!   1. Acquire mutex, push intent.
//!   2. If `pending.len() >= cap_max`, signal `cap_reached.notify_one()`.
//!   3. Drop mutex.
//!   4. Await own oneshot for the result.
//!
//! ## Why this saves on the W12 floor
//!
//! - **No spawned task** = no per-shard task spawn at creation, no
//!   per-batch mpsc.send → recv park/unpark cycle. The flusher runs
//!   the timer in its own already-running task.
//! - **`Notify::notify_one`** to break out of the timer on cap-reached
//!   is cheaper than mpsc-send + select-unpark.
//! - **Mutex<State> on the hot path** is ~10 ns uncontended (std
//!   mutex). The cross-submitter coordination becomes a quick
//!   lock-push-drop on the waiter path.
//!
//! ## Crash safety
//!
//! Unchanged from W12. `put_batch` is atomic; the local copy commits
//! before the fan starts. A panic mid-flush leaves submitters'
//! oneshots dropped → they see `Err(Unavailable)`; the gateway
//! retries.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kiseki_common::ids::{NodeId, ShardId};
use kiseki_common::locks::LockOrDie;
use kiseki_raft::tcp_transport::rpc_call;
use tokio::sync::{oneshot, Notify};

use crate::error::LogError;
use crate::intent::{IntentStore, WriteIntent};
use crate::intent_metrics;
use crate::intent_sync::{WireIntent, INTENT_PUT_TAG};

/// One submitted PUT awaiting a coalesced ack.
struct CoalesceReq {
    intent: WriteIntent,
    submitted_at: Instant,
    /// Resolves when the batch this intent belongs to has finished
    /// its fan. `Ok(())` once durable copies reach `min_acks` for this
    /// intent; otherwise `Err(QuorumLost)` or `Err(Unavailable)`.
    ack: oneshot::Sender<Result<(), LogError>>,
}

/// Shared mutable state across all submitters for this shard.
#[derive(Default)]
struct State {
    /// PUTs queued for the current batch.
    pending: Vec<CoalesceReq>,
    /// True while a flusher is waiting for the timer / cap or running
    /// the fan. A new submitter that finds this `true` joins the
    /// existing batch as a waiter.
    flusher_running: bool,
}

/// Handle for `put_intent_and_fan` to submit intents and await acks.
#[derive(Clone)]
pub struct IntentFanCoalescer {
    inner: Arc<CoalescerInner>,
}

struct CoalescerInner {
    cfg: CoalescerConfig,
    state: Mutex<State>,
    /// Waiter submitters signal this when they push an intent that
    /// brings pending up to `cap_max`. The flusher's `select!`
    /// listens on this to break out of the deadline `sleep` early.
    cap_reached: Notify,
}

/// Per-shard resolver closure: returns the current voter peer set
/// (`(NodeId, addr)`) excluding the local node, AND the current
/// leader id (if known). Computed fresh on every flush so a recent
/// membership change or leadership move is honoured without restart.
pub type PeerLeaderResolver = Arc<dyn Fn() -> (Vec<(NodeId, String)>, Option<u64>) + Send + Sync>;

/// Configuration for the coalescer — same fields as the W12 version
/// so wiring sites in `RaftShardStore` don't change.
pub struct CoalescerConfig {
    /// The shard this coalescer fans for. One coalescer per shard.
    pub shard_id: ShardId,
    /// Local node id — drives the `leader_is_local` decision per flush.
    pub local_node: NodeId,
    /// Per-shard durable intent store.
    pub store: Arc<dyn IntentStore>,
    /// Live resolver for the voter peer set + current leader.
    pub resolver: PeerLeaderResolver,
    /// Quorum threshold — `Ok` only when durable copies (local + peer
    /// acks) reach this.
    pub min_acks: usize,
    /// Max intents per batch (`KISEKI_INTENT_FAN_BATCH_MAX`).
    pub cap_max: usize,
    /// Max wait from first submission to flush
    /// (`KISEKI_INTENT_FAN_BATCH_TIMEOUT_US`).
    pub cap_timeout: Duration,
    /// Per-peer RPC timeout.
    pub peer_rpc_timeout: Duration,
}

/// Create a new coalescer for `cfg`. Unlike W12 there is **no spawned
/// task** — the first submitter to arrive becomes the flusher inline.
///
/// The `runtime` argument is kept for API compatibility with the W12
/// call site (`RaftShardStore::spawn_intent_fan_coalescer`); it is
/// no longer used because nothing is spawned at construction time.
#[must_use]
pub fn spawn(_runtime: &tokio::runtime::Handle, cfg: CoalescerConfig) -> IntentFanCoalescer {
    IntentFanCoalescer {
        inner: Arc::new(CoalescerInner {
            cfg,
            state: Mutex::new(State::default()),
            cap_reached: Notify::new(),
        }),
    }
}

impl IntentFanCoalescer {
    /// Submit one intent for fanning. Returns once the batch this
    /// intent belongs to has finished and the per-intent ack count
    /// has been compared against `min_acks`.
    ///
    /// # Errors
    /// [`LogError::Unavailable`] if the coalescer dropped the ack
    /// channel mid-flush (panic / runtime shutdown). [`LogError::
    /// QuorumLost`] when the batch's durable copies fall short of
    /// `min_acks`.
    pub async fn submit(&self, intent: WriteIntent) -> Result<(), LogError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let req = CoalesceReq {
            intent,
            submitted_at: Instant::now(),
            ack: ack_tx,
        };

        // Push under the mutex; decide flusher vs waiter.
        let im_flusher = {
            let mut s = self
                .inner
                .state
                .lock()
                .lock_or_die("intent_fan_coalescer.state");
            s.pending.push(req);
            if s.flusher_running {
                // Cap-reached short-circuit for the flusher.
                if s.pending.len() >= self.inner.cfg.cap_max {
                    drop(s);
                    self.inner.cap_reached.notify_one();
                }
                false
            } else {
                s.flusher_running = true;
                true
            }
        };

        if im_flusher {
            run_flush_cycle(Arc::clone(&self.inner)).await;
        }

        ack_rx.await.unwrap_or(Err(LogError::Unavailable))
    }
}

async fn run_flush_cycle(inner: Arc<CoalescerInner>) {
    // Wait for either the per-batch timeout OR a waiter signalling
    // that the cap has been reached.
    let deadline = tokio::time::Instant::now() + inner.cfg.cap_timeout;
    tokio::select! {
        () = tokio::time::sleep_until(deadline) => {}
        () = inner.cap_reached.notified() => {}
    }

    // Take the accumulated batch and re-arm the slot for the next one.
    let batch: Vec<CoalesceReq> = {
        let mut s = inner.state.lock().lock_or_die("intent_fan_coalescer.state");
        let taken = std::mem::take(&mut s.pending);
        s.flusher_running = false;
        taken
    };

    // Observe the per-intent wait + batch size BEFORE the flush.
    let flushed_at = Instant::now();
    for req in &batch {
        intent_metrics::observe_coalesce_wait(
            flushed_at.saturating_duration_since(req.submitted_at),
        );
    }
    intent_metrics::observe_intent_put_batch_size(batch.len());

    flush_batch(&inner.cfg, batch).await;
}

async fn flush_batch(cfg: &CoalescerConfig, batch: Vec<CoalesceReq>) {
    use futures::StreamExt;

    // 1. Local durable copy (one fjall batch, one WAL sync).
    let intents: Vec<WriteIntent> = batch.iter().map(|r| r.intent.clone()).collect();
    let put_res = cfg.store.put_batch(intents);
    if let Err(e) = put_res {
        tracing::warn!(
            shard_id = %cfg.shard_id.0,
            error = %e,
            "intent fan coalescer: local put_batch failed; refusing batch",
        );
        for req in batch {
            let _ = req.ack.send(Err(LogError::Unavailable));
        }
        return;
    }
    let local_acks: usize = 1;

    // 2. Fast path — single-copy quorum.
    if local_acks >= cfg.min_acks {
        for req in batch {
            let _ = req.ack.send(Ok(()));
        }
        return;
    }

    // 3. Resolve peers + leader.
    let (peers, leader_id) = (cfg.resolver)();
    let leader_is_local = leader_id == Some(cfg.local_node.0);
    let wire_batch: Vec<WireIntent> = batch.iter().map(|r| WireIntent::from(&r.intent)).collect();

    // 4. Leader-first (MF-3 no-orphan).
    let mut peer_acks: usize = 0;
    if !leader_is_local {
        if let Some(lid) = leader_id {
            if let Some((node_id, addr)) = peers.iter().find(|(n, _)| n.0 == lid).cloned() {
                let acked = fan_one_batch(
                    node_id,
                    addr,
                    cfg.shard_id,
                    wire_batch.clone(),
                    cfg.peer_rpc_timeout,
                )
                .await;
                if acked {
                    peer_acks += 1;
                    if local_acks + peer_acks >= cfg.min_acks {
                        for req in batch {
                            let _ = req.ack.send(Ok(()));
                        }
                        return;
                    }
                }
            }
        }
    }

    // 5. Parallel top-up to remaining voter peers.
    let mut fan = futures::stream::FuturesUnordered::new();
    for (node_id, addr) in peers {
        if !leader_is_local && leader_id == Some(node_id.0) {
            continue;
        }
        fan.push(fan_one_batch(
            node_id,
            addr,
            cfg.shard_id,
            wire_batch.clone(),
            cfg.peer_rpc_timeout,
        ));
    }
    while let Some(acked) = fan.next().await {
        if acked {
            peer_acks += 1;
            if local_acks + peer_acks >= cfg.min_acks {
                for req in batch {
                    let _ = req.ack.send(Ok(()));
                }
                return;
            }
        }
    }

    // 6. Shortfall — caller MUST NOT ack the client.
    tracing::warn!(
        shard_id = %cfg.shard_id.0,
        durable = local_acks + peer_acks,
        min_acks = cfg.min_acks,
        "intent fan coalescer: quorum shortfall — refusing to ack",
    );
    for req in batch {
        let _ = req.ack.send(Err(LogError::QuorumLost(cfg.shard_id)));
    }
}

/// Fan ONE `intent_put` RPC to one peer, carrying the whole batch.
async fn fan_one_batch(
    node_id: NodeId,
    addr: String,
    shard_id: ShardId,
    wire_batch: Vec<WireIntent>,
    timeout: Duration,
) -> bool {
    let call = rpc_call::<_, Vec<bool>>(&addr, shard_id, INTENT_PUT_TAG, None, &wire_batch);
    match tokio::time::timeout(timeout, call).await {
        Ok(Ok(acks)) => !acks.is_empty() && acks.iter().all(|b| *b),
        Ok(Err(e)) => {
            tracing::debug!(node = node_id.0, addr = %addr, error = %e, "intent_put fan: peer non-ack");
            false
        }
        Err(_) => {
            tracing::debug!(node = node_id.0, addr = %addr, "intent_put fan: peer timed out");
            false
        }
    }
}

/// Read the coalescer batch-max from `KISEKI_INTENT_FAN_BATCH_MAX`.
/// Defaults to 16.
#[must_use]
pub fn batch_max_from_env() -> usize {
    std::env::var("KISEKI_INTENT_FAN_BATCH_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n >= 1)
        .unwrap_or(16)
}

/// Read the coalescer batch-timeout from `KISEKI_INTENT_FAN_BATCH_TIMEOUT_US`.
///
/// Defaults to **500 µs** (the W12 default). The L4 Mutex+Notify design
/// removes the spawned-task wake-up overhead but does NOT change the
/// effective lower bound on the timer wake-up itself, so the timeout
/// configuration is unchanged.
#[must_use]
pub fn batch_timeout_from_env() -> Duration {
    let us = std::env::var("KISEKI_INTENT_FAN_BATCH_TIMEOUT_US")
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
                inline_payloads: vec![],
            },
        }
    }

    /// `min_acks=1` → local put alone satisfies quorum; every submitter
    /// sees Ok.
    #[tokio::test(flavor = "current_thread")]
    async fn single_copy_quorum_no_fan_needed() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let resolver: PeerLeaderResolver = Arc::new(|| (Vec::new(), None));
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            CoalescerConfig {
                shard_id: ShardId(uuid::Uuid::from_u128(1)),
                local_node: NodeId(1),
                store: Arc::clone(&store),
                resolver,
                min_acks: 1,
                cap_max: 16,
                cap_timeout: Duration::from_micros(100),
                peer_rpc_timeout: Duration::from_secs(1),
            },
        );
        let mut handles = Vec::new();
        for i in 0..8u32 {
            let c = coalescer.clone();
            handles.push(tokio::spawn(
                async move { c.submit(intent(seq(1, i, 1))).await },
            ));
        }
        for h in handles {
            assert!(h.await.unwrap().is_ok(), "every submitter must see Ok");
        }
        assert_eq!(store.pending_len().unwrap(), 8);
    }

    /// Cap-reached short-circuits the timeout: 4 concurrent submitters
    /// with cap=4 + huge timeout must all complete fast.
    #[tokio::test(flavor = "current_thread")]
    async fn batch_flushes_on_cap_reached() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let resolver: PeerLeaderResolver = Arc::new(|| (Vec::new(), None));
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            CoalescerConfig {
                shard_id: ShardId(uuid::Uuid::from_u128(2)),
                local_node: NodeId(1),
                store: Arc::clone(&store),
                resolver,
                min_acks: 1,
                cap_max: 4,
                cap_timeout: Duration::from_secs(10),
                peer_rpc_timeout: Duration::from_secs(1),
            },
        );
        let mut handles = Vec::new();
        for i in 0..4u32 {
            let c = coalescer.clone();
            handles.push(tokio::spawn(
                async move { c.submit(intent(seq(2, i, 1))).await },
            ));
        }
        let started = Instant::now();
        for h in handles {
            assert!(h.await.unwrap().is_ok());
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "must flush on cap, not wait the 10s timeout"
        );
        assert_eq!(store.pending_len().unwrap(), 4);
    }

    /// Sequential single-submitter calls work — each one becomes its
    /// own flusher; previous batches don't deadlock the next one.
    #[tokio::test(flavor = "current_thread")]
    async fn sequential_submits_complete_independently() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let resolver: PeerLeaderResolver = Arc::new(|| (Vec::new(), None));
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            CoalescerConfig {
                shard_id: ShardId(uuid::Uuid::from_u128(3)),
                local_node: NodeId(1),
                store: Arc::clone(&store),
                resolver,
                min_acks: 1,
                cap_max: 16,
                cap_timeout: Duration::from_micros(50),
                peer_rpc_timeout: Duration::from_secs(1),
            },
        );
        for i in 0..3u32 {
            assert!(coalescer.submit(intent(seq(3, i, 1))).await.is_ok());
        }
        assert_eq!(store.pending_len().unwrap(), 3);
    }
}
