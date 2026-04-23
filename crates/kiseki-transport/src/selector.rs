//! Fabric-aware transport selection with failover.
//!
//! `FabricSelector` probes for available transports at boot (CXI, IB,
//! `RoCEv2`, TCP+TLS) and selects the best one for each connection.
//! On failure, it falls back to the next-best transport.

use std::net::SocketAddr;
use std::time::Duration;

use crate::health::{HealthConfig, TransportHealthTracker};
use crate::traits::PeerIdentity;
use kiseki_common::ids::OrgId;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Available fabric transport types, ordered by preference (fastest first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FabricTransport {
    /// HPE Slingshot CXI (lowest latency on Slingshot fabric).
    Cxi,
    /// RDMA verbs — native `InfiniBand`.
    VerbsIb,
    /// RDMA verbs — `RoCEv2` (RDMA over Converged Ethernet).
    VerbsRoce,
    /// TCP + TLS (always available, universal fallback).
    TcpTls,
}

impl FabricTransport {
    /// Human-readable name for metrics and logging.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Cxi => "cxi",
            Self::VerbsIb => "verbs-ib",
            Self::VerbsRoce => "verbs-roce",
            Self::TcpTls => "tcp-tls",
        }
    }

    /// Priority rank (lower = preferred).
    #[must_use]
    pub fn priority(self) -> u8 {
        match self {
            Self::Cxi => 0,
            Self::VerbsIb => 1,
            Self::VerbsRoce => 2,
            Self::TcpTls => 3,
        }
    }
}

/// A transport candidate registered with the selector.
struct Candidate {
    transport: FabricTransport,
    available: bool,
}

/// Fabric-aware transport selector.
///
/// Probes for available HPC transports at boot and provides failover.
/// Shared between client, server, and Raft transport layers.
pub struct FabricSelector {
    candidates: Vec<Candidate>,
    health: TransportHealthTracker,
}

impl FabricSelector {
    /// Create a selector with the given candidates.
    ///
    /// Candidates should be pre-probed — pass `available: true` only
    /// for transports whose hardware is actually present.
    #[must_use]
    pub fn new(health_config: HealthConfig) -> Self {
        Self {
            candidates: Vec::new(),
            health: TransportHealthTracker::new(health_config),
        }
    }

    /// Register a transport as available.
    pub fn register(&mut self, transport: FabricTransport) {
        // Avoid duplicates.
        if !self.candidates.iter().any(|c| c.transport == transport) {
            self.candidates.push(Candidate {
                transport,
                available: true,
            });
            // Sort by priority (fastest first).
            self.candidates.sort_by_key(|c| c.transport.priority());
        }
    }

    /// Select the best available transport.
    ///
    /// Returns the highest-priority transport that is both registered
    /// as available and not tripped by the circuit breaker.
    /// Falls back to `TcpTls` if nothing else is available.
    #[must_use]
    pub fn select(&self) -> FabricTransport {
        for candidate in &self.candidates {
            if candidate.available && self.health.is_healthy(candidate.transport.name()) {
                return candidate.transport;
            }
        }
        // Ultimate fallback.
        FabricTransport::TcpTls
    }

    /// Record a successful operation on a transport.
    pub fn record_success(&mut self, transport: FabricTransport, latency: Duration) {
        self.health.record_success(transport.name(), latency);
    }

    /// Record a failure on a transport.
    pub fn record_failure(&mut self, transport: FabricTransport) {
        self.health.record_failure(transport.name());
    }

    /// Mark a transport as unavailable (e.g., hardware removed).
    pub fn mark_unavailable(&mut self, transport: FabricTransport) {
        if let Some(c) = self
            .candidates
            .iter_mut()
            .find(|c| c.transport == transport)
        {
            c.available = false;
        }
    }

    /// Mark a transport as available (e.g., after reprobe).
    pub fn mark_available(&mut self, transport: FabricTransport) {
        if let Some(c) = self
            .candidates
            .iter_mut()
            .find(|c| c.transport == transport)
        {
            c.available = true;
        }
    }

    /// Check which transports need re-probing.
    #[must_use]
    pub fn needs_reprobe(&self) -> Vec<FabricTransport> {
        self.candidates
            .iter()
            .filter(|c| self.health.should_reprobe(c.transport.name()))
            .map(|c| c.transport)
            .collect()
    }

    /// All registered transports and their current status.
    #[must_use]
    pub fn status(&self) -> Vec<(FabricTransport, bool, bool)> {
        self.candidates
            .iter()
            .map(|c| {
                let healthy = self.health.is_healthy(c.transport.name());
                (c.transport, c.available, healthy)
            })
            .collect()
    }

    /// Reference to the health tracker for direct queries.
    #[must_use]
    pub fn health(&self) -> &TransportHealthTracker {
        &self.health
    }
}

/// Probe the system for available fabric transports.
///
/// Checks sysfs for CXI and RDMA devices. TCP+TLS is always registered.
#[must_use]
pub fn probe_available_transports() -> Vec<FabricTransport> {
    let mut available = Vec::new();

    // Check for CXI devices.
    let cxi_dir = std::path::Path::new("/sys/class/cxi");
    if cxi_dir.exists() && std::fs::read_dir(cxi_dir).is_ok_and(|mut d| d.next().is_some()) {
        available.push(FabricTransport::Cxi);
    }

    // Check for InfiniBand / RoCE devices.
    let ib_dir = std::path::Path::new("/sys/class/infiniband");
    if ib_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(ib_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let link_layer_path = ib_dir
                    .join(&name)
                    .join("ports")
                    .join("1")
                    .join("link_layer");
                if let Ok(ll) = std::fs::read_to_string(link_layer_path) {
                    if ll.trim() == "Ethernet" {
                        if !available.contains(&FabricTransport::VerbsRoce) {
                            available.push(FabricTransport::VerbsRoce);
                        }
                    } else if !available.contains(&FabricTransport::VerbsIb) {
                        available.push(FabricTransport::VerbsIb);
                    }
                }
            }
        }
    }

    // TCP+TLS is always available.
    available.push(FabricTransport::TcpTls);

    available
}

/// A type-erased connection from any transport.
///
/// Wraps the concrete connection type behind `dyn AsyncRead + AsyncWrite`
/// with a `PeerIdentity` for tenant scoping.
pub struct DynConnection {
    /// The underlying I/O stream.
    inner: Box<dyn DynIo>,
    /// Peer identity from the transport handshake.
    identity: PeerIdentity,
    /// Remote address.
    remote: SocketAddr,
    /// Which transport produced this connection.
    transport: FabricTransport,
}

/// Combined async read + write + send + unpin for type erasure.
trait DynIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> DynIo for T {}

impl DynConnection {
    /// Create a new dynamic connection wrapping any `AsyncRead + AsyncWrite`.
    pub fn new(
        inner: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
        identity: PeerIdentity,
        remote: SocketAddr,
        transport: FabricTransport,
    ) -> Self {
        Self {
            inner: Box::new(inner),
            identity,
            remote,
            transport,
        }
    }

    /// The peer identity from the transport handshake.
    #[must_use]
    pub fn peer_identity(&self) -> &PeerIdentity {
        &self.identity
    }

    /// The remote socket address.
    #[must_use]
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote
    }

    /// Which transport was used for this connection.
    #[must_use]
    pub fn transport(&self) -> FabricTransport {
        self.transport
    }

    /// Build a fallback identity for connections without mTLS.
    #[must_use]
    pub fn anonymous_identity(addr: SocketAddr) -> PeerIdentity {
        PeerIdentity {
            org_id: OrgId(uuid::Uuid::nil()),
            common_name: format!("anon-{addr}"),
            cert_fingerprint: [0u8; 32],
        }
    }
}

impl AsyncRead for DynConnection {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for DynConnection {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut *self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_order() {
        assert!(FabricTransport::Cxi.priority() < FabricTransport::VerbsIb.priority());
        assert!(FabricTransport::VerbsIb.priority() < FabricTransport::VerbsRoce.priority());
        assert!(FabricTransport::VerbsRoce.priority() < FabricTransport::TcpTls.priority());
    }

    #[test]
    fn selects_highest_priority() {
        let mut sel = FabricSelector::new(HealthConfig::default());
        sel.register(FabricTransport::TcpTls);
        sel.register(FabricTransport::VerbsIb);
        assert_eq!(sel.select(), FabricTransport::VerbsIb);
    }

    #[test]
    fn fallback_on_failure() {
        let config = HealthConfig {
            failure_threshold: 1,
            failure_window: Duration::from_secs(30),
            reprobe_interval: Duration::from_secs(10),
        };
        let mut sel = FabricSelector::new(config);
        sel.register(FabricTransport::VerbsIb);
        sel.register(FabricTransport::TcpTls);

        // Trip the circuit breaker on VerbsIb.
        sel.record_failure(FabricTransport::VerbsIb);
        assert_eq!(
            sel.select(),
            FabricTransport::TcpTls,
            "should fall back to TCP after verbs failure"
        );
    }

    #[test]
    fn recovery_after_success() {
        let config = HealthConfig {
            failure_threshold: 1,
            failure_window: Duration::from_secs(30),
            reprobe_interval: Duration::from_secs(10),
        };
        let mut sel = FabricSelector::new(config);
        sel.register(FabricTransport::VerbsRoce);
        sel.register(FabricTransport::TcpTls);

        sel.record_failure(FabricTransport::VerbsRoce);
        assert_eq!(sel.select(), FabricTransport::TcpTls);

        sel.record_success(FabricTransport::VerbsRoce, Duration::from_micros(5));
        assert_eq!(
            sel.select(),
            FabricTransport::VerbsRoce,
            "should recover after successful reprobe"
        );
    }

    #[test]
    fn mark_unavailable_skips_transport() {
        let mut sel = FabricSelector::new(HealthConfig::default());
        sel.register(FabricTransport::Cxi);
        sel.register(FabricTransport::TcpTls);

        sel.mark_unavailable(FabricTransport::Cxi);
        assert_eq!(sel.select(), FabricTransport::TcpTls);

        sel.mark_available(FabricTransport::Cxi);
        assert_eq!(sel.select(), FabricTransport::Cxi);
    }

    #[test]
    fn empty_selector_returns_tcp() {
        let sel = FabricSelector::new(HealthConfig::default());
        assert_eq!(sel.select(), FabricTransport::TcpTls);
    }

    #[test]
    fn probe_always_includes_tcp() {
        let transports = probe_available_transports();
        assert!(
            transports.contains(&FabricTransport::TcpTls),
            "TCP+TLS must always be available"
        );
    }

    #[test]
    fn status_reports_all_registered() {
        let mut sel = FabricSelector::new(HealthConfig::default());
        sel.register(FabricTransport::VerbsIb);
        sel.register(FabricTransport::TcpTls);
        let status = sel.status();
        assert_eq!(status.len(), 2);
    }

    #[test]
    fn no_duplicate_registration() {
        let mut sel = FabricSelector::new(HealthConfig::default());
        sel.register(FabricTransport::TcpTls);
        sel.register(FabricTransport::TcpTls);
        assert_eq!(sel.status().len(), 1);
    }
}
