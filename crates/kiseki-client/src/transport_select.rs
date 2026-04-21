//! Transport selection and fallback (ADR-008).
//!
//! The client selects the best available transport for each RPC:
//! 1. RDMA (lowest latency, highest bandwidth)
//! 2. TCP direct (when RDMA is unavailable)
//! 3. gRPC (reliable fallback, always available)
//!
//! Selection is per-endpoint and can change as network conditions evolve.

use std::time::{Duration, Instant};

/// Available transport protocols.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    /// RDMA (InfiniBand or RoCE).
    Rdma,
    /// TCP direct (custom protocol).
    TcpDirect,
    /// gRPC over HTTP/2 (always available).
    Grpc,
}

/// Health status for a transport.
#[derive(Clone, Debug)]
struct TransportHealth {
    transport: Transport,
    available: bool,
    latency_us: u64,
    last_check: Instant,
}

/// Transport selector — picks the best available transport for an endpoint.
pub struct TransportSelector {
    candidates: Vec<TransportHealth>,
    fallback_timeout: Duration,
}

impl TransportSelector {
    /// Create a selector with the default transport priority order.
    #[must_use]
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            candidates: vec![
                TransportHealth {
                    transport: Transport::Rdma,
                    available: false, // must be probed
                    latency_us: 0,
                    last_check: now,
                },
                TransportHealth {
                    transport: Transport::TcpDirect,
                    available: true,
                    latency_us: 100,
                    last_check: now,
                },
                TransportHealth {
                    transport: Transport::Grpc,
                    available: true,
                    latency_us: 500,
                    last_check: now,
                },
            ],
            fallback_timeout: Duration::from_secs(5),
        }
    }

    /// Select the best available transport.
    #[must_use]
    pub fn select(&self) -> Transport {
        self.candidates
            .iter()
            .filter(|h| h.available)
            .min_by_key(|h| h.latency_us)
            .map_or(Transport::Grpc, |h| h.transport)
    }

    /// Update transport availability and latency after a probe.
    pub fn update(&mut self, transport: Transport, available: bool, latency_us: u64) {
        if let Some(h) = self
            .candidates
            .iter_mut()
            .find(|h| h.transport == transport)
        {
            h.available = available;
            h.latency_us = latency_us;
            h.last_check = Instant::now();
        }
    }

    /// Mark a transport as unavailable (e.g., after connection failure).
    pub fn mark_unavailable(&mut self, transport: Transport) {
        if let Some(h) = self
            .candidates
            .iter_mut()
            .find(|h| h.transport == transport)
        {
            h.available = false;
            h.last_check = Instant::now();
        }
    }

    /// Check if any candidate needs re-probing (health check expired).
    #[must_use]
    pub fn needs_reprobe(&self) -> Vec<Transport> {
        self.candidates
            .iter()
            .filter(|h| h.last_check.elapsed() > self.fallback_timeout)
            .map(|h| h.transport)
            .collect()
    }
}

impl Default for TransportSelector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_lowest_latency() {
        let mut sel = TransportSelector::new();
        sel.update(Transport::Rdma, true, 5);
        assert_eq!(sel.select(), Transport::Rdma);
    }

    #[test]
    fn falls_back_when_rdma_unavailable() {
        let sel = TransportSelector::new();
        // RDMA not available by default.
        assert_eq!(sel.select(), Transport::TcpDirect);
    }

    #[test]
    fn mark_unavailable() {
        let mut sel = TransportSelector::new();
        sel.mark_unavailable(Transport::TcpDirect);
        assert_eq!(sel.select(), Transport::Grpc);
    }

    #[test]
    fn always_has_grpc_fallback() {
        let mut sel = TransportSelector::new();
        sel.mark_unavailable(Transport::Rdma);
        sel.mark_unavailable(Transport::TcpDirect);
        assert_eq!(sel.select(), Transport::Grpc);
    }
}
