//! Prometheus metrics for the key-manager context.
//!
//! Security-critical: rotation visibility, KEK fetch latency,
//! epoch lifecycle. Operators need to alert on rotation gaps
//! (key age >90 days) and slow KEK fetches (cache miss storm).
//!
//! 5 metrics:
//! 1. `kiseki_keymanager_rotation_total` — counter, incremented
//!    on every successful `rotate()`.
//! 2. `kiseki_keymanager_current_epoch` — gauge, the latest
//!    epoch known to this node. Should be identical across the
//!    cluster once Raft converges.
//! 3. `kiseki_keymanager_epoch_count` — gauge, total tracked
//!    epochs (current + retiring + migrated).
//! 4. `kiseki_keymanager_fetch_duration_seconds{outcome}` —
//!    histogram for `fetch_master_key`; outcome label
//!    distinguishes cache hits / misses / errors.
//! 5. `kiseki_keymanager_migration_complete_total` — counter,
//!    incremented when an epoch's migration completes.

use std::time::Duration;

use prometheus::{HistogramOpts, HistogramVec, IntCounter, IntGauge, Registry};

/// Outcome label values for `kiseki_keymanager_fetch_duration_seconds`.
pub mod outcome {
    /// Successful fetch.
    pub const OK: &str = "ok";
    /// Epoch not found in this node's state.
    pub const NOT_FOUND: &str = "not_found";
    /// Backend (Raft / persistence) unavailable.
    pub const UNAVAILABLE: &str = "unavailable";
}

/// Prometheus metrics for the key-manager context.
#[derive(Clone)]
pub struct KeyManagerMetrics {
    /// Total successful rotations (cumulative).
    pub rotation_total: IntCounter,
    /// Latest epoch known to this node (gauge).
    pub current_epoch: IntGauge,
    /// Number of epochs in the local state machine.
    pub epoch_count: IntGauge,
    /// `fetch_master_key` latency by outcome.
    pub fetch_duration: HistogramVec,
    /// Total epoch-migration-complete events.
    pub migration_complete_total: IntCounter,
}

impl KeyManagerMetrics {
    /// Build all metrics and register with `registry`.
    ///
    /// # Errors
    /// Returns `prometheus::Error` on name collisions.
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let rotation_total = IntCounter::new(
            "kiseki_keymanager_rotation_total",
            "Total successful key rotations on this node.",
        )?;
        registry.register(Box::new(rotation_total.clone()))?;

        let current_epoch = IntGauge::new(
            "kiseki_keymanager_current_epoch",
            "Latest key epoch known to this node.",
        )?;
        registry.register(Box::new(current_epoch.clone()))?;

        let epoch_count = IntGauge::new(
            "kiseki_keymanager_epoch_count",
            "Number of epochs tracked in this node's local state.",
        )?;
        registry.register(Box::new(epoch_count.clone()))?;

        let fetch_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_keymanager_fetch_duration_seconds",
                "fetch_master_key latency by outcome.",
            )
            .buckets(vec![
                0.00001, 0.00005, 0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1,
            ]),
            &["outcome"],
        )?;
        registry.register(Box::new(fetch_duration.clone()))?;

        let migration_complete_total = IntCounter::new(
            "kiseki_keymanager_migration_complete_total",
            "Total epoch-migration-complete events.",
        )?;
        registry.register(Box::new(migration_complete_total.clone()))?;

        Ok(Self {
            rotation_total,
            current_epoch,
            epoch_count,
            fetch_duration,
            migration_complete_total,
        })
    }

    /// Record a `fetch_master_key` outcome + duration.
    pub fn record_fetch(&self, outcome: &str, dur: Duration) {
        self.fetch_duration
            .with_label_values(&[outcome])
            .observe(dur.as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_and_record_round_trip() {
        let reg = Registry::new();
        let m = KeyManagerMetrics::register(&reg).expect("register ok");
        m.rotation_total.inc();
        m.current_epoch.set(7);
        m.epoch_count.set(3);
        m.record_fetch(outcome::OK, Duration::from_micros(50));
        m.migration_complete_total.inc();
        assert_eq!(m.rotation_total.get(), 1);
        assert_eq!(m.current_epoch.get(), 7);
        let names: std::collections::HashSet<_> =
            reg.gather().iter().map(|f| f.name().to_owned()).collect();
        for expected in &[
            "kiseki_keymanager_rotation_total",
            "kiseki_keymanager_current_epoch",
            "kiseki_keymanager_epoch_count",
            "kiseki_keymanager_fetch_duration_seconds",
            "kiseki_keymanager_migration_complete_total",
        ] {
            assert!(names.contains(*expected), "{expected} missing");
        }
    }
}
