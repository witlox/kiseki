//! gRPC binding probe (ADR-042 §3.1 phase 1, §16.1 phase 4).
//!
//! gRPC over rustls/TCP is universally available — every Linux host
//! the Rust toolchain supports can `tonic::Server::bind`. The probe
//! validates the operator-configured listen address parses as a
//! socket-addr and returns `Available { latency_class: Standard }`.
//! Actual `bind()` happens at phase-3 listener spawn; bind failure
//! there downgrades this binding to Unavailable for the rest of the
//! session per §3.4.
//!
//! Listen-address resolution (in priority order):
//! 1. `KISEKI_NATIVE_GRPC_ADDR` env var.
//! 2. The address passed to [`GrpcProbe::with_addr`].
//! 3. Default `0.0.0.0:9100`.

use kiseki_proto::native_contract::{BindingId, LatencyClass, ListenAddr};
use kiseki_transport::native::{BindingProbe, ProbeOutcome};

/// gRPC binding probe.
pub struct GrpcProbe {
    addr: String,
}

impl GrpcProbe {
    /// Build a probe with the runtime-configured listen address.
    /// Env-var override (`KISEKI_NATIVE_GRPC_ADDR`) wins over the
    /// argument so operators can flip without a code change.
    #[must_use]
    pub fn new(default_addr: impl Into<String>) -> Self {
        let env = std::env::var("KISEKI_NATIVE_GRPC_ADDR").ok();
        Self {
            addr: env.unwrap_or_else(|| default_addr.into()),
        }
    }

    /// Override the listen address. Used by tests; production reads
    /// the env var via [`new`].
    ///
    /// [`new`]: Self::new
    #[must_use]
    pub fn with_addr(mut self, addr: impl Into<String>) -> Self {
        self.addr = addr.into();
        self
    }

    /// Borrow the resolved listen address.
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.addr
    }
}

#[async_trait::async_trait]
impl BindingProbe for GrpcProbe {
    fn binding_id(&self) -> BindingId {
        BindingId::Grpc
    }

    async fn probe(&self) -> ProbeOutcome {
        // Validate the addr parses. `0.0.0.0:9100` and equivalent
        // bind-all forms parse fine; bare `:9100` does not (matches
        // tokio's resolver behavior — keeps the failure mode tight).
        match self.addr.parse::<std::net::SocketAddr>() {
            Ok(_) => ProbeOutcome::Available {
                latency_class: LatencyClass::Standard,
                addr: ListenAddr::HostPort(self.addr.clone()),
            },
            Err(e) => ProbeOutcome::Unavailable {
                reason: format!("KISEKI_NATIVE_GRPC_ADDR parse failed: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_returns_available_with_standard_latency_class() {
        let probe = GrpcProbe::new("0.0.0.0:9100");
        match probe.probe().await {
            ProbeOutcome::Available {
                latency_class,
                addr,
            } => {
                assert_eq!(latency_class, LatencyClass::Standard);
                assert_eq!(addr, ListenAddr::HostPort("0.0.0.0:9100".into()));
            }
            ProbeOutcome::Unavailable { reason } => {
                panic!("expected Available, got Unavailable: {reason}");
            }
        }
    }

    #[tokio::test]
    async fn probe_rejects_malformed_addr() {
        let probe = GrpcProbe::new("not-an-addr");
        match probe.probe().await {
            ProbeOutcome::Unavailable { reason } => {
                assert!(
                    reason.contains("KISEKI_NATIVE_GRPC_ADDR parse failed"),
                    "reason: {reason}",
                );
            }
            ProbeOutcome::Available {
                latency_class,
                addr,
            } => {
                panic!(
                    "expected Unavailable, got Available: latency={latency_class:?} addr={addr:?}"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_addr_override_supersedes_default() {
        // Production flow: `GrpcProbe::new(default)` reads
        // `KISEKI_NATIVE_GRPC_ADDR`; tests bypass the env layer via
        // `with_addr` to keep the unit test deterministic across
        // `cargo test` parallelism. Env-var precedence is exercised
        // end-to-end via the kiseki-server runtime smoke tests.
        let probe = GrpcProbe::new("0.0.0.0:9100").with_addr("127.0.0.1:9999");
        assert_eq!(probe.addr(), "127.0.0.1:9999");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn binding_id_is_grpc() {
        let probe = GrpcProbe::new("0.0.0.0:9100");
        assert_eq!(probe.binding_id(), BindingId::Grpc);
    }
}
