//! GH #230 — the pipelined per-shard committer drain.
//!
//! ## The wall (and the premise correction)
//!
//! The 2026-06-10 GCP run measured one `committer.sink_incorporate`
//! round at ~449 ms per ~608-intent batch × 18 shards ≈ 24.4 k
//! intents/s cluster ceiling. The issue's working theory blamed the
//! 100 ms `KISEKI_RAFT_FLUSH_INTERVAL_MS` group-commit interval. The
//! step-0 local measurement REFUTED that: the env var never reaches
//! the multi-node Raft path (it only gates the single-node
//! `PersistentShardStore`); the per-shard `FjallRaftLogStore` fsyncs
//! inline (`PersistMode::SyncAll`) per append batch. Locally
//! (release, 3-node loopback) the round is **content-proportional**
//! — 0.69 ms @ 76 intents → 4.29 ms @ 608 → 7.29 ms @ 1000 (~7
//! µs/intent at 512 B payloads; ~35 µs/intent with 3.5 KiB inline
//! payloads) — there is no 100 ms-class floor to decouple from.
//!
//! What remains is the SERIAL drain: one `client_write`, awaited to
//! apply, then the next. The Raft round spends most of its wall time
//! off-CPU (replication RTT + follower fsync + apply queue), so a
//! second in-flight round overlaps almost for free. The step-0 probe
//! measured the lift at depth 2 ≈ +33 % and depth 3 ≈ +36 % on raw
//! rounds, and +47 % on the full production drain path (serial
//! `drain_local` 103 k → pipelined depth-2 152 k intents/s/shard)
//! locally — where rounds are fast and partly CPU-bound; on GCP's
//! 449 ms latency-dominated rounds the overlap fraction — and
//! therefore the lift — is strictly larger.
//!
//! ## Design
//!
//! - **Single submitter, ordered log.** Rounds are submitted with
//!   [`openraft::Raft::client_write_ff`] from ONE driver (the shard's
//!   committer supervisor thread). `client_write_ff` enqueues the
//!   command into the Raft core's queue *before returning*, so
//!   submission order == log order == ascending perspective-seq
//!   across rounds. Raft then applies entries strictly in log order
//!   on every replica — pipelining changes WHEN entries are appended,
//!   never the order they apply.
//! - **Bounded depth.** At most `depth` rounds in flight per shard
//!   (`KISEKI_COMMITTER_PIPELINE_DEPTH`, default
//!   [`DEFAULT_PIPELINE_DEPTH`]). `0` opts back into the legacy
//!   serial `drain_local` path (the revert lever).
//! - **In-flight seq filter.** The drain re-reads ALL pending each
//!   tick (PART 8 drain-all — no floor). Intents already submitted
//!   but not yet applied are filtered out so they are not re-appended
//!   every tick while their round is in flight. This is an
//!   *efficiency* filter, not a correctness gate: if a seq slips
//!   through (completion/prune race), the SM's `recent_incorporated`
//!   dedup applies it exactly once. Unlike the deleted PART-6 global
//!   floor, the filter is transient — a round's seqs leave the set
//!   the moment the round completes (success OR failure), so nothing
//!   can be starved by it.
//! - **Failure = retry next tick.** A failed round (lost leadership,
//!   Raft error) just logs: its intents are still in the local store
//!   (prune is off-band, driven by the SM's recent set) and become
//!   eligible again immediately. If the entry actually committed and
//!   the error was spurious, the SM gate makes the retry a no-op.
//!
//! ## Backpressure honesty (#230 part c)
//!
//! The ack path has no backpressure — PUT op/s can numerically exceed
//! the committer's ceiling, which is unbounded acked-but-
//! unincorporated backlog, not honest throughput. The drain tick now
//! exports per-shard gauges (`kiseki_log_intent_backlog`,
//! `kiseki_log_intent_visibility_lag_seconds`,
//! `kiseki_log_committer_inflight_rounds`) and emits a throttled WARN
//! when the backlog grows monotonically across
//! [`BACKLOG_TREND_WINDOW`] consecutive drains.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::FutureExt;

use crate::error::LogError;
use crate::intent::{IntentError, IntentStore, PerspectiveSeq, WriteIntent};
use crate::intent_committer::DRAIN_BATCH_CAP;
use crate::raft_store::IncorporateItem;

/// Default number of in-flight incorporate rounds per shard. Two is
/// enough to overlap one round's replication/apply wait with the next
/// round's append; the step-0 probe showed depth 3 adds little on top
/// (+36 % vs +33 % locally; flat on the production-drain A/B) while
/// tripling the worst-case re-append window on failure.
pub const DEFAULT_PIPELINE_DEPTH: usize = 2;

/// Hard cap on the configurable depth — beyond this the in-flight
/// payload bytes (entries are MiB-class under inline-payload load)
/// start to matter for follower memory and AE round sizes.
pub const MAX_PIPELINE_DEPTH: usize = 8;

/// Consecutive leader drains with strictly-growing backlog before the
/// throttled WARN fires.
pub const BACKLOG_TREND_WINDOW: usize = 10;

/// Throttle for the backlog-growth WARN: at most one per shard per
/// this interval.
const BACKLOG_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Read `KISEKI_COMMITTER_PIPELINE_DEPTH`: `0` = legacy serial drain,
/// `1..=MAX_PIPELINE_DEPTH` = pipelined, larger values clamp.
#[must_use]
pub fn pipeline_depth_from_env() -> usize {
    std::env::var("KISEKI_COMMITTER_PIPELINE_DEPTH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_PIPELINE_DEPTH)
        .min(MAX_PIPELINE_DEPTH)
}

/// Convert one durable [`WriteIntent`] into the wire-side
/// [`IncorporateItem`]. Shared by the legacy sync sink
/// ([`crate::raft_intent_sink::RaftLogIncorporationSink`]) and the
/// pipelined drain.
#[must_use]
pub fn intent_to_item(intent: &WriteIntent) -> IncorporateItem {
    IncorporateItem {
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
            .map(|(c, b)| (c.0, b.clone()).into())
            .collect(),
    }
}

/// The async submit seam for the pipelined committer.
///
/// `submit` ENQUEUES one batched `IncorporateIntents` command into
/// the Raft log and returns a *ticket* — a future resolving when the
/// entry has been applied to the state machine (or failed). The
/// contract that makes pipelining safe:
///
/// - **Ordered enqueue:** when `submit` returns, the command is in
///   the Raft core's queue. Two sequential `submit` calls from one
///   task therefore append in call order (log order == submit order).
/// - **Ticket resolution:** `Ok(())` only after the leader's SM has
///   applied the entry. `Err` on Raft failure (deposed leader, core
///   stopped) — the entry MAY still have committed; callers must
///   treat retry-after-error as at-least-once (the SM dedup gate
///   makes that exactly-once).
///
/// Implemented for `Arc<OpenRaftLogStore>` in production; tests use a
/// hand-rolled fake with manually-resolved tickets.
#[allow(async_fn_in_trait)]
pub trait PipelinedIntentAppender: Send + Sync {
    /// Enqueue `items` as one `IncorporateIntents` Raft command.
    ///
    /// # Errors
    /// [`LogError`] when the command cannot be enqueued at all (shard
    /// in maintenance, Raft core gone).
    async fn submit_intents(
        &self,
        items: Vec<IncorporateItem>,
    ) -> Result<BoxFuture<'static, Result<(), LogError>>, LogError>;
}

impl PipelinedIntentAppender for Arc<crate::raft::openraft_store::OpenRaftLogStore> {
    async fn submit_intents(
        &self,
        items: Vec<IncorporateItem>,
    ) -> Result<BoxFuture<'static, Result<(), LogError>>, LogError> {
        let rx = crate::raft::openraft_store::OpenRaftLogStore::submit_intents(self, items).await?;
        Ok(rx.boxed())
    }
}

/// One submitted-but-not-yet-applied incorporate round.
struct InFlightRound {
    /// The perspective-seqs riding this round — removed from the
    /// in-flight filter set when the round completes.
    seqs: Vec<PerspectiveSeq>,
    /// Resolves when the entry is applied (or the round failed).
    ticket: BoxFuture<'static, Result<(), LogError>>,
    /// RAII hot-path timer — drops (observes) at reap, so the
    /// `committer.sink_incorporate` histogram keeps meaning
    /// "submit → applied" per round, now overlapped. Held purely for
    /// its `Drop` (never read — hence the underscore name).
    _timer: kiseki_tracing::hot_path::HotTimer,
}

/// Strictly-monotonic backlog growth detector (#230 part c). Pure and
/// unit-testable: `push` returns `true` when the WARN should fire.
pub(crate) struct BacklogTrend {
    window: VecDeque<usize>,
    cap: usize,
}

impl BacklogTrend {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Record one drain-tick backlog observation. Returns `true` when
    /// the last `cap` observations are strictly increasing (the
    /// committer is persistently losing ground). Resets the window
    /// after signalling so the WARN re-arms on fresh evidence.
    pub(crate) fn push(&mut self, backlog: usize) -> bool {
        if let Some(&last) = self.window.back() {
            if backlog <= last {
                self.window.clear();
            }
        }
        self.window.push_back(backlog);
        if self.window.len() >= self.cap {
            self.window.clear();
            return true;
        }
        false
    }
}

/// Per-shard pending-intent backlog gauge (acked-but-unincorporated,
/// including intents riding in-flight rounds).
fn intent_backlog_gauge() -> &'static prometheus::IntGaugeVec {
    static G: std::sync::OnceLock<prometheus::IntGaugeVec> = std::sync::OnceLock::new();
    G.get_or_init(|| {
        prometheus::register_int_gauge_vec!(
            prometheus::Opts::new(
                "kiseki_log_intent_backlog",
                "Acked-but-unincorporated intents in this shard's local intent \
                 store at the last committer drain tick (#230). Sustained growth \
                 means PUT op/s exceeds the committer's incorporation ceiling — \
                 the excess is visibility lag, not throughput."
            ),
            &["shard"],
        )
        .expect("kiseki-log: failed to register intent_backlog gauge")
    })
}

/// Per-shard visibility lag: age (seconds) of the OLDEST pending
/// intent at the last drain tick.
fn visibility_lag_gauge() -> &'static prometheus::GaugeVec {
    static G: std::sync::OnceLock<prometheus::GaugeVec> = std::sync::OnceLock::new();
    G.get_or_init(|| {
        prometheus::register_gauge_vec!(
            prometheus::Opts::new(
                "kiseki_log_intent_visibility_lag_seconds",
                "Age of the oldest acked-but-unincorporated intent (now − its \
                 perspective-seq HLC physical time) at the last committer drain \
                 tick (#230). The honest 'how stale can an acked write be' number."
            ),
            &["shard"],
        )
        .expect("kiseki-log: failed to register visibility_lag gauge")
    })
}

/// Per-shard in-flight incorporate rounds (0..=depth).
fn inflight_rounds_gauge() -> &'static prometheus::IntGaugeVec {
    static G: std::sync::OnceLock<prometheus::IntGaugeVec> = std::sync::OnceLock::new();
    G.get_or_init(|| {
        prometheus::register_int_gauge_vec!(
            prometheus::Opts::new(
                "kiseki_log_committer_inflight_rounds",
                "Incorporate rounds currently in flight for this shard (#230 \
                 pipelining; bounded by KISEKI_COMMITTER_PIPELINE_DEPTH)."
            ),
            &["shard"],
        )
        .expect("kiseki-log: failed to register inflight_rounds gauge")
    })
}

/// Count of backlog-growth WARNs per shard (so alerting can key on a
/// counter even when the log line is throttled away).
fn backlog_growth_warn_counter() -> &'static prometheus::IntCounterVec {
    static C: std::sync::OnceLock<prometheus::IntCounterVec> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        prometheus::register_int_counter_vec!(
            prometheus::Opts::new(
                "kiseki_log_intent_backlog_growth_total",
                "Times this shard's intent backlog grew strictly monotonically \
                 across N consecutive committer drains (#230). Non-zero and \
                 climbing = the cluster is being driven past the incorporation \
                 wall and acked writes are falling behind visibility."
            ),
            &["shard"],
        )
        .expect("kiseki-log: failed to register backlog_growth counter")
    })
}

/// The pipelined per-shard committer drain (GH #230). Owned and
/// driven by the shard's committer supervisor (one instance per
/// shard, single-threaded driver — the ordering contract).
pub struct PipelinedCommitter<A: PipelinedIntentAppender> {
    store: Arc<dyn IntentStore>,
    appender: A,
    depth: usize,
    in_flight: VecDeque<InFlightRound>,
    in_flight_seqs: HashSet<PerspectiveSeq>,
    trend: BacklogTrend,
    shard_label: String,
    last_backlog_warn: Option<std::time::Instant>,
}

impl<A: PipelinedIntentAppender> PipelinedCommitter<A> {
    /// Build a drain over the shard's local intent store and the Raft
    /// submit seam. `depth >= 1` (callers map `0` to the legacy serial
    /// path before constructing).
    #[must_use]
    pub fn new(
        store: Arc<dyn IntentStore>,
        appender: A,
        depth: usize,
        shard_label: String,
    ) -> Self {
        Self {
            store,
            appender,
            depth: depth.max(1),
            in_flight: VecDeque::new(),
            in_flight_seqs: HashSet::new(),
            trend: BacklogTrend::new(BACKLOG_TREND_WINDOW),
            shard_label,
            last_backlog_warn: None,
        }
    }

    /// Rounds currently in flight.
    #[must_use]
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Await EVERY in-flight round to completion (probe + shutdown
    /// helper — the steady-state drain deliberately leaves rounds in
    /// flight across ticks instead).
    pub async fn flush_in_flight(&mut self) {
        while !self.in_flight.is_empty() {
            self.await_oldest().await;
        }
        self.export_inflight_gauge();
    }

    /// Drop all in-flight bookkeeping (leadership lost / supervisor
    /// parking). The rounds themselves may still commit — that's fine,
    /// the SM gate dedups and the off-band prune cleans the store.
    pub fn clear_in_flight(&mut self) {
        self.in_flight.clear();
        self.in_flight_seqs.clear();
        self.export_inflight_gauge();
    }

    /// Reap rounds that have already completed, front-first
    /// (completions arrive in log order — the front is always the
    /// oldest submission). Non-blocking: stops at the first
    /// still-pending round.
    fn reap_completed(&mut self) {
        while let Some(front) = self.in_flight.front_mut() {
            match (&mut front.ticket).now_or_never() {
                Some(res) => {
                    let round = self
                        .in_flight
                        .pop_front()
                        .expect("front_mut() just returned Some");
                    // `round` (and its RAII hot-path timer) drops here —
                    // the `committer.sink_incorporate` histogram observes
                    // submit → applied for this round.
                    self.finish_round(&round.seqs, res);
                }
                None => break,
            }
        }
    }

    /// Await the OLDEST in-flight round (used when at depth). Errors
    /// are logged, not propagated — the round's intents stay pending
    /// and retry next tick.
    async fn await_oldest(&mut self) {
        if let Some(mut round) = self.in_flight.pop_front() {
            let res = round.ticket.as_mut().await;
            // `round`'s RAII hot-path timer drops at end of scope.
            self.finish_round(&round.seqs, res);
        }
    }

    /// Common completion bookkeeping: release the in-flight seq
    /// filter, log failures.
    fn finish_round(&mut self, seqs: &[PerspectiveSeq], res: Result<(), LogError>) {
        for s in seqs {
            self.in_flight_seqs.remove(s);
        }
        if let Err(e) = res {
            // Routine on leadership churn; the intents are still in the
            // local store and re-drain on the next leader (or next tick
            // if the error was transient). The SM gate dedups any round
            // that actually committed before the error surfaced.
            tracing::warn!(
                shard = %self.shard_label,
                error = %e,
                intents = seqs.len(),
                "pipelined incorporate round failed — intents remain pending, retrying next tick",
            );
        }
    }

    /// Run one drain tick (leader only — the supervisor gates):
    ///
    /// 1. reap completed rounds (non-blocking);
    /// 2. read ALL pending, ascending (PART 8 drain-all, no floor);
    /// 3. export backlog / visibility-lag gauges + the trend WARN;
    /// 4. filter out seqs already in flight;
    /// 5. submit remaining intents in `DRAIN_BATCH_CAP` batches,
    ///    awaiting the oldest round whenever `depth` rounds are in
    ///    flight. Returns with up to `depth` rounds still in flight —
    ///    they overlap the supervisor's tick sleep.
    ///
    /// # Returns
    /// The number of intents SUBMITTED this tick (not necessarily yet
    /// applied).
    ///
    /// # Errors
    /// Propagates [`IntentError`] from [`IntentStore::pending`] or a
    /// submit-side enqueue failure ([`IntentError::Incorporate`]).
    pub async fn drain_tick(&mut self) -> Result<usize, IntentError> {
        self.reap_completed();

        // Same hot-span label as the legacy path — dashboards keep
        // reading the store-scan cost regardless of drain mode.
        let pending_res =
            kiseki_tracing::hot_span!("committer.read_pending", { self.store.pending() });
        let mut pending = pending_res?;

        self.observe_backlog(&pending);

        if pending.is_empty() {
            return Ok(0);
        }
        kiseki_tracing::hot_span!("committer.sort_and_filter", {
            pending.sort_by_key(|i| i.perspective_seq);
            pending.retain(|i| !self.in_flight_seqs.contains(&i.perspective_seq));
        });

        let mut submitted = 0usize;
        for batch in pending.chunks(DRAIN_BATCH_CAP) {
            while self.in_flight.len() >= self.depth {
                self.await_oldest().await;
            }
            let items: Vec<IncorporateItem> = batch.iter().map(intent_to_item).collect();
            // The round timer starts at submit and observes at reap —
            // `committer.sink_incorporate` stays "one Raft round,
            // submit → applied", now measured overlapped.
            let timer = kiseki_tracing::hot_path::HotTimer::new("committer.sink_incorporate");
            let ticket = match self.appender.submit_intents(items).await {
                Ok(t) => t,
                Err(e) => {
                    // Keep the in-flight gauge honest on the early
                    // return — rounds submitted earlier this tick (or
                    // left over from previous ticks) are still in
                    // flight; without this the gauge reads a stale
                    // value until the next tick.
                    self.export_inflight_gauge();
                    return Err(IntentError::Incorporate(e.to_string()));
                }
            };
            let seqs: Vec<PerspectiveSeq> = batch.iter().map(|i| i.perspective_seq).collect();
            for s in &seqs {
                self.in_flight_seqs.insert(*s);
            }
            self.in_flight.push_back(InFlightRound {
                seqs,
                ticket,
                _timer: timer,
            });
            submitted += batch.len();
        }
        self.export_inflight_gauge();
        Ok(submitted)
    }

    /// Export the current in-flight round count to the per-shard gauge.
    fn export_inflight_gauge(&self) {
        inflight_rounds_gauge()
            .with_label_values(&[&self.shard_label])
            .set(i64::try_from(self.in_flight.len()).unwrap_or(i64::MAX));
    }

    /// Gauge + trend bookkeeping for one tick's pending snapshot.
    fn observe_backlog(&mut self, pending: &[WriteIntent]) {
        let backlog = pending.len();
        intent_backlog_gauge()
            .with_label_values(&[&self.shard_label])
            .set(i64::try_from(backlog).unwrap_or(i64::MAX));

        let now_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let oldest_ms = pending
            .iter()
            .map(|i| i.perspective_seq.0.physical_ms)
            .min();
        // Clamp the age to u32 ms (~49 days) before the f64 conversion —
        // any real lag is seconds-class; the clamp only protects the cast.
        let lag_s = oldest_ms.map_or(0.0, |o| {
            f64::from(u32::try_from(now_ms.saturating_sub(o)).unwrap_or(u32::MAX)) / 1_000.0
        });
        visibility_lag_gauge()
            .with_label_values(&[&self.shard_label])
            .set(lag_s);

        // Trend WARN: only meaningful when there is real backlog —
        // ignore sub-batch noise (a backlog under one round's cap
        // clears in a single tick).
        if backlog > DRAIN_BATCH_CAP && self.trend.push(backlog) {
            backlog_growth_warn_counter()
                .with_label_values(&[&self.shard_label])
                .inc();
            let now = std::time::Instant::now();
            if self
                .last_backlog_warn
                .is_none_or(|t| now.duration_since(t) >= BACKLOG_WARN_INTERVAL)
            {
                self.last_backlog_warn = Some(now);
                tracing::warn!(
                    shard = %self.shard_label,
                    backlog,
                    visibility_lag_s = lag_s,
                    window = BACKLOG_TREND_WINDOW,
                    "intent backlog grew monotonically across the last N drains — \
                     ingest rate exceeds the committer's incorporation ceiling; \
                     acked writes are falling behind visibility (#230)",
                );
            }
        } else if backlog <= DRAIN_BATCH_CAP {
            // Below one batch the committer is keeping up — reset the
            // trend so stale growth evidence doesn't accumulate.
            self.trend = BacklogTrend::new(BACKLOG_TREND_WINDOW);
        }
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
    use crate::intent::{InMemIntentStore, PerspectiveSeq, PutOutcome};
    use crate::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};

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

    fn fill(store: &InMemIntentStore, seqs: &[PerspectiveSeq]) {
        for s in seqs {
            assert_eq!(store.put(intent(*s)).unwrap(), PutOutcome::Recorded);
        }
    }

    /// A fake appender whose tickets the TEST resolves manually —
    /// deterministic control over round completion for pipelining
    /// assertions. Records every submitted batch's seqs in order.
    #[derive(Clone, Default)]
    struct ManualAppender {
        inner: Arc<ManualInner>,
    }

    type TicketTx = tokio::sync::oneshot::Sender<Result<(), LogError>>;

    #[derive(Default)]
    struct ManualInner {
        /// Seqs of each submitted batch, in submission order.
        batches: Mutex<Vec<Vec<HybridLogicalClock>>>,
        /// Senders resolving each batch's ticket, same order.
        ticket_txs: Mutex<Vec<Option<TicketTx>>>,
    }

    impl ManualAppender {
        fn batches(&self) -> Vec<Vec<HybridLogicalClock>> {
            self.inner
                .batches
                .lock()
                .lock_or_die("manual_appender.batches")
                .clone()
        }

        /// Resolve the `idx`-th submitted round.
        fn resolve(&self, idx: usize, res: Result<(), LogError>) {
            let tx = self
                .inner
                .ticket_txs
                .lock()
                .lock_or_die("manual_appender.ticket_txs")[idx]
                .take()
                .expect("round already resolved");
            tx.send(res).ok();
        }
    }

    impl PipelinedIntentAppender for ManualAppender {
        async fn submit_intents(
            &self,
            items: Vec<IncorporateItem>,
        ) -> Result<BoxFuture<'static, Result<(), LogError>>, LogError> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.inner
                .batches
                .lock()
                .lock_or_die("manual_appender.batches")
                .push(items.iter().map(|i| i.perspective_seq).collect());
            self.inner
                .ticket_txs
                .lock()
                .lock_or_die("manual_appender.ticket_txs")
                .push(Some(tx));
            Ok(async move { rx.await.map_err(|_| LogError::Unavailable).and_then(|r| r) }.boxed())
        }
    }

    fn committer(
        store: &Arc<InMemIntentStore>,
        appender: &ManualAppender,
        depth: usize,
    ) -> PipelinedCommitter<ManualAppender> {
        PipelinedCommitter::new(
            Arc::clone(store) as Arc<dyn IntentStore>,
            appender.clone(),
            depth,
            "test-shard".into(),
        )
    }

    /// Depth 2: a tick submits the whole backlog as one round and
    /// leaves it in flight (no blocking on an idle pipeline).
    #[tokio::test]
    async fn drain_submits_without_awaiting_when_below_depth() {
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1)]);
        let appender = ManualAppender::default();
        let mut pc = committer(&store, &appender, 2);

        let n = pc.drain_tick().await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(pc.in_flight_len(), 1, "round left in flight, not awaited");
        assert_eq!(
            appender.batches(),
            vec![vec![seq(1, 0, 1).0, seq(2, 0, 1).0]],
            "ascending perspective order within the round",
        );
    }

    /// The in-flight filter: while a round is unresolved, a re-drain
    /// does NOT resubmit its seqs; NEW seqs still go out (in a new
    /// round), so nothing is starved behind the in-flight round.
    #[tokio::test]
    async fn in_flight_seqs_are_not_resubmitted() {
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1)]);
        let appender = ManualAppender::default();
        let mut pc = committer(&store, &appender, 3);

        assert_eq!(pc.drain_tick().await.unwrap(), 2);
        // Round 0 unresolved; two new intents arrive (one OLDER-seq
        // late arrival from another writer, one newer).
        fill(&store, &[seq(0, 0, 2), seq(3, 0, 1)]);
        assert_eq!(
            pc.drain_tick().await.unwrap(),
            2,
            "only the two new seqs submit"
        );
        assert_eq!(pc.in_flight_len(), 2);
        let batches = appender.batches();
        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches[1],
            vec![seq(0, 0, 2).0, seq(3, 0, 1).0],
            "late-arriving OLDER seq still drains (no PART-6 floor) — \
             just in a later log entry",
        );
    }

    /// Depth bound: with depth 2 and a backlog of 3 batches, the
    /// drain awaits the oldest round before submitting the third —
    /// never more than `depth` in flight.
    #[tokio::test]
    async fn depth_bound_awaits_oldest_round() {
        let store = Arc::new(InMemIntentStore::new());
        // 2.5 batches worth of seqs.
        let seqs: Vec<PerspectiveSeq> = (1..=2_500u64).map(|i| seq(i, 0, 1)).collect();
        fill(&store, &seqs);
        let appender = ManualAppender::default();
        let mut pc = committer(&store, &appender, 2);

        // Resolve round 0 the moment it exists, from a parallel task —
        // the drain blocks awaiting it before submitting batch 3.
        let resolver = appender.clone();
        let resolve_task = tokio::spawn(async move {
            loop {
                let n = resolver
                    .inner
                    .ticket_txs
                    .lock()
                    .lock_or_die("test resolver")
                    .len();
                if n >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            resolver.resolve(0, Ok(()));
        });

        let n = pc.drain_tick().await.unwrap();
        resolve_task.await.unwrap();
        assert_eq!(n, 2_500, "all three batches submitted");
        assert_eq!(
            appender.batches().len(),
            3,
            "split at DRAIN_BATCH_CAP into 3 rounds"
        );
        assert_eq!(pc.in_flight_len(), 2, "depth bound held");
        // Cross-batch ordering: batch 2 starts after batch 1 ends.
        let b = appender.batches();
        assert!(b[0].last().unwrap() < b[1].first().unwrap());
        assert!(b[1].last().unwrap() < b[2].first().unwrap());
    }

    /// Round completion releases the filter: after resolve + reap, the
    /// (still unpruned) seqs are eligible again and resubmit — the SM
    /// gate is the dedup, the filter is only an in-flight guard.
    #[tokio::test]
    async fn completed_round_seqs_become_eligible_again() {
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1)]);
        let appender = ManualAppender::default();
        let mut pc = committer(&store, &appender, 2);

        assert_eq!(pc.drain_tick().await.unwrap(), 1);
        appender.resolve(0, Ok(()));
        // Yield so the oneshot send lands before the reap polls.
        tokio::task::yield_now().await;
        // Store NOT pruned (prune is off-band) — the seq re-reads as
        // pending and, with its round complete, resubmits.
        assert_eq!(pc.drain_tick().await.unwrap(), 1);
        assert_eq!(appender.batches().len(), 2, "resubmitted after completion");
        assert_eq!(pc.in_flight_len(), 1);
    }

    /// A FAILED round releases its seqs for retry (and only logs —
    /// `drain_tick` itself succeeds).
    #[tokio::test]
    async fn failed_round_retries_next_tick() {
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1)]);
        let appender = ManualAppender::default();
        let mut pc = committer(&store, &appender, 2);

        assert_eq!(pc.drain_tick().await.unwrap(), 2);
        appender.resolve(0, Err(LogError::Unavailable));
        tokio::task::yield_now().await;

        assert_eq!(
            pc.drain_tick().await.unwrap(),
            2,
            "failed round's intents resubmit"
        );
        assert_eq!(appender.batches().len(), 2);
        assert_eq!(
            appender.batches()[1],
            vec![seq(1, 0, 1).0, seq(2, 0, 1).0],
            "same seqs, ascending",
        );
    }

    /// `clear_in_flight` (leadership loss) drops the filter so a
    /// re-elected leader's fresh drain re-submits everything pending —
    /// at-least-once, SM-deduped.
    #[tokio::test]
    async fn clear_in_flight_resets_filter() {
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1)]);
        let appender = ManualAppender::default();
        let mut pc = committer(&store, &appender, 2);

        assert_eq!(pc.drain_tick().await.unwrap(), 1);
        assert_eq!(pc.in_flight_len(), 1);
        pc.clear_in_flight();
        assert_eq!(pc.in_flight_len(), 0);
        assert_eq!(pc.drain_tick().await.unwrap(), 1, "resubmits after clear");
    }

    /// Off-band prune semantics under pipelining: once the supervisor
    /// prunes applied seqs from the store, the drain stops seeing
    /// them — no resubmission, in-flight bookkeeping unaffected.
    #[tokio::test]
    async fn pruned_seqs_do_not_resubmit() {
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1)]);
        let appender = ManualAppender::default();
        let mut pc = committer(&store, &appender, 2);

        assert_eq!(pc.drain_tick().await.unwrap(), 2);
        appender.resolve(0, Ok(()));
        tokio::task::yield_now().await;
        // Supervisor's off-band per-intent prune (SM recent set).
        store.remove_seqs(&[seq(1, 0, 1), seq(2, 0, 1)]).unwrap();

        assert_eq!(pc.drain_tick().await.unwrap(), 0, "nothing pending");
        assert_eq!(appender.batches().len(), 1, "no resubmission");
        assert_eq!(pc.in_flight_len(), 0);
    }

    /// Backlog trend detector: strictly-increasing observations fire
    /// at the window cap; any non-increase resets.
    #[test]
    fn backlog_trend_fires_only_on_monotonic_growth() {
        let mut t = BacklogTrend::new(4);
        assert!(!t.push(10));
        assert!(!t.push(20));
        assert!(!t.push(30));
        assert!(t.push(40), "4th strictly-increasing observation fires");
        // Window reset after firing.
        assert!(!t.push(50));
        // A plateau resets the window (50 → [50] again).
        assert!(!t.push(50));
        assert!(!t.push(60));
        assert!(!t.push(70));
        assert!(
            t.push(80),
            "4 strictly-increasing observations after the plateau reset fire again"
        );
        // And a decrease resets too.
        assert!(!t.push(10));
        assert!(!t.push(20));
    }

    /// The default depth must sit inside the clamp range (a config
    /// regression here would silently serialize the drain).
    #[test]
    fn default_depth_is_pipelined_and_within_clamp() {
        // NOTE: no env mutation here (process-wide) — exercise the
        // constants' relationship instead.
        assert_eq!(DEFAULT_PIPELINE_DEPTH.clamp(1, MAX_PIPELINE_DEPTH), 2);
    }
}
