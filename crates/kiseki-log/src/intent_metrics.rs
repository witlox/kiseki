//! W12 (2026-06-02) — intent-fan coalescing metrics.
//!
//! Two Prometheus histograms register at server boot and are observed
//! from the hot path via free functions:
//!
//! - `kiseki_intent_put_batch_size` — distribution of intents per fanned
//!   `intent_put` RPC. Pairs with the existing
//!   `kiseki_raft_transport_rpc_duration_seconds{op="intent_put"}` so
//!   the per-RPC mean is interpretable post-W12 (a higher mean now means
//!   bigger batches, not slower receivers).
//! - `kiseki_intent_coalesce_wait_seconds` — per-intent wait inside the
//!   producer coalescer (submission → flush). The tuning knob between
//!   throughput (bigger batches) and per-PUT tail latency.
//!
//! Both are simple single-instance histograms (no labels) — they
//! aggregate across all shards. Cardinality is bounded by the bucket
//! list alone, so adding labels would be the only way to inflate the
//! scrape, and we deliberately don't.
//!
//! Calls fire silently before [`register`] runs (pre-boot, unit tests).

use std::sync::OnceLock;
use std::time::Duration;

use prometheus::{Histogram, HistogramOpts, IntCounter, Registry};

/// `kiseki_intent_put_batch_size` — exposed on `/metrics`.
pub const BATCH_SIZE_METRIC_NAME: &str = "kiseki_intent_put_batch_size";

/// `kiseki_intent_coalesce_wait_seconds` — exposed on `/metrics`.
pub const COALESCE_WAIT_METRIC_NAME: &str = "kiseki_intent_coalesce_wait_seconds";

/// `kiseki_intent_commit_batch_size` — exposed on `/metrics`.
pub const COMMIT_BATCH_SIZE_METRIC_NAME: &str = "kiseki_intent_commit_batch_size";

/// `kiseki_intent_topup_rescue_total` — exposed on `/metrics`.
pub const TOPUP_RESCUE_METRIC_NAME: &str = "kiseki_intent_topup_rescue_total";

/// `kiseki_intent_topup_rescue_saved_total` — exposed on `/metrics`.
pub const TOPUP_RESCUE_SAVED_METRIC_NAME: &str = "kiseki_intent_topup_rescue_saved_total";

/// Batch-size buckets — powers-of-two from 1 → 128. `KISEKI_INTENT_FAN_BATCH_MAX`
/// defaults to 16, so the 1, 2, 4, 8, 16 buckets capture the steady-state
/// distribution; higher buckets are for capacity/headroom analysis.
const BATCH_SIZE_BUCKETS: &[f64] = &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0];

/// Coalesce-wait buckets (seconds) — 1 µs → 10 ms. The
/// `KISEKI_INTENT_FAN_BATCH_TIMEOUT_US` default is 500 µs, so the wait
/// p99 should sit inside the 1 µs–500 µs window in normal load.
const COALESCE_WAIT_BUCKETS: &[f64] = &[
    0.000_001, 0.000_005, 0.000_01, 0.000_05, 0.000_1, 0.000_25, 0.000_5, 0.001, 0.0025, 0.005,
    0.01,
];

static BATCH_SIZE: OnceLock<Histogram> = OnceLock::new();
static COALESCE_WAIT: OnceLock<Histogram> = OnceLock::new();
static COMMIT_BATCH_SIZE: OnceLock<Histogram> = OnceLock::new();
static TOPUP_RESCUE: OnceLock<IntCounter> = OnceLock::new();
static TOPUP_RESCUE_SAVED: OnceLock<IntCounter> = OnceLock::new();

/// Register the W12 intent-coalescing histograms into the given
/// Prometheus [`Registry`]. Idempotent: a second call after the
/// histograms latch is a no-op.
///
/// # Errors
///
/// Propagates `prometheus::Error` on a registry collision (e.g. a
/// duplicate metric name from elsewhere in the same registry). Both
/// names are unique within the workspace.
pub fn register(registry: &Registry) -> Result<(), prometheus::Error> {
    if BATCH_SIZE.get().is_none() {
        let h = Histogram::with_opts(
            HistogramOpts::new(
                BATCH_SIZE_METRIC_NAME,
                "Number of intents per fanned intent_put RPC (W12 producer-side coalescing). \
                 Pairs with kiseki_raft_transport_rpc_duration_seconds{op=\"intent_put\"} so the \
                 per-RPC mean stays interpretable after batching.",
            )
            .buckets(BATCH_SIZE_BUCKETS.to_vec()),
        )?;
        registry.register(Box::new(h.clone()))?;
        let _ = BATCH_SIZE.set(h);
    }
    if COALESCE_WAIT.get().is_none() {
        let h = Histogram::with_opts(
            HistogramOpts::new(
                COALESCE_WAIT_METRIC_NAME,
                "Per-intent wait inside the producer-side coalescer, from submission to flush \
                 (W12). The throughput-vs-tail-latency tuning knob — \
                 KISEKI_INTENT_FAN_BATCH_TIMEOUT_US is the upper bound.",
            )
            .buckets(COALESCE_WAIT_BUCKETS.to_vec()),
        )?;
        registry.register(Box::new(h.clone()))?;
        let _ = COALESCE_WAIT.set(h);
    }
    if COMMIT_BATCH_SIZE.get().is_none() {
        let h = Histogram::with_opts(
            HistogramOpts::new(
                COMMIT_BATCH_SIZE_METRIC_NAME,
                "Submissions per intent-store fjall commit (GH #228 dedicated commit thread; \
                 the pif.commit_batch_size drain-size distribution). 1 = the inline fast path \
                 or a lone drain; >1 = cross-flush group commit amortising the WAL sync.",
            )
            .buckets(BATCH_SIZE_BUCKETS.to_vec()),
        )?;
        registry.register(Box::new(h.clone()))?;
        let _ = COMMIT_BATCH_SIZE.set(h);
    }
    if TOPUP_RESCUE.get().is_none() {
        let c = IntCounter::new(
            TOPUP_RESCUE_METRIC_NAME,
            "Times the #228 targeted top-up walk ended short of min_acks and the flush \
             escalated to the GH #253 parallel rescue broadcast (full peer_rpc_timeout \
             budget). Sustained growth = followers are missing the short topup window — \
             the cluster is near its ingest wall.",
        )?;
        registry.register(Box::new(c.clone()))?;
        let _ = TOPUP_RESCUE.set(c);
    }
    if TOPUP_RESCUE_SAVED.get().is_none() {
        let c = IntCounter::new(
            TOPUP_RESCUE_SAVED_METRIC_NAME,
            "Rescue broadcasts (GH #253) that reached min_acks — each one is an acked \
             write that the pre-#253 walk would have spuriously failed with QuorumLost.",
        )?;
        registry.register(Box::new(c.clone()))?;
        let _ = TOPUP_RESCUE_SAVED.set(c);
    }
    Ok(())
}

/// Observe one fanned-batch size. Called from the receiver-side
/// `intent_put` dispatcher and the producer-side coalescer (both
/// observe the same `n`; aggregating across all nodes gives the cluster
/// distribution).
pub fn observe_intent_put_batch_size(n: usize) {
    if let Some(h) = BATCH_SIZE.get() {
        // Clamp at 2^52 so the f64 mantissa never overflows. Real batches
        // are bounded by KISEKI_INTENT_FAN_BATCH_MAX (≤ 128 in any sane
        // config); this is defensive only.
        let clamped = u64::try_from(n).unwrap_or(u64::MAX).min(1u64 << 52);
        // u64 fits in f64 mantissa once clamped ≤ 2^52 — no precision loss.
        #[allow(clippy::cast_precision_loss)]
        let v = clamped as f64;
        h.observe(v);
    }
}

/// Observe one per-intent coalesce wait. Called by the producer
/// coalescer when each PUT is included in a flushed batch.
pub fn observe_coalesce_wait(d: Duration) {
    if let Some(h) = COALESCE_WAIT.get() {
        h.observe(d.as_secs_f64());
    }
}

/// Observe one commit cycle's drain size (GH #228): the number of
/// submissions sharing one fjall batch + WAL sync. Called by the
/// dedicated commit thread per drain cycle, and by `submit_batch`'s
/// inline fast path with `1`.
pub fn observe_commit_batch_size(n: usize) {
    if let Some(h) = COMMIT_BATCH_SIZE.get() {
        // Bounded by COMMIT_QUEUE_DEPTH + 1 in practice; clamp is
        // defensive only (mirrors observe_intent_put_batch_size).
        let clamped = u64::try_from(n).unwrap_or(u64::MAX).min(1u64 << 52);
        #[allow(clippy::cast_precision_loss)]
        let v = clamped as f64;
        h.observe(v);
    }
}

/// Count one entry into the GH #253 rescue broadcast (the targeted
/// top-up walk ended short of `min_acks`).
pub fn inc_topup_rescue() {
    if let Some(c) = TOPUP_RESCUE.get() {
        c.inc();
    }
}

/// Count one rescue broadcast that reached `min_acks` — a write the
/// pre-#253 walk would have spuriously failed.
pub fn inc_topup_rescue_saved() {
    if let Some(c) = TOPUP_RESCUE_SAVED.get() {
        c.inc();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use prometheus::Registry;

    #[test]
    fn register_is_idempotent() {
        let reg = Registry::new();
        register(&reg).expect("first");
        register(&reg).expect("second (no-op)");
    }

    #[test]
    fn observe_before_register_does_not_panic() {
        // No-op when the global isn't latched — the assertion is "no panic".
        observe_intent_put_batch_size(8);
        observe_coalesce_wait(Duration::from_micros(250));
        observe_commit_batch_size(3);
        inc_topup_rescue();
        inc_topup_rescue_saved();
    }
}
