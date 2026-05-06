//! `TcpFramedPrincipal` — `RequestPrincipal` adapter for the
//! TCP-framed-postcard binding (ADR-042 §2.2).
//!
//! Per-connection structure: the rustls handshake establishes the
//! peer cert; the connection-acceptance hook canonicalizes the SAN
//! once and stashes it on the per-connection state. Every request
//! frame on the connection gets a `TcpFramedPrincipal` that points
//! at the stashed SAN — no extension-map lookup per request.
//!
//! Contrast with [`crate::native::grpc::principal::TonicPrincipal`]:
//! the gRPC binding goes through tonic's per-request `Request<T>`
//! extensions because that's tonic's plumbing; the TCP-framed
//! binding has explicit per-connection state so the principal is
//! cloned cheaply (Arc<String>) at frame dispatch.

use std::sync::Arc;

use kiseki_proto::native_contract::{BindingId, ConnectionId, RequestPrincipal};

/// `RequestPrincipal` for the TCP-framed binding.
///
/// `Arc<str>`-backed canonical SAN so cloning at per-frame dispatch
/// is allocation-free. Tests construct via [`TcpFramedPrincipal::new`];
/// the listener constructs once per connection from the validated
/// rustls peer cert + minted [`ConnectionId`].
#[derive(Clone, Debug)]
pub struct TcpFramedPrincipal {
    canonical_san: Arc<str>,
    connection_id: ConnectionId,
}

impl TcpFramedPrincipal {
    /// Build a new principal. `canonical_san` MUST be the output of
    /// `super::canonical_san::canonicalize(...)` — the listener does
    /// this once per accepted connection. Empty string signals "no
    /// SAN stashed" (mirrors the gRPC binding's interceptor-not-
    /// installed fallback in `enforce_san_payload_tenant_match`).
    #[must_use]
    pub fn new(canonical_san: impl Into<Arc<str>>, connection_id: ConnectionId) -> Self {
        Self {
            canonical_san: canonical_san.into(),
            connection_id,
        }
    }
}

impl RequestPrincipal for TcpFramedPrincipal {
    fn cert_san_canonical(&self) -> &str {
        &self.canonical_san
    }
    fn binding_id(&self) -> BindingId {
        BindingId::TcpFramed
    }
    fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_id_is_tcp_framed() {
        let p = TcpFramedPrincipal::new("spiffe://kiseki/tenant/x", ConnectionId(7));
        assert_eq!(p.binding_id(), BindingId::TcpFramed);
    }

    #[test]
    fn cert_san_round_trips_through_dyn_dispatch() {
        let want = "spiffe://kiseki/tenant/org-perf";
        let p = TcpFramedPrincipal::new(want, ConnectionId(99));
        let dyn_p: &dyn RequestPrincipal = &p;
        assert_eq!(dyn_p.cert_san_canonical(), want);
        assert_eq!(dyn_p.connection_id(), ConnectionId(99));
        assert_eq!(dyn_p.binding_id(), BindingId::TcpFramed);
    }

    /// Cloning a `TcpFramedPrincipal` should be O(1) — all state is
    /// either Copy or Arc-backed. We can't directly assert constant
    /// time, but we CAN assert no fresh allocation by checking the
    /// `Arc<str>` strong-count rises after a clone.
    #[test]
    fn clone_does_not_reallocate_canonical_san() {
        let p = TcpFramedPrincipal::new("spiffe://kiseki/tenant/x", ConnectionId(1));
        let strong_before = Arc::strong_count(&p.canonical_san);
        let q = p.clone();
        assert_eq!(Arc::strong_count(&p.canonical_san), strong_before + 1);
        // Drop q — strong count returns.
        drop(q);
        assert_eq!(Arc::strong_count(&p.canonical_san), strong_before);
    }

    /// Empty SAN ("interceptor-not-installed" fallback) must surface
    /// as empty `cert_san_canonical()` so the existing
    /// `enforce_san_payload_tenant_match` skip logic in
    /// `kiseki-gateway::native::server` works for both bindings.
    #[test]
    fn empty_canonical_san_yields_empty_string() {
        let p = TcpFramedPrincipal::new("", ConnectionId(0));
        assert!(p.cert_san_canonical().is_empty());
    }

    /// Different connection ids on the same SAN — the principal can
    /// distinguish concurrent requests on different connections from
    /// the same tenant. Important for audit correlation.
    #[test]
    fn distinct_connection_ids_distinguish_principals() {
        let san: Arc<str> = Arc::from("spiffe://kiseki/tenant/multi");
        let a = TcpFramedPrincipal::new(Arc::clone(&san), ConnectionId(1));
        let b = TcpFramedPrincipal::new(Arc::clone(&san), ConnectionId(2));
        assert_eq!(a.cert_san_canonical(), b.cert_san_canonical());
        assert_ne!(a.connection_id(), b.connection_id());
    }
}
