//! Composition-side Prometheus metrics (ADR-040 §D10).
//!
//! Surface persistent-storage health (on-disk size, commit errors) and
//! hydrator health (apply latency, last-applied-seq per shard,
//! transient/permanent skip counts, halt flag).
//!
//! Pattern follows `kiseki_chunk_cluster::FabricMetrics` and
//! `kiseki_gateway::metrics::GatewayRetryMetrics`: the runtime
//! constructs one [`CompositionMetrics`] at startup, registers it
//! with the global `Registry`, and clones the `Arc` into the active
//! [`CompositionStorage`] backend (via `with_metrics`) and
//! [`CompositionHydrator`] (via `with_metrics`). Tests that
//! don't pass metrics get no-op behavior because the consumer
//! fields are `Option<Arc<CompositionMetrics>>`.
//!
//! Metric names are backend-neutral (`store_size_bytes`,
//! `store_commit_errors_total`, …). The fjall migration
//! 2026-05-06 dropped the redb-specific `redb_size_bytes` /
//! `redb_commit_*` / `lru_*` counters since the LSM has no outer
//! LRU and the WAL/manifest size is what dashboards care about.
//!
//! [`CompositionStorage`]: crate::persistent::CompositionStorage
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
    /// Catch-all for backend-specific table / batch / I/O errors.
    pub const BACKEND: &str = "backend";
}

/// Composition-side metrics surface (ADR-040 §D10).
#[derive(Clone)]
pub struct CompositionMetrics {
    /// On-disk footprint of the persistent composition store
    /// (recursive size of the store directory). Runtime polls every
    /// 30 s.
    pub store_size_bytes: IntGauge,
    /// Live composition count in the persistent store.
    pub count: IntGauge,
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
    /// Latches on alarm — operator must wipe the metadata directory
    /// + restart to clear.
    pub hydrator_stalled: IntGauge,
    /// Backend `commit()` / `persist()` failures (out-of-space,
    /// fsync error, etc.).
    pub store_commit_errors_total: IntCounter,
    /// Total successful backend commits driven by the runtime's
    /// periodic flusher and hydrator. Useful as a denominator for
    /// commit-error rate alarms.
    pub store_commits_total: IntCounter,
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
        let store_size_bytes = IntGauge::new(
            "kiseki_composition_store_size_bytes",
            "On-disk footprint of the composition store directory in bytes (refreshed every 30s).",
        )?;
        registry.register(Box::new(store_size_bytes.clone()))?;

        let count = IntGauge::new(
            "kiseki_composition_count",
            "Live composition records in the persistent store.",
        )?;
        registry.register(Box::new(count.clone()))?;

        let hydrator_apply_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_composition_hydrator_apply_duration_seconds",
                "Duration of one apply_hydration_batch (atomic backend commit).",
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

        let store_commit_errors_total = IntCounter::new(
            "kiseki_composition_store_commit_errors_total",
            "Composition-store commit/persist failures (disk full, fsync error).",
        )?;
        registry.register(Box::new(store_commit_errors_total.clone()))?;

        let store_commits_total = IntCounter::new(
            "kiseki_composition_store_commits_total",
            "Total successful composition-store commits (periodic flusher + hydrator).",
        )?;
        registry.register(Box::new(store_commits_total.clone()))?;

        let decode_errors_total = IntCounterVec::new(
            Opts::new(
                "kiseki_composition_decode_errors_total",
                "Persistent-store decode-path failures by kind.",
            ),
            &["kind"],
        )?;
        registry.register(Box::new(decode_errors_total.clone()))?;

        Ok(Self {
            store_size_bytes,
            count,
            hydrator_apply_duration,
            hydrator_last_applied_seq,
            hydrator_skip_total,
            hydrator_stalled,
            store_commit_errors_total,
            store_commits_total,
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
        m.store_commits_total.inc();
        m.store_commit_errors_total.inc_by(3);
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
        assert_eq!(m.store_commits_total.get(), 1);
        assert_eq!(m.store_commit_errors_total.get(), 3);
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
