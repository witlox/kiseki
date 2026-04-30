//! Gateway-side Prometheus metrics (ADR-040 §D7 + §D10 — F-4 closure).
//!
//! Two counters that surface the read-path retry budget's pressure:
//!
//!   `kiseki_gateway_read_retry_total`
//!     Incremented on every `read()` that exited the retry loop with
//!     a hit. Steady-state rate ≈ cross-gateway RYW read rate.
//!
//!   `kiseki_gateway_read_retry_exhausted_total`
//!     Incremented on every `read()` that hit
//!     `KISEKI_GATEWAY_READ_RETRY_BUDGET_MS` without resolving. A
//!     non-zero rate means the budget is too tight for the current
//!     hydrator latency. Operators alarm on this and either bump the
//!     budget or investigate `kiseki_composition_hydrator_*` (which
//!     land with the §D10 follow-up).
//!
//! Pattern follows `kiseki_chunk_cluster::FabricMetrics`: the runtime
//! constructs one [`GatewayRetryMetrics`] at startup, registers it
//! with the global `Registry`, and clones the `Arc` into the
//! gateway via [`InMemoryGateway::with_retry_metrics`]. Tests that
//! don't pass metrics get no-op behavior because the gateway field
//! is `Option<Arc<GatewayRetryMetrics>>`.

use prometheus::{IntCounter, Registry};

/// Read-path retry counters. See module docs.
#[derive(Clone)]
pub struct GatewayRetryMetrics {
    /// Reads that exited the retry loop with a hit (any number of
    /// retries, including zero).
    pub read_retry_total: IntCounter,
    /// Reads that exhausted the retry budget without resolving.
    pub read_retry_exhausted_total: IntCounter,
}

impl GatewayRetryMetrics {
    /// Build the counters and register them with `registry`.
    ///
    /// # Errors
    /// Returns `prometheus::Error` if any metric fails to register
    /// (typically a name collision in `registry`).
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let read_retry_total = IntCounter::new(
            "kiseki_gateway_read_retry_total",
            "Reads that exited the retry loop with a hit.",
        )?;
        registry.register(Box::new(read_retry_total.clone()))?;

        let read_retry_exhausted_total = IntCounter::new(
            "kiseki_gateway_read_retry_exhausted_total",
            "Reads that exhausted the retry budget without resolving.",
        )?;
        registry.register(Box::new(read_retry_exhausted_total.clone()))?;

        Ok(Self {
            read_retry_total,
            read_retry_exhausted_total,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_succeeds_in_fresh_registry() {
        let reg = Registry::new();
        let m = GatewayRetryMetrics::register(&reg).expect("register ok");
        m.read_retry_total.inc();
        m.read_retry_exhausted_total.inc_by(2);
        assert_eq!(m.read_retry_total.get(), 1);
        assert_eq!(m.read_retry_exhausted_total.get(), 2);
    }

    #[test]
    fn register_twice_in_same_registry_fails() {
        let reg = Registry::new();
        let _m1 = GatewayRetryMetrics::register(&reg).expect("first");
        let m2 = GatewayRetryMetrics::register(&reg);
        assert!(m2.is_err(), "name collision on second register");
    }
}
