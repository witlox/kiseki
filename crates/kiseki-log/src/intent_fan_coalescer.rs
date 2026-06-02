//! W12 (2026-06-02) — producer-side intent-fan coalescer.
//!
//! The 2026-06-02 W11 GCP A/B left the cluster at 7.4 k aggregate PUT
//! op/s with `raft_transport_rpc{op="intent_put"}` at 22 k RPC/s × 158 ms
//! mean = ~3 500 in-flight fans cluster-wide. The receiver's `aux.store_put`
//! was dominated by one fjall WAL sync per PUT; the producer's RPC volume
//! was the wire-level throttle.
//!
//! This module amortises both. Per-shard, a single background task
//! accumulates submitted intents until either:
//!
//! - The batch hits `KISEKI_INTENT_FAN_BATCH_MAX` intents (default 16), or
//! - `KISEKI_INTENT_FAN_BATCH_TIMEOUT_US` elapses since the FIRST submission
//!   in the batch (default 500 µs).
//!
//! On flush the task does ONE `store.put_batch` for all local writes
//! (one mutex + one fjall batch + one WAL sync) and fans ONE
//! `intent_put` RPC carrying `Vec<WireIntent>` to each peer. The receiver
//! decodes the whole vector, does one `store.put_batch`, and returns a
//! `Vec<bool>` (one ack flag per input). Per-batch atomicity means a peer
//! either acks every intent or none, so the producer treats each peer's
//! contribution as a uniform `+1 ack` to every intent in the batch.
//!
//! ## Why a spawned task and not a Mutex<Coalescer>
//!
//! The async submitter just `send`s and `await`s a oneshot — no shared
//! mutex on the hot path. The task owns the batch state without locking
//! and can use `tokio::select!` for the count-vs-timeout race cleanly.
//! Per-shard tasks are cheap (one task per shard, dropped on shutdown via
//! the channel-close path).
//!
//! ## Crash safety
//!
//! The local `put_batch` commits before the fan starts (no-loss floor
//! I-L2/I-CS1 — the producer always has the durable copy first). If the
//! task crashes mid-flush, submitted PUTs are NOT acked (their oneshot
//! receiver returns `Err(LogError::Unavailable)`); the caller treats that
//! as a non-ack and retries. The local fjall state is consistent
//! regardless because `put_batch` is atomic.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kiseki_common::ids::{NodeId, ShardId};
use kiseki_raft::tcp_transport::rpc_call;
use tokio::sync::{mpsc, oneshot};

use crate::error::LogError;
use crate::intent::{IntentStore, WriteIntent};
use crate::intent_metrics;
use crate::intent_sync::{WireIntent, INTENT_PUT_TAG};

/// One submitted PUT awaiting a coalesced ack.
struct CoalesceReq {
    intent: WriteIntent,
    submitted_at: Instant,
    /// Resolves when the batch this intent belongs to has finished its
    /// fan. `Ok(())` once durable copies (local + remote acks) reach
    /// `min_acks` for this intent; otherwise `Err(QuorumLost)`.
    ack: oneshot::Sender<Result<(), LogError>>,
}

/// Handle for `put_intent_and_fan` to submit intents and await acks.
#[derive(Clone)]
pub struct IntentFanCoalescer {
    tx: mpsc::Sender<CoalesceReq>,
}

/// Per-shard resolver closure: returns the current voter peer set
/// (`(NodeId, addr)`) excluding the local node, AND the current
/// leader id (if known). Computed fresh on every flush so a recent
/// membership change or leadership move is honoured without restart.
pub type PeerLeaderResolver = Arc<dyn Fn() -> (Vec<(NodeId, String)>, Option<u64>) + Send + Sync>;

/// Configuration for spawning a coalescer — pulled into a struct to keep
/// `spawn`'s arg count under the clippy `too-many-arguments` limit.
pub struct CoalescerConfig {
    /// The shard this coalescer fans for. One coalescer per shard.
    pub shard_id: ShardId,
    /// Local node id — drives the `leader_is_local` decision per flush.
    pub local_node: NodeId,
    /// Per-shard durable intent store.
    pub store: Arc<dyn IntentStore>,
    /// Live resolver for the voter peer set + current leader (see
    /// [`PeerLeaderResolver`]).
    pub resolver: PeerLeaderResolver,
    /// Quorum threshold — `Ok` only when durable copies (local + peer
    /// acks) reach this.
    pub min_acks: usize,
    /// Max intents per batch (`KISEKI_INTENT_FAN_BATCH_MAX`).
    pub cap_max: usize,
    /// Max wait from first submission to flush (`KISEKI_INTENT_FAN_BATCH_TIMEOUT_US`).
    pub cap_timeout: Duration,
    /// Per-peer RPC timeout (mirrors the pre-W12 `INTENT_FAN_PEER_TIMEOUT`).
    pub peer_rpc_timeout: Duration,
}

/// Spawn the coalescer task on `runtime` and return the submission
/// handle. The task lives until either:
///
/// - The handle (and all its clones) are dropped — closes the channel.
/// - The runtime shuts down.
#[must_use]
pub fn spawn(runtime: &tokio::runtime::Handle, cfg: CoalescerConfig) -> IntentFanCoalescer {
    // Backpressure cap on the channel — 4 × cap_max keeps a few batches
    // queued under burst load without unbounded growth. Senders await
    // capacity (no .try_send).
    let (tx, rx) = mpsc::channel(cfg.cap_max.saturating_mul(4).max(64));
    runtime.spawn(coalescer_loop(cfg, rx));
    IntentFanCoalescer { tx }
}

impl IntentFanCoalescer {
    /// Submit one intent for fanning. Returns once the batch this
    /// intent belongs to has finished and the per-intent ack count
    /// has been compared against `min_acks`.
    ///
    /// # Errors
    /// [`LogError::Unavailable`] if the coalescer task has stopped
    /// (channel closed) or if the oneshot is dropped before the batch
    /// flushes (the task panicked / the runtime is shutting down).
    /// [`LogError::QuorumLost`] when the batch's durable copies fall
    /// short of `min_acks` (callers MUST NOT ack the client).
    pub async fn submit(&self, intent: WriteIntent) -> Result<(), LogError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(CoalesceReq {
                intent,
                submitted_at: Instant::now(),
                ack: ack_tx,
            })
            .await
            .map_err(|_| LogError::Unavailable)?;
        ack_rx.await.unwrap_or(Err(LogError::Unavailable))
    }
}

async fn coalescer_loop(cfg: CoalescerConfig, mut rx: mpsc::Receiver<CoalesceReq>) {
    loop {
        // Block until the first request — no batch starts until at least
        // one PUT is waiting. The recv() resolves to None when every
        // sender has been dropped: the coalescer's natural shutdown.
        let Some(first) = rx.recv().await else {
            return;
        };
        let batch_start = first.submitted_at;
        let mut batch: Vec<CoalesceReq> = Vec::with_capacity(cfg.cap_max);
        batch.push(first);

        // Accumulate up to cap_max OR until cap_timeout elapses since
        // the first submission. The select! picks whichever happens
        // first; `recv` resolving with None means the channel closed
        // while we were filling — flush what we have and exit at the
        // top of the next outer loop iteration.
        while batch.len() < cfg.cap_max {
            let deadline = batch_start + cfg.cap_timeout;
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            tokio::select! {
                next = rx.recv() => {
                    match next {
                        Some(req) => batch.push(req),
                        None => break, // channel closed — flush and exit.
                    }
                }
                () = tokio::time::sleep(remaining) => break,
            }
        }

        // Observe the per-intent wait + the batch size BEFORE the flush
        // (so a slow fan never inflates the wait measurement).
        let flushed_at = Instant::now();
        for req in &batch {
            intent_metrics::observe_coalesce_wait(
                flushed_at.saturating_duration_since(req.submitted_at),
            );
        }
        intent_metrics::observe_intent_put_batch_size(batch.len());

        flush_batch(&cfg, batch).await;
    }
}

async fn flush_batch(cfg: &CoalescerConfig, batch: Vec<CoalesceReq>) {
    use futures::StreamExt;

    // 1. Local durable copy (one fjall batch, one WAL sync). On error every
    //    PUT in the batch gets a non-ack — the caller treats that as
    //    Unavailable, and the local fjall state stays consistent (put_batch
    //    is atomic).
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
    // After this point every intent has one local durable copy.
    let local_acks: usize = 1;

    // 2. Fast path — single-copy quorum is satisfied by the local write
    //    alone. No fan needed; signal everyone Ok.
    if local_acks >= cfg.min_acks {
        for req in batch {
            let _ = req.ack.send(Ok(()));
        }
        return;
    }

    // 3. Resolve peers + leader. The closure re-reads live membership +
    //    leadership state each flush, so a recent change is honoured.
    let (peers, leader_id) = (cfg.resolver)();
    let leader_is_local = leader_id == Some(cfg.local_node.0);

    // Encode the wire batch once and clone per-peer.
    let wire_batch: Vec<WireIntent> = batch.iter().map(|r| WireIntent::from(&r.intent)).collect();

    // 4. Leader-first when the leader is a remote voter (MF-3 no-orphan).
    //    Tracks how many peer-acks the batch has gotten; one peer ack
    //    contributes uniformly to every intent's durable count.
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

    // 5. Parallel top-up to remaining voter peers. Stop as soon as the
    //    durable copy count reaches min_acks.
    let mut fan = futures::stream::FuturesUnordered::new();
    for (node_id, addr) in peers {
        if !leader_is_local && leader_id == Some(node_id.0) {
            continue; // already fanned leader above
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
/// Returns `true` if the peer acked (the receiver's `put_batch` succeeded);
/// the receiver's per-batch atomicity means a partial peer ack is impossible
/// today — either every intent is durable on the peer or none.
async fn fan_one_batch(
    node_id: NodeId,
    addr: String,
    shard_id: ShardId,
    wire_batch: Vec<WireIntent>,
    timeout: Duration,
) -> bool {
    let call = rpc_call::<_, Vec<bool>>(&addr, shard_id, INTENT_PUT_TAG, None, &wire_batch);
    match tokio::time::timeout(timeout, call).await {
        Ok(Ok(acks)) => {
            // Treat the peer's response as "acked" iff every intent in the
            // batch reports true. With per-batch atomicity on the receiver
            // this is always the whole vector; the per-intent shape stays
            // for forward-compat (a future receiver could partially ack
            // by returning per-intent flags).
            !acks.is_empty() && acks.iter().all(|b| *b)
        }
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
/// Defaults to 16 — the 2026-06-02 W11 A/B headline analysis estimated
/// 4-8× lift at this batch size with sub-millisecond tail-latency cost.
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
/// Defaults to **100 µs** (#174 — was 500 µs pre-W12-followups). The
/// 2026-06-02 W12 GCP A/B measured `kiseki_intent_put_batch_size` at
/// mean 1.17 with the 500 µs default: 86 % of fans carried a single
/// intent because the bench's per-shard arrival rate (~1 PUT/ms/shard)
/// could not feed batches of 4-8 within the window. Holding the timeout
/// at 500 µs added per-PUT latency without throughput benefit. 100 µs
/// preserves the 14 % of fans that genuinely had concurrent partners
/// while cutting the no-partner wait by 5×.
#[must_use]
pub fn batch_timeout_from_env() -> Duration {
    let us = std::env::var("KISEKI_INTENT_FAN_BATCH_TIMEOUT_US")
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

    /// A single-copy quorum (`min_acks=1`) is satisfied by the local
    /// `put_batch` alone — no fan needed, every submitter sees `Ok`.
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
        for i in 0..8 {
            let c = coalescer.clone();
            handles.push(tokio::spawn(
                async move { c.submit(intent(seq(1, i, 1))).await },
            ));
        }
        for h in handles {
            assert!(h.await.unwrap().is_ok(), "every submitter must see Ok");
        }
        assert_eq!(store.pending_len().unwrap(), 8, "all eight landed locally");
    }

    /// Batch fills to `cap_max` then flushes early (no timeout wait).
    /// Stresses the "fill triggers flush" branch.
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
                cap_max: 4,                           // small cap
                cap_timeout: Duration::from_secs(10), // huge timeout — must NOT fire
                peer_rpc_timeout: Duration::from_secs(1),
            },
        );
        let mut handles = Vec::new();
        for i in 0..4 {
            let c = coalescer.clone();
            handles.push(tokio::spawn(
                async move { c.submit(intent(seq(2, i, 1))).await },
            ));
        }
        // All four must resolve quickly (cap reached, not waiting for timeout).
        let started = Instant::now();
        for h in handles {
            assert!(h.await.unwrap().is_ok());
        }
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "must flush on cap_max, not wait the 10s timeout"
        );
        assert_eq!(store.pending_len().unwrap(), 4);
    }

    /// Channel-closed shutdown: dropping the handle ends the task cleanly
    /// AFTER the in-flight batch completes.
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_handle_shuts_down_task() {
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
                cap_timeout: Duration::from_micros(100),
                peer_rpc_timeout: Duration::from_secs(1),
            },
        );
        coalescer.submit(intent(seq(3, 0, 1))).await.unwrap();
        drop(coalescer);
        // Yield so the task observes the channel closure and exits.
        tokio::task::yield_now().await;
        assert_eq!(store.pending_len().unwrap(), 1);
    }
}
