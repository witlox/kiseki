//! Prometheus metrics for the control plane (ADR-027 + ADR-033).
//!
//! Exposes operator-facing visibility into tenant ops, namespace
//! topology mutations, and ratio-floor evaluations. Names are
//! `kiseki_control_*`-prefixed.
//!
//! 5 metrics:
//! 1. `kiseki_control_tenant_ops_total{op, outcome}` — admin tenant
//!    operations (`create_pool`, `list_pools`, `add_device`, etc.).
//! 2. `kiseki_control_namespace_create_total{outcome}` — namespace
//!    creations through `NamespaceShardMapStore::create_namespace`.
//! 3. `kiseki_control_namespaces` — gauge, total namespaces in the
//!    local in-memory map. Mirrors what
//!    `cluster_control::ClusterControlMetrics::namespaces` reports
//!    when the control-plane Raft is wired (ADR-033 §4).
//! 4. `kiseki_control_ratio_floor_evaluations_total{outcome}` —
//!    `evaluate_ratio_floor` calls; outcome `triggered_split` /
//!    `no_op` / `error` so dashboards can alert on continuous
//!    splitting.
//! 5. `kiseki_control_alias_total` — counter, namespace aliases
//!    created. Useful when chasing routing-cache invalidation
//!    bugs.

use prometheus::{IntCounter, IntCounterVec, IntGauge, Opts, Registry};

/// Outcome label values.
pub mod outcome {
    /// Operation completed successfully.
    pub const OK: &str = "ok";
    /// Authorization rejected (`AdminError::Forbidden`).
    pub const FORBIDDEN: &str = "forbidden";
    /// Validation rejected (`AdminError::Rejected`).
    pub const REJECTED: &str = "rejected";
    /// Conflict (`AdminError::AlreadyExists`).
    pub const ALREADY_EXISTS: &str = "already_exists";
    /// Resource not found (`AdminError::NotFound`).
    pub const NOT_FOUND: &str = "not_found";
    /// `evaluate_ratio_floor`: a split was triggered.
    pub const TRIGGERED_SPLIT: &str = "triggered_split";
    /// `evaluate_ratio_floor`: no action needed.
    pub const NO_OP: &str = "no_op";
    /// Unclassified error.
    pub const ERROR: &str = "error";
}

/// Op label values for `kiseki_control_tenant_ops_total`.
pub mod op {
    /// Pool create.
    pub const CREATE_POOL: &str = "create_pool";
    /// Pool list (read).
    pub const LIST_POOLS: &str = "list_pools";
    /// Device add.
    pub const ADD_DEVICE: &str = "add_device";
    /// Device list.
    pub const LIST_DEVICES: &str = "list_devices";
    /// Pool delete.
    pub const DELETE_POOL: &str = "delete_pool";
    /// Tenant bounds set.
    pub const SET_TENANT_BOUNDS: &str = "set_tenant_bounds";
}

/// Prometheus metrics for the control plane.
#[derive(Clone)]
pub struct ControlMetrics {
    /// Per-(op, outcome) tenant operation count.
    pub tenant_ops_total: IntCounterVec,
    /// Per-outcome namespace create count.
    pub namespace_create_total: IntCounterVec,
    /// Active namespaces (gauge).
    pub namespaces: IntGauge,
    /// Per-outcome ratio-floor evaluation count.
    pub ratio_floor_evaluations_total: IntCounterVec,
    /// Total namespace aliases created.
    pub alias_total: IntCounter,
}

impl ControlMetrics {
    /// Build all metrics and register with `registry`.
    ///
    /// # Errors
    /// Returns `prometheus::Error` on name collisions.
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let tenant_ops_total = IntCounterVec::new(
            Opts::new(
                "kiseki_control_tenant_ops_total",
                "Tenant-facing admin operations by op and outcome (ADR-027).",
            ),
            &["op", "outcome"],
        )?;
        registry.register(Box::new(tenant_ops_total.clone()))?;

        let namespace_create_total = IntCounterVec::new(
            Opts::new(
                "kiseki_control_namespace_create_total",
                "Namespace creations through NamespaceShardMapStore.",
            ),
            &["outcome"],
        )?;
        registry.register(Box::new(namespace_create_total.clone()))?;

        let namespaces = IntGauge::new(
            "kiseki_control_namespaces",
            "Active namespaces in the local NamespaceShardMapStore.",
        )?;
        registry.register(Box::new(namespaces.clone()))?;

        let ratio_floor_evaluations_total = IntCounterVec::new(
            Opts::new(
                "kiseki_control_ratio_floor_evaluations_total",
                "evaluate_ratio_floor calls by outcome (ADR-033).",
            ),
            &["outcome"],
        )?;
        registry.register(Box::new(ratio_floor_evaluations_total.clone()))?;

        let alias_total = IntCounter::new(
            "kiseki_control_alias_total",
            "Total namespace aliases created.",
        )?;
        registry.register(Box::new(alias_total.clone()))?;

        Ok(Self {
            tenant_ops_total,
            namespace_create_total,
            namespaces,
            ratio_floor_evaluations_total,
            alias_total,
        })
    }

    /// Record a tenant op outcome.
    pub fn record_tenant_op(&self, op_label: &str, outcome_label: &str) {
        self.tenant_ops_total
            .with_label_values(&[op_label, outcome_label])
            .inc();
    }

    /// Record a namespace creation outcome.
    pub fn record_namespace_create(&self, outcome_label: &str) {
        self.namespace_create_total
            .with_label_values(&[outcome_label])
            .inc();
    }

    /// Record a ratio-floor evaluation outcome.
    pub fn record_ratio_floor_evaluation(&self, outcome_label: &str) {
        self.ratio_floor_evaluations_total
            .with_label_values(&[outcome_label])
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_and_record_round_trip() {
        let reg = Registry::new();
        let m = ControlMetrics::register(&reg).expect("register ok");
        m.record_tenant_op(op::CREATE_POOL, outcome::OK);
        m.record_namespace_create(outcome::OK);
        m.record_ratio_floor_evaluation(outcome::TRIGGERED_SPLIT);
        m.namespaces.set(2);
        m.alias_total.inc();
        assert_eq!(
            m.tenant_ops_total
                .with_label_values(&[op::CREATE_POOL, outcome::OK])
                .get(),
            1,
        );
        let names: std::collections::HashSet<_> =
            reg.gather().iter().map(|f| f.name().to_owned()).collect();
        for expected in &[
            "kiseki_control_tenant_ops_total",
            "kiseki_control_namespace_create_total",
            "kiseki_control_namespaces",
            "kiseki_control_ratio_floor_evaluations_total",
            "kiseki_control_alias_total",
        ] {
            assert!(names.contains(*expected), "{expected} missing");
        }
    }
}
