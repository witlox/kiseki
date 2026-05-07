//! TCP-framed-postcard binding probe (ADR-042 §2.2, §3.1 phase 1).
//!
//! TCP+rustls is universally available; the probe validates the
//! operator-configured listen address parses and honors the
//! `disabled` operator-disable sentinel. Latency class is `Low` —
//! the binding's value vs gRPC is precisely the avoided h2 framing
//! tax. RDMA bindings (`Rdma`) outrank in the selector.
//!
//! Listen-address resolution mirrors the gRPC probe:
//! 1. `KISEKI_NATIVE_TCP_ADDR` env var.
//! 2. The address passed to [`TcpFramedProbe::with_addr`].
//! 3. Default `0.0.0.0:9101` (per §2.2).
//!
//! `KISEKI_NATIVE_TCP_ADDR=disabled` is the operator escape hatch:
//! the probe reports `Unavailable { reason: "disabled by operator" }`
//! and the binding is omitted from the spawn plan even when other
//! bindings are present.

use kiseki_proto::native_contract::{BindingId, LatencyClass, ListenAddr};
use kiseki_transport::native::{BindingProbe, ProbeOutcome};

/// TCP-framed-postcard binding probe.
pub struct TcpFramedProbe {
    addr: String,
}

impl TcpFramedProbe {
    /// Build a probe with the runtime-configured listen address.
    /// Env-var override (`KISEKI_NATIVE_TCP_ADDR`) wins over the
    /// default; the literal value `disabled` self-disqualifies.
    #[must_use]
    pub fn new(default_addr: impl Into<String>) -> Self {
        let env = std::env::var("KISEKI_NATIVE_TCP_ADDR").ok();
        Self {
            addr: env.unwrap_or_else(|| default_addr.into()),
        }
    }

    /// Override the listen address. Tests use this; production reads
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
impl BindingProbe for TcpFramedProbe {
    fn binding_id(&self) -> BindingId {
        BindingId::TcpFramed
    }

    async fn probe(&self) -> ProbeOutcome {
        if self.addr.eq_ignore_ascii_case("disabled") {
            return ProbeOutcome::Unavailable {
                reason: "disabled by operator (KISEKI_NATIVE_TCP_ADDR=disabled)".into(),
            };
        }
        match self.addr.parse::<std::net::SocketAddr>() {
            Ok(_) => ProbeOutcome::Available {
                latency_class: LatencyClass::Low,
                addr: ListenAddr::HostPort(self.addr.clone()),
            },
            Err(e) => ProbeOutcome::Unavailable {
                reason: format!("KISEKI_NATIVE_TCP_ADDR parse failed: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_returns_available_with_low_latency_class() {
        let probe = TcpFramedProbe::new("0.0.0.0:9101");
        match probe.probe().await {
            ProbeOutcome::Available {
                latency_class,
                addr,
            } => {
                assert_eq!(latency_class, LatencyClass::Low);
                assert_eq!(addr, ListenAddr::HostPort("0.0.0.0:9101".into()));
            }
            other => panic!("expected Available, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_honors_disabled_sentinel() {
        let probe = TcpFramedProbe::new("0.0.0.0:9101").with_addr("disabled");
        match probe.probe().await {
            ProbeOutcome::Unavailable { reason } => {
                assert!(
                    reason.contains("disabled by operator"),
                    "reason: {reason}",
                );
            }
            other => panic!("expected Unavailable, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_rejects_malformed_addr() {
        let probe = TcpFramedProbe::new("0.0.0.0:9101").with_addr("nope");
        match probe.probe().await {
            ProbeOutcome::Unavailable { reason } => {
                assert!(reason.contains("parse failed"), "reason: {reason}");
            }
            other => panic!("expected Unavailable, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn binding_id_is_tcp_framed() {
        let probe = TcpFramedProbe::new("0.0.0.0:9101");
        assert_eq!(probe.binding_id(), BindingId::TcpFramed);
    }

    /// Latency-class outranks Standard so `auto`-mode selector
    /// picks TCP-framed over gRPC when both are present.
    #[tokio::test(flavor = "multi_thread")]
    async fn latency_class_outranks_standard() {
        assert!(LatencyClass::Low > LatencyClass::Standard);
    }
}
