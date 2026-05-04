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

use std::collections::HashMap;
use std::sync::Mutex;
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
    /// successful RPC since the last failure). Updated on every
    /// `record_op` from the per-peer state in [`Self::peer_state`].
    pub peers_up: IntGauge,
    /// Total quorum-lost events at the leader's write fan-out path.
    pub quorum_lost_total: prometheus::IntCounter,
    /// Sender-side `PutFragment` phase latency. Labels:
    /// `phase` ∈ {`proto_encode`, `transport`}. The 2026-05-04 GCP
    /// findings showed fabric PUT averaging 65–119 ms per 16 MiB
    /// extent without telling us *where* the time went; with this
    /// split + the receiver-side histogram below, an operator can
    /// subtract to derive the network-only component:
    /// `network ≈ send.transport - (recv.decode + recv.write_chunk)`.
    pub put_send_phase_duration: HistogramVec,
    /// Receiver-side `PutFragment` phase latency. Labels:
    /// `phase` ∈ {`decode`, `write_chunk`}. `decode` covers
    /// `proto_envelope_to_rust` plus the chunk-envelope-registry
    /// record (memory copies); `write_chunk` covers the local
    /// `AsyncChunkOps::write_chunk` / `write_fragment` call.
    pub put_recv_phase_duration: HistogramVec,
    /// Per-peer current health: `true` after a successful op, `false`
    /// after a non-OK / non-NOT_FOUND outcome. Drives the
    /// `peers_up` gauge. Lock is held only across very short
    /// `HashMap` reads/writes — never across IO.
    peer_state: std::sync::Arc<Mutex<HashMap<String, bool>>>,
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

        let put_send_phase_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_fabric_put_send_phase_duration_seconds",
                "Sender-side PutFragment phase latency: proto_encode, transport.",
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ]),
            &["phase"],
        )?;
        registry.register(Box::new(put_send_phase_duration.clone()))?;

        let put_recv_phase_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_fabric_put_recv_phase_duration_seconds",
                "Receiver-side PutFragment phase latency: decode, write_chunk.",
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ]),
            &["phase"],
        )?;
        registry.register(Box::new(put_recv_phase_duration.clone()))?;

        Ok(Self {
            ops_total,
            op_duration,
            peers_up,
            quorum_lost_total,
            put_send_phase_duration,
            put_recv_phase_duration,
            peer_state: std::sync::Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Record a sender-side `PutFragment` phase observation.
    /// `phase` is one of `"proto_encode"`, `"transport"`.
    pub fn observe_put_send(&self, phase: &str, dur: Duration) {
        self.put_send_phase_duration
            .with_label_values(&[phase])
            .observe(dur.as_secs_f64());
    }

    /// Record a receiver-side `PutFragment` phase observation.
    /// `phase` is one of `"decode"`, `"write_chunk"`.
    pub fn observe_put_recv(&self, phase: &str, dur: Duration) {
        self.put_recv_phase_duration
            .with_label_values(&[phase])
            .observe(dur.as_secs_f64());
    }

    /// Record a fabric op outcome + duration.
    ///
    /// Side-effect: refreshes the per-peer health map and re-counts
    /// `peers_up`. `OK` and `NOT_FOUND` count as healthy (the peer
    /// responded — `NOT_FOUND` is the protocol's "I don't have this
    /// fragment" response, not a failure). `UNAVAILABLE`,
    /// `REJECTED`, and `TRANSPORT` mark the peer as down.
    pub fn record_op(&self, op: &str, peer: &str, outcome: &str, dur: Duration) {
        self.ops_total.with_label_values(&[op, peer, outcome]).inc();
        self.op_duration
            .with_label_values(&[op])
            .observe(dur.as_secs_f64());
        let healthy = matches!(outcome, outcome::OK | outcome::NOT_FOUND);
        if let Ok(mut state) = self.peer_state.lock() {
            state.insert(peer.to_owned(), healthy);
            let up = state.values().filter(|v| **v).count();
            self.peers_up.set(i64::try_from(up).unwrap_or(i64::MAX));
        }
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
        // Touch the put-phase histograms so the registry gathers
        // them. `prometheus::Registry::gather` skips histograms
        // with zero observations on at least one label set.
        m.observe_put_send("proto_encode", Duration::from_micros(50));
        m.observe_put_recv("decode", Duration::from_micros(50));

        // Scrape — confirm presence + non-zero observations.
        let families = reg.gather();
        let names: std::collections::HashSet<_> =
            families.iter().map(|f| f.name().to_owned()).collect();
        assert!(names.contains("kiseki_fabric_ops_total"));
        assert!(names.contains("kiseki_fabric_op_duration_seconds"));
        assert!(names.contains("kiseki_fabric_peers_up"));
        assert!(names.contains("kiseki_fabric_quorum_lost_total"));
        assert!(names.contains("kiseki_fabric_put_send_phase_duration_seconds"));
        assert!(names.contains("kiseki_fabric_put_recv_phase_duration_seconds"));
        assert_eq!(m.quorum_lost_total.get(), 1);
    }

    /// The 2026-05-04 GCP perf cluster found fabric PUTs averaging
    /// 65–119 ms per 16 MiB extent without telling us where the time
    /// went. Pin the contract: a sender-side `proto_encode` /
    /// `transport` observation must reach the histogram, and the
    /// receiver-side `decode` / `write_chunk` observations must land
    /// on their separate histogram. Without this, an operator scraping
    /// `/metrics` cannot decompose the fabric PUT cost into its
    /// network-versus-CPU-versus-IO components.
    #[test]
    fn put_phase_histograms_observe_per_label() {
        let reg = Registry::new();
        let m = FabricMetrics::register(&reg).expect("register ok");

        m.observe_put_send("proto_encode", Duration::from_micros(150));
        m.observe_put_send("transport", Duration::from_millis(25));
        m.observe_put_recv("decode", Duration::from_micros(80));
        m.observe_put_recv("write_chunk", Duration::from_millis(15));

        for phase in ["proto_encode", "transport"] {
            let count = m
                .put_send_phase_duration
                .with_label_values(&[phase])
                .get_sample_count();
            assert!(
                count >= 1,
                "kiseki_fabric_put_send_phase_duration_seconds{{phase={phase}}} \
                 must observe at least one sample (got {count})",
            );
        }
        for phase in ["decode", "write_chunk"] {
            let count = m
                .put_recv_phase_duration
                .with_label_values(&[phase])
                .get_sample_count();
            assert!(
                count >= 1,
                "kiseki_fabric_put_recv_phase_duration_seconds{{phase={phase}}} \
                 must observe at least one sample (got {count})",
            );
        }
    }

    /// `kiseki_fabric_peers_up` should reflect the count of peers
    /// that have responded successfully recently. Currently it is
    /// stuck at 0 — the gauge is registered but never written. The
    /// 2026-05-03 GCP transport-profile run reported `peers_up=0`
    /// even while ~1500 successful peer ops were happening, because
    /// `record_op` doesn't track per-peer state and never touches
    /// the gauge. Pin the contract here: a successful op must move
    /// `peers_up` above 0.
    #[test]
    fn peers_up_reflects_successful_peer_ops() {
        let reg = Registry::new();
        let m = FabricMetrics::register(&reg).expect("register ok");
        // No ops yet → no peers up.
        assert_eq!(m.peers_up.get(), 0);

        // Two distinct peers each get a successful op. The gauge
        // should now read 2.
        m.record_op(op::PUT, "node-2", outcome::OK, Duration::from_millis(5));
        m.record_op(op::PUT, "node-3", outcome::OK, Duration::from_millis(5));
        assert_eq!(
            m.peers_up.get(),
            2,
            "after one successful PUT to node-2 and one to node-3 the \
             gauge should report 2 peers up; got {}",
            m.peers_up.get(),
        );
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
