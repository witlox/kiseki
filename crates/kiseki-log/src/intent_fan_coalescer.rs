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
//! runs the local put + fan + ack distribution synchronously.
//!
//! GH #228 (2026-06-10) revised the flush itself:
//! - The local durable copy goes through `IntentStore::submit_batch`
//!   (the store's dedicated commit thread): UNDER CONTENTION the fjall
//!   commit moves off the tokio worker and cross-flush group commit
//!   kicks in; the uncontended fast path still commits inline on the
//!   caller (identical to pre-#228 — the hop is only paid when there
//!   is something to batch with, per the task-hop regression ledger).
//! - The min-acks top-up is TARGETED: leader-first (unchanged), then
//!   ONE rotation-picked candidate at a time under the short
//!   `KISEKI_INTENT_TOPUP_TIMEOUT_MS` budget with fallback to the next
//!   candidate — replacing the all-peers broadcast whose cancelled
//!   in-flight RPCs measured 39 % redundant `intent_put` traffic and
//!   ~1.9 k/s/ingress TCP redial churn.
//!
//! GH #253 (2026-06-11) added the RESCUE stage: when the targeted walk
//! ends short of `min_acks`, the flush does NOT declare `QuorumLost`
//! yet — it re-fans to every non-acked peer in PARALLEL under the full
//! `peer_rpc_timeout` budget first. The 2026-06-11 local 3-node A/B
//! showed the short sequential windows alone (≤ 2 × 100 ms of total
//! quorum budget) spuriously fail 0.2–0.45 % of PUTs under follower
//! fsync pressure, with the errors scaling with run length; the rescue
//! restores the pre-#228 broadcast's robustness while paying its RPC
//! cost only on the rare shortfall path. `kiseki_intent_topup_rescue_
//! total` / `_saved_total` count entries and saves.
//!
//! ## Submitter paths
//!
//! - **Flusher path** (first arrival in a fresh batch):
//!   1. Acquire mutex, push intent, set `flusher_running = true`.
//!   2. Drop mutex.
//!   3. `select!` between `sleep(deadline)` and `cap_reached.notified()`.
//!   4. Re-acquire mutex, take pending batch, clear `flusher_running`.
//!   5. Drop mutex.
//!   6. Run local `submit_batch` + fan; per-input ack via each
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
//! Unchanged from W12. The local batch commit is atomic and the local
//! copy commits before the fan starts (`submit_batch` resolves only
//! after the durable commit). A panic mid-flush leaves submitters'
//! oneshots dropped → they see `Err(Unavailable)`; the gateway
//! retries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use kiseki_common::ids::{NodeId, ShardId};
use kiseki_common::locks::LockOrDie;
use kiseki_raft::tcp_transport::rpc_call_aux;
use tokio::sync::{oneshot, Notify};

use crate::error::LogError;
use crate::intent::{IntentStore, WriteIntent};
use crate::intent_metrics;
use crate::intent_sync::{WireIntent, INTENT_PUT_TAG};

/// One submitted PUT awaiting a coalesced ack.
struct CoalesceReq {
    intent: WriteIntent,
    submitted_at: Instant,
    /// ADR-047 hot-path timer (`pif.enqueue_wait`) — started at submit,
    /// ended by the flusher the moment it TAKES the batch (set to
    /// `None` in [`run_flush_cycle`]), so it measures submit →
    /// batch-taken, not submit → ack. Held as a struct field (the
    /// `hot_timer!` macro can't span two functions), which is why this
    /// uses `HotTimer::new` directly — the off-feature `HotTimer` is a
    /// ZST with a no-op `Drop`, so production builds pay nothing.
    enqueue_wait: Option<kiseki_tracing::hot_path::HotTimer>,
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
    /// GH #228 — per-flush rotation counter for the targeted top-up.
    /// `fetch_add` per top-up round picks the starting candidate among
    /// the non-leader voters, so steady-state top-up load spreads
    /// evenly instead of always hammering the first peer in the
    /// resolver's order.
    topup_seq: AtomicU64,
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
    /// Per-peer RPC timeout for the leader-first hop (MF-3) — the
    /// generous budget (3 s at the `RaftShardStore` wiring site).
    pub peer_rpc_timeout: Duration,
    /// GH #228 — per-attempt timeout for the targeted top-up
    /// (`KISEKI_INTENT_TOPUP_TIMEOUT_MS`, default 100 ms — deliberately
    /// SHORT relative to `peer_rpc_timeout`'s 3 s): a slow candidate
    /// costs one short stall before the walk falls back to the NEXT
    /// candidate, preserving the `min_acks` guarantee without the old
    /// broadcast's redundant RPCs.
    pub topup_rpc_timeout: Duration,
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
            topup_seq: AtomicU64::new(0),
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
            enqueue_wait: Some(kiseki_tracing::hot_path::HotTimer::new("pif.enqueue_wait")),
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
    let mut batch: Vec<CoalesceReq> = {
        let mut s = inner.state.lock().lock_or_die("intent_fan_coalescer.state");
        let taken = std::mem::take(&mut s.pending);
        s.flusher_running = false;
        taken
    };

    // Observe the per-intent wait + batch size BEFORE the flush.
    let flushed_at = Instant::now();
    for req in &mut batch {
        intent_metrics::observe_coalesce_wait(
            flushed_at.saturating_duration_since(req.submitted_at),
        );
        // ADR-047 (pif.enqueue_wait) ends HERE — the flusher has taken
        // the batch; everything after this point is flush wall time
        // (pif.flush_total), not queueing.
        req.enqueue_wait = None;
    }
    // Batch-size distribution — the direct evidence for the conc128
    // batch-cap convoy hypothesis (KISEKI_INTENT_FAN_BATCH_MAX=16
    // default pinning batches at the cap under load). The always-on
    // `kiseki_intent_put_batch_size` histogram is the canonical view;
    // the debug line below gives a scrape-free read on a live run.
    intent_metrics::observe_intent_put_batch_size(batch.len());
    tracing::debug!(
        shard_id = %inner.cfg.shard_id.0,
        batch_size = batch.len(),
        "intent fan coalescer: batch taken for flush",
    );

    flush_batch(&inner, batch).await;
}

// One flush is one linear quorum walk (local put → leader-first →
// targeted top-up → rescue → shortfall); splitting the stages would
// scatter the ack/return bookkeeping each stage shares.
#[allow(clippy::too_many_lines)]
async fn flush_batch(inner: &CoalescerInner, batch: Vec<CoalesceReq>) {
    let cfg = &inner.cfg;

    // ADR-047 hot-path timer (pif.flush_total) — wall time of the whole
    // flush: local put + leader-first hop + top-up + ack distribution.
    // RAII, so every early return (fast path, quorum reached, shortfall)
    // still observes. Together with pif.enqueue_wait this partitions
    // pif.total: total ≈ enqueue_wait + flush_total per intent.
    kiseki_tracing::hot_timer_guard!(_ht_pif_flush_total = "pif.flush_total");

    // 1. Local durable copy (one fjall batch, one WAL sync).
    // ADR-047 hot-path timer (pif.local_put) — submit-to-complete on the
    // submitting node. GH #228: routed through `submit_batch`, so the
    // fjall commit runs on the store's dedicated commit thread and
    // group-commits with the inbound `intent_put` handler's batches
    // (same per-shard store Arc) instead of blocking this tokio worker
    // for the WAL sync. The span still measures the honest ack-path
    // cost: the future resolves only after the durable commit.
    let intents: Vec<WriteIntent> = batch.iter().map(|r| r.intent.clone()).collect();
    let put_res =
        kiseki_tracing::hot_span!("pif.local_put", { cfg.store.submit_batch(intents).await });
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
    // GH #253 — peers that did NOT ack their first attempt go into the
    // rescue pool (step 5b) instead of being written off for the flush.
    let mut rescue_pool: Vec<(NodeId, String)> = Vec::new();
    if !leader_is_local {
        if let Some(lid) = leader_id {
            if let Some((node_id, addr)) = peers.iter().find(|(n, _)| n.0 == lid).cloned() {
                // ADR-047 hot-path timer (pif.leader_first_hop) — the
                // awaited leader-first peer RPC (MF-3). Serial with the
                // top-up, so it's a pure add to the flush wall time.
                let acked = kiseki_tracing::hot_span!("pif.leader_first_hop", {
                    fan_one_batch(
                        node_id,
                        addr.clone(),
                        cfg.shard_id,
                        wire_batch.clone(),
                        cfg.peer_rpc_timeout,
                    )
                    .await
                });
                if acked {
                    peer_acks += 1;
                    if local_acks + peer_acks >= cfg.min_acks {
                        for req in batch {
                            let _ = req.ack.send(Ok(()));
                        }
                        return;
                    }
                } else {
                    rescue_pool.push((node_id, addr));
                }
            }
        }
    }

    // 5. Targeted top-up (GH #228). The pre-#228 shape fanned to EVERY
    // remaining voter in parallel and returned at quorum, cancelling the
    // still-in-flight RPCs — measured on the 2026-06-10 GCP runs at 39 %
    // redundant intent_put RPCs (2.64 store-applies/write vs the 2.0
    // min_acks needs) plus ~1.9 k/s/ingress TCP redial churn from the
    // cancelled pooled streams. Instead: walk ONE candidate at a time in
    // a per-flush rotation (so steady-state load spreads evenly across
    // the non-leader voters), each under the SHORT
    // `KISEKI_INTENT_TOPUP_TIMEOUT_MS` budget, falling back to the NEXT
    // candidate on timeout/error — the min_acks guarantee and most of
    // the tail benefit survive a slow peer at a fraction of the RPC
    // load. Steady-state RPC cancellation is gone; the SLOW-PEER
    // fallback still drops its timed-out in-flight future (since
    // PR-1c the drop only abandons a request_id on the shared mux
    // connection — no stream is torn down — but the timed-out peer
    // may still apply server-side, so applies/write can exceed 2.0
    // under sustained peer pressure; watch
    // kiseki_intent_commit_batch_size and the applies/write ratio on
    // instrumented runs).
    //
    // ADR-047 hot-path timer (pif.topup) — from entering the top-up walk
    // to flush exit (quorum return or shortfall; RAII drops at every
    // path). The shortfall tail adds only a warn! + ack sends.
    kiseki_tracing::hot_timer_guard!(_ht_pif_topup = "pif.topup");
    let mut candidates: Vec<(NodeId, String)> = peers
        .into_iter()
        // The leader was already tried by the leader-first hop above —
        // don't re-try it. (When the leader IS local it never appears
        // in `peers`, which excludes the local node.)
        .filter(|(node_id, _)| leader_is_local || leader_id != Some(node_id.0))
        .collect();
    if !candidates.is_empty() {
        let start = usize::try_from(inner.topup_seq.fetch_add(1, Ordering::Relaxed)).unwrap_or(0)
            % candidates.len();
        candidates.rotate_left(start);
        for (node_id, addr) in candidates {
            let acked = fan_one_batch(
                node_id,
                addr.clone(),
                cfg.shard_id,
                wire_batch.clone(),
                cfg.topup_rpc_timeout,
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
            } else {
                rescue_pool.push((node_id, addr));
            }
        }
    }

    // 5b. Rescue broadcast (GH #253). The targeted walk gives each
    // candidate ONE short `topup_rpc_timeout` window — on a 3-node
    // cluster that is ≤ 2 × 100 ms of total quorum budget against
    // followers whose fsync stalls are CORRELATED (same load wave,
    // and locally the same disk). The 2026-06-11 A/B traced sustained
    // `QuorumLost` on acked-able writes to exactly this: both probes
    // miss, the write is refused, yet both peers go on to apply it.
    // Falsified alternatives from that run: committer round flooding
    // (#230 — errors persisted at PIPELINE_DEPTH=0) and the #231
    // hydrator window (errors persisted at KISEKI_HYDRATOR_BATCH=1000);
    // raising the topup window to 1 s produced 0 errors.
    //
    // So before declaring shortfall, re-fan to every non-acked
    // candidate IN PARALLEL under the full `peer_rpc_timeout` budget —
    // the pre-#228 broadcast's robustness, paid ONLY on the rare path
    // (steady-state keeps #228's targeted-walk RPC savings).
    // `QuorumLost` then honestly means "peers unreachable or slower
    // than the generous budget", not "two short probes missed". The
    // re-sent batch is idempotent on the receiver (per-seq store dedup)
    // and a timed-out probe's late server-side apply was already
    // possible pre-#253 (documented above).
    if local_acks + peer_acks < cfg.min_acks && !rescue_pool.is_empty() {
        let needed = cfg.min_acks - (local_acks + peer_acks);
        peer_acks += rescue_fan_until(
            rescue_pool,
            cfg.shard_id,
            &wire_batch,
            cfg.peer_rpc_timeout,
            needed,
        )
        .await;
        if local_acks + peer_acks >= cfg.min_acks {
            intent_metrics::inc_topup_rescue_saved();
            for req in batch {
                let _ = req.ack.send(Ok(()));
            }
            return;
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

/// GH #253 — the parallel rescue broadcast: fan `wire_batch` to every
/// peer in `rescue_pool` concurrently under the (generous) `timeout`,
/// reaping acks as they land and returning as soon as `needed` acks
/// arrive (or the pool is exhausted). Returns the number of acks
/// gathered (`<= needed`). Counts entries via
/// `kiseki_intent_topup_rescue_total`; the CALLER counts saves (it
/// knows whether quorum was reached).
async fn rescue_fan_until(
    rescue_pool: Vec<(NodeId, String)>,
    shard_id: ShardId,
    wire_batch: &[WireIntent],
    timeout: Duration,
    needed: usize,
) -> usize {
    intent_metrics::inc_topup_rescue();
    kiseki_tracing::hot_timer_guard!(_ht_pif_rescue = "pif.rescue_fan");
    let mut rescue_fans: FuturesUnordered<_> = rescue_pool
        .into_iter()
        .map(|(node_id, addr)| fan_one_batch(node_id, addr, shard_id, wire_batch.to_vec(), timeout))
        .collect();
    let mut acks = 0usize;
    while let Some(acked) = rescue_fans.next().await {
        if acked {
            acks += 1;
            if acks >= needed {
                break;
            }
        }
    }
    acks
}

/// Fan ONE `intent_put` RPC to one peer, carrying the whole batch.
///
/// Rides the request_id-multiplexed aux transport (GH #228 PR-1c):
/// concurrent fans to the same peer share a pooled connection, so fan
/// concurrency no longer consumes connection slots or counts against
/// the peer's inbound connection cap. Dropping this future on the
/// top-up timeout below is safe — the abandoned request is reaped
/// client-side and its late response is read-and-dropped without
/// disturbing the shared stream (see `rpc_call_aux`).
async fn fan_one_batch(
    node_id: NodeId,
    addr: String,
    shard_id: ShardId,
    wire_batch: Vec<WireIntent>,
    timeout: Duration,
) -> bool {
    let call = rpc_call_aux::<_, Vec<bool>>(&addr, shard_id, INTENT_PUT_TAG, None, &wire_batch);
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

/// Read the targeted top-up per-attempt timeout from
/// `KISEKI_INTENT_TOPUP_TIMEOUT_MS` (GH #228).
///
/// Defaults to **100 ms** — deliberately SHORT next to the 3 s
/// leader-first per-RPC budget (`INTENT_FAN_PEER_TIMEOUT` at the
/// `RaftShardStore` wiring site): the top-up walk falls back to the
/// next candidate on expiry, so the timeout bounds the added tail when
/// one peer is slow rather than bounding the whole flush.
#[must_use]
pub fn topup_timeout_from_env() -> Duration {
    let ms = std::env::var("KISEKI_INTENT_TOPUP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n >= 1)
        .unwrap_or(100);
    Duration::from_millis(ms)
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
                topup_rpc_timeout: Duration::from_millis(100),
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
                topup_rpc_timeout: Duration::from_millis(100),
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
                topup_rpc_timeout: Duration::from_millis(100),
            },
        );
        for i in 0..3u32 {
            assert!(coalescer.submit(intent(seq(3, i, 1))).await.is_ok());
        }
        assert_eq!(store.pending_len().unwrap(), 3);
    }

    /// Instrumented flush path (`pif.enqueue_wait` / `pif.flush_total` /
    /// `pif.local_put` sub-spans) still acks every submitter across
    /// MULTIPLE flush cycles: 8 submitters with cap=3 forces at least
    /// three batches (cap-reached + timer flushes mixed), so the
    /// enqueue-timer reset in `run_flush_cycle` runs on both the
    /// flusher and waiter paths repeatedly.
    #[tokio::test(flavor = "current_thread")]
    async fn multi_cycle_flush_acks_every_submitter() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let resolver: PeerLeaderResolver = Arc::new(|| (Vec::new(), None));
        let coalescer = spawn(
            &tokio::runtime::Handle::current(),
            CoalescerConfig {
                shard_id: ShardId(uuid::Uuid::from_u128(4)),
                local_node: NodeId(1),
                store: Arc::clone(&store),
                resolver,
                min_acks: 1,
                cap_max: 3,
                cap_timeout: Duration::from_micros(100),
                peer_rpc_timeout: Duration::from_secs(1),
                topup_rpc_timeout: Duration::from_millis(100),
            },
        );
        let mut handles = Vec::new();
        for i in 0..8u32 {
            let c = coalescer.clone();
            handles.push(tokio::spawn(
                async move { c.submit(intent(seq(4, i, 1))).await },
            ));
        }
        for h in handles {
            assert!(h.await.unwrap().is_ok(), "every submitter must see Ok");
        }
        assert_eq!(store.pending_len().unwrap(), 8);
    }
}
