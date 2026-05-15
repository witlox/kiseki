//! Prometheus metrics for the Log context (ADR-032).
//!
//! Exposes per-shard counters + duration histograms for the
//! data-path operations operators need to alert on:
//!
//! 1. `kiseki_log_append_total{shard, outcome}` — delta append rate.
//! 2. `kiseki_log_append_duration_seconds{shard}` — append latency.
//! 3. `kiseki_log_read_total{shard, outcome}` — delta read rate.
//! 4. `kiseki_log_read_duration_seconds{shard}` — read latency.
//! 5. `kiseki_log_compaction_total{shard, outcome}` — compaction
//!    attempts (per-shard).
//! 6. `kiseki_log_compaction_duration_seconds{shard}` — compaction
//!    duration distribution.
//! 7. `kiseki_log_compaction_removed_total` — total deltas removed
//!    by compaction (cumulative across all shards).
//! 8. `kiseki_log_watermark_advance_total{shard, consumer}` — count
//!    of watermark-advance calls per (shard, consumer) pair.
//! 9. `kiseki_log_truncate_boundary_seq` — last truncation
//!    boundary sequence number (gauge), per-shard.
//!
//! All `kiseki_log_*`-prefixed so they sort cleanly in the
//! Prometheus scrape.

use std::time::Duration;

use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGaugeVec, Opts, Registry,
};

/// Outcome label values for the per-op counters.
pub mod outcome {
    /// Operation completed successfully.
    pub const OK: &str = "ok";
    /// Operation failed (any `LogError` other than `ShardNotFound`).
    pub const ERROR: &str = "error";
    /// `ShardNotFound` — separated so dashboards can highlight
    /// stale routing-cache pressure.
    pub const SHARD_NOT_FOUND: &str = "shard_not_found";
    /// `ForwardToLeader` (ADR-044) — separated so the
    /// "proxy-forward rate" alarm (`kiseki_log_append_total{outcome="forward_to_leader"}`)
    /// can fire on sustained > 20 % forwarding share without
    /// drowning in the generic `error` bucket. The gate-1 finding
    /// C-M3 mandated this label so operators can distinguish "leader
    /// stable, follower received the write" from generic Raft failures.
    pub const FORWARD_TO_LEADER: &str = "forward_to_leader";
}

/// Prometheus metrics struct for the Log context.
#[derive(Clone)]
pub struct LogMetrics {
    /// Per-shard delta append count by outcome.
    pub append_total: IntCounterVec,
    /// Per-shard append latency.
    pub append_duration: HistogramVec,
    /// Per-shard delta read count by outcome.
    pub read_total: IntCounterVec,
    /// Per-shard read latency.
    pub read_duration: HistogramVec,
    /// Per-shard compaction count by outcome.
    pub compaction_total: IntCounterVec,
    /// Per-shard compaction latency.
    pub compaction_duration: HistogramVec,
    /// Total deltas removed by compaction (cumulative).
    pub compaction_removed_total: IntCounter,
    /// Per-(shard, consumer) watermark-advance count.
    pub watermark_advance_total: IntCounterVec,
    /// Per-shard last truncation boundary sequence.
    pub truncate_boundary_seq: IntGaugeVec,
}

impl LogMetrics {
    /// Build all metrics and register with `registry`.
    ///
    /// # Errors
    /// Returns `prometheus::Error` on name collisions.
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let append_total = IntCounterVec::new(
            Opts::new(
                "kiseki_log_append_total",
                "Per-shard delta-append count by outcome (ADR-032).",
            ),
            &["shard", "outcome"],
        )?;
        registry.register(Box::new(append_total.clone()))?;

        let append_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_log_append_duration_seconds",
                "Per-shard delta-append latency (ADR-032).",
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
            ]),
            &["shard"],
        )?;
        registry.register(Box::new(append_duration.clone()))?;

        let read_total = IntCounterVec::new(
            Opts::new(
                "kiseki_log_read_total",
                "Per-shard delta-read count by outcome.",
            ),
            &["shard", "outcome"],
        )?;
        registry.register(Box::new(read_total.clone()))?;

        let read_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_log_read_duration_seconds",
                "Per-shard delta-read latency.",
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
            ]),
            &["shard"],
        )?;
        registry.register(Box::new(read_duration.clone()))?;

        let compaction_total = IntCounterVec::new(
            Opts::new(
                "kiseki_log_compaction_total",
                "Per-shard compaction count by outcome.",
            ),
            &["shard", "outcome"],
        )?;
        registry.register(Box::new(compaction_total.clone()))?;

        let compaction_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_log_compaction_duration_seconds",
                "Per-shard compaction duration.",
            )
            .buckets(vec![
                0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
            ]),
            &["shard"],
        )?;
        registry.register(Box::new(compaction_duration.clone()))?;

        let compaction_removed_total = IntCounter::new(
            "kiseki_log_compaction_removed_total",
            "Total deltas removed by compaction (cumulative across shards).",
        )?;
        registry.register(Box::new(compaction_removed_total.clone()))?;

        let watermark_advance_total = IntCounterVec::new(
            Opts::new(
                "kiseki_log_watermark_advance_total",
                "Per-(shard, consumer) watermark-advance call count.",
            ),
            &["shard", "consumer"],
        )?;
        registry.register(Box::new(watermark_advance_total.clone()))?;

        let truncate_boundary_seq = IntGaugeVec::new(
            Opts::new(
                "kiseki_log_truncate_boundary_seq",
                "Per-shard last truncation boundary sequence number.",
            ),
            &["shard"],
        )?;
        registry.register(Box::new(truncate_boundary_seq.clone()))?;

        Ok(Self {
            append_total,
            append_duration,
            read_total,
            read_duration,
            compaction_total,
            compaction_duration,
            compaction_removed_total,
            watermark_advance_total,
            truncate_boundary_seq,
        })
    }

    /// Record an append completion.
    pub fn record_append(&self, shard_id: &str, outcome: &str, dur: Duration) {
        self.append_total
            .with_label_values(&[shard_id, outcome])
            .inc();
        self.append_duration
            .with_label_values(&[shard_id])
            .observe(dur.as_secs_f64());
    }

    /// Record a read completion.
    pub fn record_read(&self, shard_id: &str, outcome: &str, dur: Duration) {
        self.read_total
            .with_label_values(&[shard_id, outcome])
            .inc();
        self.read_duration
            .with_label_values(&[shard_id])
            .observe(dur.as_secs_f64());
    }

    /// Record a compaction completion.
    pub fn record_compaction(&self, shard_id: &str, outcome: &str, dur: Duration, removed: u64) {
        self.compaction_total
            .with_label_values(&[shard_id, outcome])
            .inc();
        self.compaction_duration
            .with_label_values(&[shard_id])
            .observe(dur.as_secs_f64());
        self.compaction_removed_total.inc_by(removed);
    }

    /// Record a watermark advance.
    pub fn record_watermark_advance(&self, shard_id: &str, consumer: &str) {
        self.watermark_advance_total
            .with_label_values(&[shard_id, consumer])
            .inc();
    }

    /// Update the per-shard truncate boundary gauge.
    pub fn set_truncate_boundary(&self, shard_id: &str, seq: u64) {
        self.truncate_boundary_seq
            .with_label_values(&[shard_id])
            .set(i64::try_from(seq).unwrap_or(i64::MAX));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_and_record_round_trip() {
        let reg = Registry::new();
        let m = LogMetrics::register(&reg).expect("register ok");

        m.record_append("s-1", outcome::OK, Duration::from_micros(80));
        m.record_read("s-1", outcome::OK, Duration::from_micros(50));
        m.record_compaction("s-1", outcome::OK, Duration::from_millis(200), 12);
        m.record_watermark_advance("s-1", "view-nfs");
        m.set_truncate_boundary("s-1", 42);

        assert_eq!(
            m.append_total
                .with_label_values(&["s-1", outcome::OK])
                .get(),
            1,
        );
        assert_eq!(m.compaction_removed_total.get(), 12);

        let names: std::collections::HashSet<_> =
            reg.gather().iter().map(|f| f.name().to_owned()).collect();
        for expected in &[
            "kiseki_log_append_total",
            "kiseki_log_append_duration_seconds",
            "kiseki_log_read_total",
            "kiseki_log_read_duration_seconds",
            "kiseki_log_compaction_total",
            "kiseki_log_compaction_duration_seconds",
            "kiseki_log_compaction_removed_total",
            "kiseki_log_watermark_advance_total",
            "kiseki_log_truncate_boundary_seq",
        ] {
            assert!(names.contains(*expected), "{expected} not registered");
        }
    }

    #[test]
    fn double_register_returns_error_not_panic() {
        let reg = Registry::new();
        let _m1 = LogMetrics::register(&reg).expect("first");
        let m2 = LogMetrics::register(&reg);
        assert!(m2.is_err());
    }
}
