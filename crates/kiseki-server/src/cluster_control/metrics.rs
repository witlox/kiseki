//! Prometheus metrics for the control-plane Raft group (ADR-033 §4).
//!
//! 6 metrics give operators visibility into the cluster-wide
//! topology mutation path:
//!
//! 1. `kiseki_cluster_control_submit_total{op, outcome}` — count of
//!    `client_write` calls per `ControlCommand` variant + outcome.
//! 2. `kiseki_cluster_control_submit_duration_seconds{op}` — submit
//!    latency including replication + apply on the leader.
//! 3. `kiseki_cluster_control_apply_total{op}` — count of state-
//!    machine applies per command variant. Sum across replicas
//!    should be `submit_count × replica_count`.
//! 4. `kiseki_cluster_control_apply_hook_duration_seconds{op}` —
//!    how long the per-replica apply hook takes (`create_shard`
//!    on every node is the heavy part).
//! 5. `kiseki_cluster_control_namespaces` — gauge of namespaces
//!    in the local state machine. Same value on every replica
//!    once consensus has converged.
//! 6. `kiseki_cluster_control_leader_forwarded_total{op}` — count
//!    of admin RPCs that this node forwarded to the cluster
//!    leader because it was a follower at the time.
//!
//! All `kiseki_cluster_control_*`-prefixed so they sort cleanly
//! alongside `kiseki_raft_transport_*` and `kiseki_gateway_*`.

use std::time::Duration;

use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry};

/// Op label values for the per-command counters / histograms.
pub mod op {
    /// `ControlCommand::CreateNamespace`.
    pub const CREATE_NAMESPACE: &str = "create_namespace";
    /// `ControlCommand::RecordSplit`.
    pub const RECORD_SPLIT: &str = "record_split";
    /// `ControlCommand::RecordMerge`.
    pub const RECORD_MERGE: &str = "record_merge";
    /// `ControlCommand::RetireShard`.
    pub const RETIRE_SHARD: &str = "retire_shard";
    /// ADR-049 `ControlCommand::UpsertNodeInventory`.
    pub const UPSERT_NODE_INVENTORY: &str = "upsert_node_inventory";
    /// ADR-049 `ControlCommand::SetPlacementPolicy`.
    pub const SET_PLACEMENT_POLICY: &str = "set_placement_policy";
    /// ADR-049 `ControlCommand::SetWorkloadParams`.
    pub const SET_WORKLOAD_PARAMS: &str = "set_workload_params";
}

/// Outcome label values for `kiseki_cluster_control_submit_total`.
pub mod outcome {
    /// `client_write` returned `Ok` and the entry committed.
    pub const OK: &str = "ok";
    /// `client_write` returned a "forward to leader" error.
    /// Sustained values during steady state indicate a leadership
    /// flap or a misconfigured admin client always landing on a
    /// follower (the admin RPC handler now forwards, so this
    /// label should only spike during election windows).
    pub const FORWARD: &str = "forward";
    /// Any other error. Includes "no leader" during election —
    /// the BDD `@leader-change` scenarios stress this.
    pub const ERROR: &str = "error";
}

/// 6-metric struct registered with the global prometheus
/// registry once at runtime startup. Cloned into the relevant
/// call sites: `OpenRaftControlStore::submit`, the state machine
/// apply path, and the storage-admin forwarding helpers.
#[derive(Clone)]
pub struct ClusterControlMetrics {
    /// Submit count by op + outcome.
    pub submit_total: IntCounterVec,
    /// Submit latency histogram (seconds) by op.
    pub submit_duration: HistogramVec,
    /// State-machine apply count by op (per-replica).
    pub apply_total: IntCounterVec,
    /// Apply-hook duration histogram (seconds) by op.
    pub apply_hook_duration: HistogramVec,
    /// Number of namespaces currently in the local state machine.
    pub namespaces: IntGauge,
    /// Admin RPC forward count by op.
    pub leader_forwarded_total: IntCounterVec,
}

impl ClusterControlMetrics {
    /// Build all 6 metrics and register them with `registry`.
    ///
    /// # Errors
    /// Returns `prometheus::Error` on name collisions.
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let submit_total = IntCounterVec::new(
            Opts::new(
                "kiseki_cluster_control_submit_total",
                "Control-plane Raft submit count by op and outcome.",
            ),
            &["op", "outcome"],
        )?;
        registry.register(Box::new(submit_total.clone()))?;

        let submit_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_cluster_control_submit_duration_seconds",
                "Control-plane Raft submit latency (replication + apply) by op.",
            )
            // Submits should land in 1-100 ms on a healthy cluster;
            // wider tail than the per-shard transport because the
            // apply hook does real work (create_shard on every
            // replica).
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ]),
            &["op"],
        )?;
        registry.register(Box::new(submit_duration.clone()))?;

        let apply_total = IntCounterVec::new(
            Opts::new(
                "kiseki_cluster_control_apply_total",
                "State-machine apply count by op (per-replica).",
            ),
            &["op"],
        )?;
        registry.register(Box::new(apply_total.clone()))?;

        let apply_hook_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_cluster_control_apply_hook_duration_seconds",
                "Per-replica apply hook duration by op.",
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
            ]),
            &["op"],
        )?;
        registry.register(Box::new(apply_hook_duration.clone()))?;

        let namespaces = IntGauge::new(
            "kiseki_cluster_control_namespaces",
            "Namespaces currently tracked in this replica's control-plane state machine.",
        )?;
        registry.register(Box::new(namespaces.clone()))?;

        let leader_forwarded_total = IntCounterVec::new(
            Opts::new(
                "kiseki_cluster_control_leader_forwarded_total",
                "Admin RPCs forwarded to the control-plane leader because this node was a follower.",
            ),
            &["op"],
        )?;
        registry.register(Box::new(leader_forwarded_total.clone()))?;

        Ok(Self {
            submit_total,
            submit_duration,
            apply_total,
            apply_hook_duration,
            namespaces,
            leader_forwarded_total,
        })
    }

    /// Record one submit completion.
    pub fn record_submit(&self, op: &str, outcome: &str, dur: Duration) {
        self.submit_total.with_label_values(&[op, outcome]).inc();
        self.submit_duration
            .with_label_values(&[op])
            .observe(dur.as_secs_f64());
    }

    /// Record one state-machine apply.
    pub fn record_apply(&self, op: &str) {
        self.apply_total.with_label_values(&[op]).inc();
    }

    /// Record an apply-hook duration sample.
    pub fn record_hook_duration(&self, op: &str, dur: Duration) {
        self.apply_hook_duration
            .with_label_values(&[op])
            .observe(dur.as_secs_f64());
    }

    /// Record an admin RPC forward.
    pub fn record_forwarded(&self, op: &str) {
        self.leader_forwarded_total.with_label_values(&[op]).inc();
    }

    /// Map a `ControlCommand` to its `op` label.
    #[must_use]
    pub fn op_label(cmd: &super::ControlCommand) -> &'static str {
        match cmd {
            super::ControlCommand::CreateNamespace { .. } => op::CREATE_NAMESPACE,
            super::ControlCommand::RecordSplit { .. } => op::RECORD_SPLIT,
            super::ControlCommand::RecordMerge { .. } => op::RECORD_MERGE,
            super::ControlCommand::RetireShard { .. } => op::RETIRE_SHARD,
            super::ControlCommand::UpsertNodeInventory { .. } => op::UPSERT_NODE_INVENTORY,
            super::ControlCommand::SetPlacementPolicy { .. } => op::SET_PLACEMENT_POLICY,
            super::ControlCommand::SetWorkloadParams { .. } => op::SET_WORKLOAD_PARAMS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_and_record_round_trip() {
        let reg = Registry::new();
        let m = ClusterControlMetrics::register(&reg).expect("register ok");

        m.record_submit(op::RECORD_SPLIT, outcome::OK, Duration::from_millis(5));
        m.record_apply(op::RECORD_SPLIT);
        m.record_hook_duration(op::RECORD_SPLIT, Duration::from_micros(150));
        m.record_forwarded(op::RECORD_SPLIT);
        m.namespaces.set(2);

        assert_eq!(
            m.submit_total
                .with_label_values(&[op::RECORD_SPLIT, outcome::OK])
                .get(),
            1,
        );
        assert_eq!(
            m.apply_total.with_label_values(&[op::RECORD_SPLIT]).get(),
            1
        );
        assert_eq!(
            m.leader_forwarded_total
                .with_label_values(&[op::RECORD_SPLIT])
                .get(),
            1,
        );
        assert_eq!(m.namespaces.get(), 2);

        // /metrics gather pulls all 6 metric families.
        let names: std::collections::HashSet<_> =
            reg.gather().iter().map(|f| f.name().to_owned()).collect();
        for expected in &[
            "kiseki_cluster_control_submit_total",
            "kiseki_cluster_control_submit_duration_seconds",
            "kiseki_cluster_control_apply_total",
            "kiseki_cluster_control_apply_hook_duration_seconds",
            "kiseki_cluster_control_namespaces",
            "kiseki_cluster_control_leader_forwarded_total",
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
        let _m1 = ClusterControlMetrics::register(&reg).expect("first");
        let m2 = ClusterControlMetrics::register(&reg);
        assert!(m2.is_err(), "second register on the same registry must Err");
    }
}
