//! `TonicPrincipal` — `RequestPrincipal` adapter for the gRPC binding.
//!
//! ADR-042 §1.8: handler-side code (`ServerImpl`) reads the per-request
//! principal context ONLY through `kiseki_proto::native_contract::
//! RequestPrincipal`. Each binding ships its own adapter that packages
//! the binding-specific stash location into a `RequestPrincipal` impl.
//!
//! For the gRPC binding the stash is `tonic::Request::extensions()`,
//! populated by [`crate::native::san_interceptor::SanInterceptor`] at
//! per-request entry. We pull the canonical SAN out of that map and
//! mint a per-request `ConnectionId` from the per-process monotonic
//! counter (the connection-level identity is preserved by tonic at the
//! transport layer; what handlers actually need is "uniquely tag this
//! request for audit/metric correlation" which a monotonic counter
//! satisfies cheaply).

use std::sync::atomic::{AtomicU64, Ordering};

use kiseki_proto::native_contract::{BindingId, ConnectionId, RequestPrincipal};

use crate::native::canonical_san::CanonicalSanUri;

/// `RequestPrincipal` for the gRPC binding. Cheap to construct
/// (`String` clone of the canonical SAN + a monotonic counter).
///
/// Construct via [`principal_from_request`] which extracts the
/// canonical SAN from request extensions; the constructor exists for
/// tests that mint synthetic principals without a tonic stack.
#[derive(Clone, Debug)]
pub struct TonicPrincipal {
    canonical_san: String,
    connection_id: ConnectionId,
}

impl TonicPrincipal {
    /// Synthesize from canonical SAN + connection id. Tests use this
    /// directly; production code goes through
    /// [`principal_from_request`] so the same extraction logic is
    /// exercised.
    #[must_use]
    pub fn new(canonical_san: String, connection_id: ConnectionId) -> Self {
        Self {
            canonical_san,
            connection_id,
        }
    }
}

impl RequestPrincipal for TonicPrincipal {
    fn cert_san_canonical(&self) -> &str {
        &self.canonical_san
    }
    fn binding_id(&self) -> BindingId {
        BindingId::Grpc
    }
    fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }
}

/// Per-process counter minting `ConnectionId` for tonic requests. The
/// gRPC binding doesn't currently surface a stable per-connection id
/// to handlers (tonic's `TcpConnectInfo::peer_addr` is per-connection
/// but rebuilt per request); this counter is the cheapest correlation
/// id while still being unique within a process lifetime.
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// Build a `TonicPrincipal` from a tonic request. Reads the canonical
/// SAN that [`SanInterceptor`](crate::native::san_interceptor::SanInterceptor)
/// stashed in request extensions; falls back to an empty string when
/// the interceptor wasn't installed (unit tests on the bare
/// `ServerImpl` — the same fallback path that
/// `enforce_san_payload_tenant_match` already accommodates).
///
/// [`tonic::Request<T>`] is a binding-specific type; this function is
/// the only ADR-permitted place that reaches into it for principal
/// extraction (per §1.8 enforcement: only adapter code may; handler
/// code must go through `&dyn RequestPrincipal`).
pub fn principal_from_request<T>(req: &tonic::Request<T>) -> TonicPrincipal {
    let canonical_san = req
        .extensions()
        .get::<CanonicalSanUri>()
        .map(|c| c.as_str().to_string())
        .unwrap_or_default();
    TonicPrincipal {
        canonical_san,
        connection_id: ConnectionId(NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Request;

    #[test]
    fn binding_id_is_grpc() {
        let p = TonicPrincipal::new("spiffe://kiseki/tenant/org-test".into(), ConnectionId(1));
        assert_eq!(p.binding_id(), BindingId::Grpc);
    }

    #[test]
    fn cert_san_canonical_round_trips_through_dyn_dispatch() {
        let want = "spiffe://kiseki/tenant/org-perf";
        let p = TonicPrincipal::new(want.into(), ConnectionId(42));
        let dyn_p: &dyn RequestPrincipal = &p;
        assert_eq!(dyn_p.cert_san_canonical(), want);
        assert_eq!(dyn_p.connection_id(), ConnectionId(42));
        assert_eq!(dyn_p.binding_id(), BindingId::Grpc);
    }

    #[test]
    fn principal_from_request_with_extension_carries_canonical_san() {
        let want = "spiffe://kiseki/tenant/org-pharma";
        let mut req: Request<()> = Request::new(());
        let canonical = CanonicalSanUri::from_canonical_for_tests(want);
        req.extensions_mut().insert(canonical);
        let p = principal_from_request(&req);
        assert_eq!(p.cert_san_canonical(), want);
    }

    #[test]
    fn principal_from_request_without_extension_yields_empty_san() {
        // Mirrors enforce_san_payload_tenant_match's existing fallback:
        // when the interceptor wasn't installed (unit-test bare
        // ServerImpl path), the canonical SAN is absent and the cross-
        // check elsewhere skips. Empty-string return surfaces this
        // unambiguously to the cross-check.
        let req: Request<()> = Request::new(());
        let p = principal_from_request(&req);
        assert!(p.cert_san_canonical().is_empty());
    }

    #[test]
    fn principal_from_request_mints_unique_connection_ids() {
        let req1: Request<()> = Request::new(());
        let req2: Request<()> = Request::new(());
        let p1 = principal_from_request(&req1);
        let p2 = principal_from_request(&req2);
        assert_ne!(
            p1.connection_id(),
            p2.connection_id(),
            "monotonic counter must yield distinct ids per call",
        );
    }

    /// Demonstrates that handler-shaped code can take `&dyn
    /// RequestPrincipal` and the gRPC binding's adapter satisfies it
    /// without leaking tonic types into the function signature.
    fn read_san_via_trait(p: &dyn RequestPrincipal) -> String {
        p.cert_san_canonical().to_string()
    }

    #[test]
    fn handler_code_only_sees_dyn_request_principal() {
        let mut req: Request<()> = Request::new(());
        let canonical =
            CanonicalSanUri::from_canonical_for_tests("spiffe://kiseki/tenant/org-handler");
        req.extensions_mut().insert(canonical);
        let principal = principal_from_request(&req);
        // handler signature is &dyn RequestPrincipal — no tonic types.
        let san = read_san_via_trait(&principal);
        assert_eq!(san, "spiffe://kiseki/tenant/org-handler");
    }
}
