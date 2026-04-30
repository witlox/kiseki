//! Composition-side Prometheus metrics (ADR-040 §D10).
//!
//! The 11 counters / gauges / histograms that surface persistent-
//! storage health (redb size, LRU hit rate, commit errors) and
//! hydrator health (apply latency, last-applied-seq per shard,
//! transient/permanent skip counts, halt flag).
//!
//! Pattern follows `kiseki_chunk_cluster::FabricMetrics` and
//! `kiseki_gateway::metrics::GatewayRetryMetrics`: the runtime
//! constructs one [`CompositionMetrics`] at startup, registers it
//! with the global `Registry`, and clones the `Arc` into
//! [`PersistentRedbStorage`] (via `with_metrics`) and
//! [`CompositionHydrator`] (via `with_metrics`). Tests that
//! don't pass metrics get no-op behavior because the consumer
//! fields are `Option<Arc<CompositionMetrics>>`.
//!
//! [`PersistentRedbStorage`]: crate::persistent::PersistentRedbStorage
//! [`CompositionHydrator`]: crate::hydrator::CompositionHydrator

use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
};

/// Permanent-skip reasons. Used as the `reason` label on
/// `kiseki_composition_hydrator_skip_total`. Stable strings so
/// dashboards / alerts can route by exact value.
pub mod skip_reason {
    /// The delta payload didn't decode (wrong length, postcard
    /// failure). One of `bad_payload_*` for the 3 op variants.
    pub const CREATE_PAYLOAD_DECODE: &str = "create_payload_decode";
    /// Update payload decode failure.
    pub const UPDATE_PAYLOAD_DECODE: &str = "update_payload_decode";
    /// Delete payload decode failure.
    pub const DELETE_PAYLOAD_DECODE: &str = "delete_payload_decode";
    /// Storage backend returned an error during the staging read.
    pub const STORAGE_READ_FAILED: &str = "storage_read_failed";
    /// Transient skip exhausted its retry budget and was promoted to
    /// permanent. Operator action: investigate the upstream cause
    /// (commonly: namespace not yet replicated to this node).
    pub const EXHAUSTED_RETRIES: &str = "exhausted_retries";
}

/// Decode-error kinds for `kiseki_composition_decode_errors_total`.
/// Mirrors `PersistentStoreError::metric_kind()`.
pub mod decode_kind {
    /// I/O during read (rare).
    pub const IO: &str = "io";
    /// On-disk record advertises a `schema_version` this binary
    /// can't decode. Surfaced as "binary too old."
    pub const SCHEMA_TOO_NEW: &str = "schema_too_new";
    /// Postcard payload doesn't match the declared shape.
    pub const DECODE: &str = "decode";
    /// Inner-domain error from `CompositionStore` rule validation
    /// (e.g. namespace not registered).
    pub const COMPOSITION: &str = "composition";
    /// Catch-all for redb table / transaction / storage errors.
    pub const BACKEND: &str = "backend";
}

/// Composition-side metrics surface (ADR-040 §D10).
#[derive(Clone)]
pub struct CompositionMetrics {
    /// On-disk size of the persistent compositions redb in bytes.
    /// Runtime polls this periodically (every 30s) and updates the
    /// gauge from `std::fs::metadata`.
    pub redb_size_bytes: IntGauge,
    /// Live composition count in the persistent store.
    pub count: IntGauge,
    /// LRU cache hits in `PersistentRedbStorage::get`.
    pub lru_hit_total: IntCounter,
    /// LRU cache misses (fall through to redb read).
    pub lru_miss_total: IntCounter,
    /// LRU evictions (oldest entry pushed out by capacity).
    pub lru_evicted_total: IntCounter,
    /// Hydrator `apply_hydration_batch` duration, labeled by shard.
    /// Bucket choice mirrors the Phase 16 fabric histogram for
    /// dashboard consistency.
    pub hydrator_apply_duration: HistogramVec,
    /// Highest delta sequence durably applied per shard. Drives the
    /// "is the hydrator keeping up?" alarm.
    pub hydrator_last_applied_seq: IntGaugeVec,
    /// Permanent-skip counter, labeled by reason. See `skip_reason`.
    pub hydrator_skip_total: IntCounterVec,
    /// 1 when the hydrator is in halt mode (compaction outran us, or
    /// transient skip exhausted its retry budget). 0 otherwise.
    /// Latches on alarm — operator must drop the metadata redb +
    /// restart to clear.
    pub hydrator_stalled: IntGauge,
    /// redb `commit()` failures (out-of-space, fsync error, etc.).
    pub redb_commit_errors_total: IntCounter,
    /// Decode-path errors keyed by error kind. See `decode_kind`.
    pub decode_errors_total: IntCounterVec,
}

impl CompositionMetrics {
    /// Build the metrics and register them with `registry`.
    ///
    /// # Errors
    /// Returns `prometheus::Error` if any metric fails to register
    /// (typically a name collision in `registry`).
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let redb_size_bytes = IntGauge::new(
            "kiseki_composition_redb_size_bytes",
            "On-disk size of compositions.redb in bytes (refreshed every 30s).",
        )?;
        registry.register(Box::new(redb_size_bytes.clone()))?;

        let count = IntGauge::new(
            "kiseki_composition_count",
            "Live composition records in the persistent store.",
        )?;
        registry.register(Box::new(count.clone()))?;

        let lru_hit_total = IntCounter::new(
            "kiseki_composition_lru_hit_total",
            "Hot-tail LRU cache hits in PersistentRedbStorage::get.",
        )?;
        registry.register(Box::new(lru_hit_total.clone()))?;

        let lru_miss_total = IntCounter::new(
            "kiseki_composition_lru_miss_total",
            "Hot-tail LRU cache misses (fell through to redb).",
        )?;
        registry.register(Box::new(lru_miss_total.clone()))?;

        let lru_evicted_total = IntCounter::new(
            "kiseki_composition_lru_evicted_total",
            "Hot-tail LRU evictions (oldest entry pushed out by capacity).",
        )?;
        registry.register(Box::new(lru_evicted_total.clone()))?;

        let hydrator_apply_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_composition_hydrator_apply_duration_seconds",
                "Duration of one apply_hydration_batch (atomic redb commit).",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["shard"],
        )?;
        registry.register(Box::new(hydrator_apply_duration.clone()))?;

        let hydrator_last_applied_seq = IntGaugeVec::new(
            Opts::new(
                "kiseki_composition_hydrator_last_applied_seq",
                "Highest delta sequence applied per shard (durable from meta).",
            ),
            &["shard"],
        )?;
        registry.register(Box::new(hydrator_last_applied_seq.clone()))?;

        let hydrator_skip_total = IntCounterVec::new(
            Opts::new(
                "kiseki_composition_hydrator_skip_total",
                "Permanent skips: deltas the hydrator advanced past without applying.",
            ),
            &["reason"],
        )?;
        registry.register(Box::new(hydrator_skip_total.clone()))?;

        let hydrator_stalled = IntGauge::new(
            "kiseki_composition_hydrator_stalled",
            "1 when the hydrator is halted; 0 otherwise. Latches; operator clears.",
        )?;
        registry.register(Box::new(hydrator_stalled.clone()))?;

        let redb_commit_errors_total = IntCounter::new(
            "kiseki_composition_redb_commit_errors_total",
            "redb WriteTransaction::commit() failures (disk full, fsync error).",
        )?;
        registry.register(Box::new(redb_commit_errors_total.clone()))?;

        let decode_errors_total = IntCounterVec::new(
            Opts::new(
                "kiseki_composition_decode_errors_total",
                "Persistent-store decode-path failures by kind.",
            ),
            &["kind"],
        )?;
        registry.register(Box::new(decode_errors_total.clone()))?;

        Ok(Self {
            redb_size_bytes,
            count,
            lru_hit_total,
            lru_miss_total,
            lru_evicted_total,
            hydrator_apply_duration,
            hydrator_last_applied_seq,
            hydrator_skip_total,
            hydrator_stalled,
            redb_commit_errors_total,
            decode_errors_total,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_succeeds_in_fresh_registry() {
        let reg = Registry::new();
        let m = CompositionMetrics::register(&reg).expect("register ok");
        m.lru_hit_total.inc();
        m.lru_evicted_total.inc_by(3);
        m.hydrator_skip_total
            .with_label_values(&[skip_reason::EXHAUSTED_RETRIES])
            .inc();
        m.decode_errors_total
            .with_label_values(&[decode_kind::SCHEMA_TOO_NEW])
            .inc_by(2);
        m.hydrator_stalled.set(1);
        m.hydrator_last_applied_seq
            .with_label_values(&["00000000-0000-0000-0000-000000000001"])
            .set(42);
        assert_eq!(m.lru_hit_total.get(), 1);
        assert_eq!(m.lru_evicted_total.get(), 3);
        assert_eq!(m.hydrator_stalled.get(), 1);
    }

    #[test]
    fn register_twice_in_same_registry_fails() {
        let reg = Registry::new();
        let _m1 = CompositionMetrics::register(&reg).expect("first");
        let m2 = CompositionMetrics::register(&reg);
        assert!(m2.is_err());
    }
}
