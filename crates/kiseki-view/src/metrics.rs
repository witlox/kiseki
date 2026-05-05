//! Prometheus metrics for the View context (NFS materialization).
//!
//! Operators care about materialization lag (how far behind the
//! Log tip a view is) and watermark progression rate (is the
//! poller actually consuming?).
//!
//! 5 metrics:
//! 1. `kiseki_view_versions_added_total` — total `add_version`
//!    calls, cumulative across all views.
//! 2. `kiseki_view_versions_deleted_total` — total
//!    `delete_version` calls.
//! 3. `kiseki_view_objects` — gauge, distinct objects across
//!    all views on this node.
//! 4. `kiseki_view_total_versions` — gauge, total versions
//!    across all views (objects × versions-per-object).
//! 5. `kiseki_view_poll_deltas_total{shard}` — counter of
//!    deltas the stream processor pulled from a shard. Lag
//!    (= shard tip − last polled) is computed at scrape time
//!    by joining this with `kiseki_log_truncate_boundary_seq`.

use prometheus::{IntCounter, IntCounterVec, IntGauge, Opts, Registry};

/// Prometheus metrics for the View context.
#[derive(Clone)]
pub struct ViewMetrics {
    /// Total versions added across all views.
    pub versions_added_total: IntCounter,
    /// Total versions deleted across all views.
    pub versions_deleted_total: IntCounter,
    /// Distinct objects across all views.
    pub objects: IntGauge,
    /// Total versions (objects × versions-per-object).
    pub total_versions: IntGauge,
    /// Per-shard delta poll count by `StreamProcessor::poll`.
    pub poll_deltas_total: IntCounterVec,
}

impl ViewMetrics {
    /// Build all metrics and register with `registry`.
    ///
    /// # Errors
    /// Returns `prometheus::Error` on name collisions.
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let versions_added_total = IntCounter::new(
            "kiseki_view_versions_added_total",
            "Total versions added across all views.",
        )?;
        registry.register(Box::new(versions_added_total.clone()))?;

        let versions_deleted_total = IntCounter::new(
            "kiseki_view_versions_deleted_total",
            "Total versions deleted across all views.",
        )?;
        registry.register(Box::new(versions_deleted_total.clone()))?;

        let objects = IntGauge::new(
            "kiseki_view_objects",
            "Distinct objects across all views on this node.",
        )?;
        registry.register(Box::new(objects.clone()))?;

        let total_versions = IntGauge::new(
            "kiseki_view_total_versions",
            "Total object versions across all views on this node.",
        )?;
        registry.register(Box::new(total_versions.clone()))?;

        let poll_deltas_total = IntCounterVec::new(
            Opts::new(
                "kiseki_view_poll_deltas_total",
                "Per-shard delta count consumed by the StreamProcessor.",
            ),
            &["shard"],
        )?;
        registry.register(Box::new(poll_deltas_total.clone()))?;

        Ok(Self {
            versions_added_total,
            versions_deleted_total,
            objects,
            total_versions,
            poll_deltas_total,
        })
    }

    /// Record a delta-poll cycle on the given shard.
    pub fn record_poll(&self, shard_id: &str, count: u64) {
        self.poll_deltas_total
            .with_label_values(&[shard_id])
            .inc_by(count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_and_record_round_trip() {
        let reg = Registry::new();
        let m = ViewMetrics::register(&reg).expect("register ok");
        m.versions_added_total.inc();
        m.versions_deleted_total.inc();
        m.objects.set(10);
        m.total_versions.set(25);
        m.record_poll("s-1", 7);
        let names: std::collections::HashSet<_> =
            reg.gather().iter().map(|f| f.name().to_owned()).collect();
        for expected in &[
            "kiseki_view_versions_added_total",
            "kiseki_view_versions_deleted_total",
            "kiseki_view_objects",
            "kiseki_view_total_versions",
            "kiseki_view_poll_deltas_total",
        ] {
            assert!(names.contains(*expected), "{expected} missing");
        }
    }
}
