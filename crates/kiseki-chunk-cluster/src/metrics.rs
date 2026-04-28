//! Prometheus metrics for the cluster chunk fabric.
//!
//! Phase 16a step 11. Defines the per-op counters / histograms /
//! gauges that surface fabric health under
//! `/metrics` (kiseki-server's existing endpoint). The kiseki-server
//! runtime constructs one [`FabricMetrics`] at startup, registers it
//! with the global registry, and clones the `Arc` into the
//! [`ClusteredChunkStore`][cs] and [`GrpcFabricPeer`][gp]. Both
//! treat `Option<Arc<FabricMetrics>>` as no-op when None — tests stay
//! cheap and the unit tests in steps 1–6 don't need to thread metrics
//! through.
//!
//! [cs]: crate::ClusteredChunkStore
//! [gp]: crate::peer::GrpcFabricPeer

use std::time::Duration;

use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry};

/// Outcome label values for `kiseki_fabric_ops_total`.
pub mod outcome {
    /// RPC succeeded.
    pub const OK: &str = "ok";
    /// Fragment not found on peer (real signal — not a failure).
    pub const NOT_FOUND: &str = "not_found";
    /// Peer unreachable / timed out.
    pub const UNAVAILABLE: &str = "unavailable";
    /// Peer rejected the call (auth / SAN failure).
    pub const REJECTED: &str = "rejected";
    /// Other transport / protocol error.
    pub const TRANSPORT: &str = "transport";
}

/// Op label values for `kiseki_fabric_ops_total` and
/// `kiseki_fabric_op_duration_seconds`.
pub mod op {
    /// `PutFragment`.
    pub const PUT: &str = "put";
    /// `GetFragment`.
    pub const GET: &str = "get";
    /// `DeleteFragment`.
    pub const DELETE: &str = "delete";
    /// `HasFragment`.
    pub const HAS: &str = "has";
}

/// Collection of Prometheus metrics for the cluster fabric.
#[derive(Clone)]
pub struct FabricMetrics {
    /// Per-op outcome counter, labeled by (op, peer, outcome).
    pub ops_total: IntCounterVec,
    /// Per-op latency histogram, labeled by op.
    pub op_duration: HistogramVec,
    /// Healthy peer count (peers that have answered at least one
    /// successful RPC since the last failure).
    pub peers_up: IntGauge,
    /// Total quorum-lost events at the leader's write fan-out path.
    pub quorum_lost_total: prometheus::IntCounter,
}

impl FabricMetrics {
    /// Build the metrics and register them with `registry`. Names use
    /// the `kiseki_fabric_*` prefix so `/metrics` filters cleanly by
    /// subsystem.
    ///
    /// # Errors
    /// Returns `prometheus::Error` if any metric fails to register
    /// (typically a name collision in `registry`).
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let ops_total = IntCounterVec::new(
            Opts::new(
                "kiseki_fabric_ops_total",
                "Cluster-fabric RPC count by op, peer, and outcome.",
            ),
            &["op", "peer", "outcome"],
        )?;
        registry.register(Box::new(ops_total.clone()))?;

        let op_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_fabric_op_duration_seconds",
                "Cluster-fabric RPC latency by op.",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["op"],
        )?;
        registry.register(Box::new(op_duration.clone()))?;

        let peers_up = IntGauge::new(
            "kiseki_fabric_peers_up",
            "Healthy fabric peer count (last-call success).",
        )?;
        registry.register(Box::new(peers_up.clone()))?;

        let quorum_lost_total = prometheus::IntCounter::new(
            "kiseki_fabric_quorum_lost_total",
            "Writes that failed to reach the configured min_acks.",
        )?;
        registry.register(Box::new(quorum_lost_total.clone()))?;

        Ok(Self {
            ops_total,
            op_duration,
            peers_up,
            quorum_lost_total,
        })
    }

    /// Record a fabric op outcome + duration.
    pub fn record_op(&self, op: &str, peer: &str, outcome: &str, dur: Duration) {
        self.ops_total.with_label_values(&[op, peer, outcome]).inc();
        self.op_duration
            .with_label_values(&[op])
            .observe(dur.as_secs_f64());
    }

    /// Record a quorum-lost write.
    pub fn record_quorum_lost(&self) {
        self.quorum_lost_total.inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_to_a_fresh_registry() {
        let reg = Registry::new();
        let m = FabricMetrics::register(&reg).expect("register ok");
        m.record_op(op::PUT, "node-2", outcome::OK, Duration::from_millis(7));
        m.record_op(
            op::GET,
            "node-3",
            outcome::NOT_FOUND,
            Duration::from_millis(2),
        );
        m.record_quorum_lost();

        // Scrape — confirm presence + non-zero observations.
        let families = reg.gather();
        let names: std::collections::HashSet<_> =
            families.iter().map(|f| f.name().to_owned()).collect();
        assert!(names.contains("kiseki_fabric_ops_total"));
        assert!(names.contains("kiseki_fabric_op_duration_seconds"));
        assert!(names.contains("kiseki_fabric_peers_up"));
        assert!(names.contains("kiseki_fabric_quorum_lost_total"));
        assert_eq!(m.quorum_lost_total.get(), 1);
    }

    #[test]
    fn double_register_returns_error_not_panic() {
        let reg = Registry::new();
        let _m1 = FabricMetrics::register(&reg).expect("first");
        let m2 = FabricMetrics::register(&reg);
        assert!(m2.is_err(), "second register on the same registry must Err");
    }

    #[test]
    fn record_op_increments_counter_per_label_set() {
        let reg = Registry::new();
        let m = FabricMetrics::register(&reg).unwrap();
        m.record_op(op::PUT, "node-2", outcome::OK, Duration::from_millis(1));
        m.record_op(op::PUT, "node-2", outcome::OK, Duration::from_millis(1));
        m.record_op(op::PUT, "node-3", outcome::OK, Duration::from_millis(1));

        let counter_n2 = m
            .ops_total
            .with_label_values(&[op::PUT, "node-2", outcome::OK])
            .get();
        let counter_n3 = m
            .ops_total
            .with_label_values(&[op::PUT, "node-3", outcome::OK])
            .get();
        assert_eq!(counter_n2, 2);
        assert_eq!(counter_n3, 1);
    }
}
