//! Prometheus metrics for the multiplexed Raft RPC transport.
//!
//! Per ADR-041 §"Observability": 8 metrics surface listener health
//! and per-shard RPC traffic. All `kiseki_raft_transport_*`-prefixed
//! so they sort cleanly alongside `kiseki_fabric_*` and
//! `kiseki_gateway_*` in `/metrics`.
//!
//! The kiseki-server runtime constructs one [`RaftTransportMetrics`]
//! at startup, registers it with the global registry, and threads
//! the `Arc` into every `RaftRpcListener` via `with_metrics`. None
//! is a no-op (tests + library users without metrics configured
//! aren't forced to set it).

use std::time::Duration;

use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
};

/// Outcome label values for `kiseki_raft_transport_rpc_total`.
pub mod outcome {
    /// Dispatcher returned a typed response (status `0x00`).
    pub const OK: &str = "ok";
    /// No registry entry for the requested `shard_id` (status `0x01`).
    /// High values during in-flight membership changes; persistently
    /// high values indicate stale peer caches.
    pub const UNKNOWN_SHARD: &str = "unknown_shard";
    /// Request frame malformed (status `0x02`).
    pub const PARSE_ERROR: &str = "parse_error";
    /// Dispatcher panicked (status `0x03`).
    pub const DISPATCHER_PANIC: &str = "dispatcher_panic";
}

/// Op label values for the dispatcher.
pub mod op {
    /// `AppendEntries`.
    pub const APPEND_ENTRIES: &str = "append_entries";
    /// `Vote`.
    pub const VOTE: &str = "vote";
    /// `InstallFullSnapshot`.
    pub const FULL_SNAPSHOT: &str = "full_snapshot";
    /// ADR-047 aux tag `intent_put` — the producer's quorum intent-write
    /// (per-PUT hot path). Tag string defined in
    /// `kiseki_log::intent_sync::INTENT_PUT_TAG`; kept literal here to keep
    /// `kiseki-raft` free of an upward dependency on `kiseki-log`.
    pub const INTENT_PUT: &str = "intent_put";
    /// ADR-047 aux tag `intent_gather_pending` — leader-recovery O2 gather
    /// (cold path; one round per election). Tag string defined in
    /// `kiseki_log::intent_sync::INTENT_GATHER_PENDING_TAG`.
    pub const INTENT_GATHER_PENDING: &str = "intent_gather_pending";
    /// P3 aux tag `consumer_positions` — the shard leader's
    /// watermark-advance round reading a voter's node-local reported
    /// consumer positions (cold path; one fan per round, ~5 s). Tag
    /// string defined in
    /// `kiseki_log::intent_sync::CONSUMER_POSITIONS_TAG`.
    pub const CONSUMER_POSITIONS: &str = "consumer_positions";
    /// Unknown / unrecognized tag.
    pub const UNKNOWN: &str = "unknown";
}

/// 8 Prometheus metrics for the multiplexed Raft transport.
#[derive(Clone)]
pub struct RaftTransportMetrics {
    /// Per-RPC count, labeled by (`shard`, `op`, `outcome`).
    pub rpc_total: IntCounterVec,
    /// Per-RPC server-side latency histogram, labeled by (`shard`, `op`).
    pub rpc_duration: HistogramVec,
    /// Active shard count on this listener (matches
    /// `RegistryHandle::size()`).
    pub registry_size: IntGauge,
    /// Aggregate count of `unknown_shard` responses across all
    /// shards. Pairs with `rpc_total{outcome="unknown_shard"}` for
    /// quick scrape.
    pub unknown_shard_total: IntCounter,
    /// Listener supervisor restarts (gate-1 F-H3). Should stay 0 in
    /// steady state.
    pub listener_restarts_total: IntCounter,
    /// Per-task panics caught by `catch_unwind` in the dispatcher.
    /// Labeled by (`shard`, `op`) so a misbehaving Raft instance is
    /// identifiable.
    pub dispatcher_panic_total: IntCounterVec,
    /// Per-peer connection-cap exceedances (gate-1 F-M5). Labeled
    /// by `peer` (IP address).
    pub connection_cap_exceeded_total: IntCounterVec,
    /// Current accepted-connection count on this listener.
    pub active_connections: IntGauge,
}

impl RaftTransportMetrics {
    /// Build all 8 metrics and register them with `registry`.
    ///
    /// # Errors
    /// Returns `prometheus::Error` on name collisions.
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let rpc_total = IntCounterVec::new(
            Opts::new(
                "kiseki_raft_transport_rpc_total",
                "Multiplexed Raft RPC count by shard, op, and outcome.",
            ),
            &["shard", "op", "outcome"],
        )?;
        registry.register(Box::new(rpc_total.clone()))?;

        let rpc_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_raft_transport_rpc_duration_seconds",
                "Multiplexed Raft RPC server-side latency by shard and op.",
            )
            // Tighter bucket distribution than the fabric metrics —
            // Raft RPCs are small heartbeats + log appends, target
            // sub-millisecond on a healthy LAN.
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
            ]),
            &["shard", "op"],
        )?;
        registry.register(Box::new(rpc_duration.clone()))?;

        let registry_size = IntGauge::new(
            "kiseki_raft_transport_registry_size",
            "Active shard count registered with this Raft RPC listener.",
        )?;
        registry.register(Box::new(registry_size.clone()))?;

        let unknown_shard_total = IntCounter::new(
            "kiseki_raft_transport_unknown_shard_total",
            "Inbound RPCs targeted at a shard_id not in the registry. \
             Sustained values indicate stale peer caches (ADR-034 grace \
             period not refreshing).",
        )?;
        registry.register(Box::new(unknown_shard_total.clone()))?;

        let listener_restarts_total = IntCounter::new(
            "kiseki_raft_transport_listener_restarts_total",
            "Listener supervisor restarts (ADR-041 gate-1 F-H3). \
             Should stay 0 in steady state.",
        )?;
        registry.register(Box::new(listener_restarts_total.clone()))?;

        let dispatcher_panic_total = IntCounterVec::new(
            Opts::new(
                "kiseki_raft_transport_dispatcher_panic_total",
                "Per-task panics caught by catch_unwind in the dispatcher \
                 (ADR-041 gate-1 F-H3). Listener stays up; caller sees \
                 status 0x03.",
            ),
            &["shard", "op"],
        )?;
        registry.register(Box::new(dispatcher_panic_total.clone()))?;

        let connection_cap_exceeded_total = IntCounterVec::new(
            Opts::new(
                "kiseki_raft_transport_connection_cap_exceeded_total",
                "Per-peer connection-cap exceedances (ADR-041 gate-1 \
                 F-M5). Labeled by peer IP.",
            ),
            &["peer"],
        )?;
        registry.register(Box::new(connection_cap_exceeded_total.clone()))?;

        let active_connections = IntGauge::new(
            "kiseki_raft_transport_active_connections",
            "Current accepted-connection count on this Raft RPC listener.",
        )?;
        registry.register(Box::new(active_connections.clone()))?;

        Ok(Self {
            rpc_total,
            rpc_duration,
            registry_size,
            unknown_shard_total,
            listener_restarts_total,
            dispatcher_panic_total,
            connection_cap_exceeded_total,
            active_connections,
        })
    }

    /// Record one inbound RPC completion. Increments
    /// `rpc_total{shard, op, outcome}` and observes the duration on
    /// `rpc_duration{shard, op}`. `unknown_shard_total` ticks
    /// alongside the labeled counter for the `unknown_shard` outcome
    /// so operators can scrape either.
    pub fn record_rpc(&self, shard_id: &str, op: &str, outcome: &str, dur: Duration) {
        self.rpc_total
            .with_label_values(&[shard_id, op, outcome])
            .inc();
        self.rpc_duration
            .with_label_values(&[shard_id, op])
            .observe(dur.as_secs_f64());
        if outcome == outcome::UNKNOWN_SHARD {
            self.unknown_shard_total.inc();
        }
    }

    /// Record a dispatcher panic per (shard, op).
    pub fn record_dispatcher_panic(&self, shard_id: &str, op: &str) {
        self.dispatcher_panic_total
            .with_label_values(&[shard_id, op])
            .inc();
    }

    /// Record a per-peer connection-cap exceedance.
    pub fn record_connection_cap_exceeded(&self, peer: &str) {
        self.connection_cap_exceeded_total
            .with_label_values(&[peer])
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All 8 metrics register cleanly to a fresh registry; observe
    /// helpers tick the right counter labels.
    #[test]
    fn metrics_register_and_record_round_trip() {
        let reg = Registry::new();
        let m = RaftTransportMetrics::register(&reg).expect("register ok");

        m.record_rpc(
            "test-shard",
            op::APPEND_ENTRIES,
            outcome::OK,
            Duration::from_micros(50),
        );
        m.record_rpc(
            "retired-shard",
            op::APPEND_ENTRIES,
            outcome::UNKNOWN_SHARD,
            Duration::from_micros(10),
        );
        m.record_dispatcher_panic("test-shard", op::VOTE);
        m.record_connection_cap_exceeded("10.0.0.42");
        m.listener_restarts_total.inc();
        m.registry_size.set(7);
        m.active_connections.set(3);

        // Counters reachable via label lookup.
        assert_eq!(
            m.rpc_total
                .with_label_values(&["test-shard", op::APPEND_ENTRIES, outcome::OK])
                .get(),
            1,
        );
        assert_eq!(
            m.rpc_total
                .with_label_values(&["retired-shard", op::APPEND_ENTRIES, outcome::UNKNOWN_SHARD])
                .get(),
            1,
        );
        assert_eq!(
            m.unknown_shard_total.get(),
            1,
            "UNKNOWN_SHARD outcome must also tick the aggregate counter \
             so a single scrape reveals stale-cache pressure",
        );
        assert_eq!(
            m.dispatcher_panic_total
                .with_label_values(&["test-shard", op::VOTE])
                .get(),
            1
        );
        assert_eq!(m.listener_restarts_total.get(), 1);
        assert_eq!(m.registry_size.get(), 7);
        assert_eq!(m.active_connections.get(), 3);
        assert_eq!(
            m.connection_cap_exceeded_total
                .with_label_values(&["10.0.0.42"])
                .get(),
            1,
        );

        // /metrics gather pulls all 8 metric families.
        let families = reg.gather();
        let names: std::collections::HashSet<_> =
            families.iter().map(|f| f.name().to_owned()).collect();
        for expected in &[
            "kiseki_raft_transport_rpc_total",
            "kiseki_raft_transport_rpc_duration_seconds",
            "kiseki_raft_transport_registry_size",
            "kiseki_raft_transport_unknown_shard_total",
            "kiseki_raft_transport_listener_restarts_total",
            "kiseki_raft_transport_dispatcher_panic_total",
            "kiseki_raft_transport_connection_cap_exceeded_total",
            "kiseki_raft_transport_active_connections",
        ] {
            assert!(
                names.contains(*expected),
                "metric {expected} not registered",
            );
        }
    }

    #[test]
    fn double_register_returns_error_not_panic() {
        let reg = Registry::new();
        let _m1 = RaftTransportMetrics::register(&reg).expect("first");
        let m2 = RaftTransportMetrics::register(&reg);
        assert!(m2.is_err(), "second register on the same registry must Err");
    }
}
