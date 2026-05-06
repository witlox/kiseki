//! Verb dispatch for the TCP-framed binding (ADR-042 §2.2).
//!
//! Per-frame work: postcard-decode the request body against the type
//! the verb identifier names, call the corresponding inherent method
//! on [`ServerImpl`], postcard-encode the response (or
//! tonic-Status-mapped error reason), return `(WireStatus, body)`
//! for the caller to wrap in a response frame.
//!
//! The verb-tag → ServerImpl mapping is one big match; each arm is
//! ~one line via the [`unary_verb!`] macro. Adding a new verb is:
//! one match arm + one signature confirmation. No reflection, no
//! registry — explicit dispatch keeps the wire surface review-able.

use kiseki_proto::native_contract::RequestPrincipal;
use kiseki_proto::native_contract::wire_tcp_framed::WireStatus;
use kiseki_proto::v1::native as np;
use tonic::Status;

use crate::native::server::ServerImpl;

/// Map a `tonic::Status` (returned by `ServerImpl` inherent methods)
/// to the wire's [`WireStatus`] byte. The reason string travels in
/// the body, encoded by the dispatch caller.
///
/// `tonic::Code` is exhaustive; unknown future variants fall into
/// `Internal`. The mapping below mirrors ADR-042 §1.4 exactly for
/// the variants both share.
#[must_use]
pub fn status_to_wire(status: &Status) -> WireStatus {
    use tonic::Code;
    match status.code() {
        Code::Ok => WireStatus::Ok,
        Code::Cancelled | Code::Aborted => WireStatus::Aborted,
        Code::InvalidArgument => WireStatus::InvalidArgument,
        Code::NotFound => WireStatus::NotFound,
        Code::AlreadyExists => WireStatus::AlreadyExists,
        Code::PermissionDenied => WireStatus::PermissionDenied,
        Code::ResourceExhausted => WireStatus::ResourceExhausted,
        Code::FailedPrecondition => WireStatus::PreconditionFailed,
        Code::OutOfRange => WireStatus::OutOfRange,
        Code::Unavailable | Code::DeadlineExceeded => WireStatus::Unavailable,
        Code::Unauthenticated => WireStatus::Unauthenticated,
        // Internal / Unknown / Unimplemented / DataLoss collapse to
        // Internal (§1.4 has no Unimplemented variant — POSIX stubs
        // shouldn't reach the TCP-framed dispatch path because they
        // live in the gRPC adapter only).
        Code::Internal | Code::Unknown | Code::Unimplemented | Code::DataLoss => {
            WireStatus::Internal
        }
    }
}

/// Macro: generate one match arm of [`dispatch_verb`] for a unary
/// verb. Decode the request body, call the handler future, encode
/// the response. On any error path the body carries the
/// human-readable reason the handler emitted (encoded as raw UTF-8
/// bytes; the client decodes via `String::from_utf8_lossy`).
macro_rules! unary_verb {
    ($verb:literal, $payload:expr, $handler:expr) => {{
        let req = match postcard::from_bytes($payload) {
            Ok(r) => r,
            Err(e) => {
                return (
                    WireStatus::ProtocolError,
                    format!("postcard decode failed in {}: {e}", $verb).into_bytes(),
                );
            }
        };
        match $handler(req).await {
            Ok(resp) => match postcard::to_allocvec(&resp) {
                Ok(bytes) => (WireStatus::Ok, bytes),
                Err(e) => (
                    WireStatus::Internal,
                    format!("postcard encode failed in {}: {e}", $verb).into_bytes(),
                ),
            },
            Err(status) => (
                status_to_wire(&status),
                status.message().as_bytes().to_vec(),
            ),
        }
    }};
}

/// Dispatch one TCP-framed RPC. Decodes the verb's request body,
/// calls the corresponding [`ServerImpl`] inherent method, encodes
/// the response (or maps the status). Returns `(WireStatus, body)`
/// for the caller to wrap in a [`encode_response_frame`] frame.
///
/// Streaming gRPC verbs (`put_object_stream`, `get_object_stream`,
/// `put_part`, `read_stream`, `write_stream`) are NOT exposed on
/// TCP-framed — the binding's per-call body cap of 80 MiB
/// (>64 MiB per-stream cap, §1.5) covers the unary surface; clients
/// use multipart for above-cap. Spec §2.2 calls this out: "requests
/// multiplex via `request_id` like h2 streams" — the multiplex IS
/// the streaming, per-call body is buffered.
///
/// POSIX stubs (`open`, `read`, `write`, `close`, ...) are not
/// bridged in this phase; the dispatch returns `UnknownVerb` for
/// them so a v1 client + v0 server pairing surfaces a clear error.
///
/// `fsync` is special-cased: no principal, no request body.
///
/// [`encode_response_frame`]: kiseki_proto::native_contract::wire_tcp_framed::encode_response_frame
pub async fn dispatch_verb(
    server: &ServerImpl,
    principal: &dyn RequestPrincipal,
    verb_tag: &str,
    payload_bytes: &[u8],
) -> (WireStatus, Vec<u8>) {
    match verb_tag {
        // Object verbs.
        "put_object" => unary_verb!("put_object", payload_bytes, |req: np::PutObjectRequest| {
            server.put_object(principal, req)
        }),
        "get_object" => unary_verb!("get_object", payload_bytes, |req: np::GetObjectRequest| {
            server.get_object(principal, req)
        }),
        "delete_object" => unary_verb!(
            "delete_object",
            payload_bytes,
            |req: np::DeleteObjectRequest| server.delete_object(principal, req)
        ),
        "head_object" => unary_verb!("head_object", payload_bytes, |req: np::HeadObjectRequest| {
            server.head_object(principal, req)
        }),
        "list_objects" => unary_verb!(
            "list_objects",
            payload_bytes,
            |req: np::ListObjectsRequest| server.list_objects(principal, req)
        ),
        "lookup_by_name" => unary_verb!(
            "lookup_by_name",
            payload_bytes,
            |req: np::LookupByNameRequest| server.lookup_by_name(principal, req)
        ),
        // Multipart verbs (init / complete / abort; put_part requires
        // streaming on TCP-framed and is deferred to a follow-up).
        "init_multipart" => unary_verb!(
            "init_multipart",
            payload_bytes,
            |req: np::InitMultipartRequest| server.init_multipart(principal, req)
        ),
        "complete_multipart" => unary_verb!(
            "complete_multipart",
            payload_bytes,
            |req: np::CompleteMultipartRequest| server.complete_multipart(principal, req)
        ),
        "abort_multipart" => unary_verb!(
            "abort_multipart",
            payload_bytes,
            |req: np::AbortMultipartRequest| server.abort_multipart(principal, req)
        ),
        // Lease verbs.
        "acquire_lease" => unary_verb!(
            "acquire_lease",
            payload_bytes,
            |req: np::AcquireLeaseRequest| server.acquire_lease(principal, req)
        ),
        "renew_lease" => unary_verb!("renew_lease", payload_bytes, |req: np::RenewLeaseRequest| {
            server.renew_lease(principal, req)
        }),
        "release_lease" => unary_verb!(
            "release_lease",
            payload_bytes,
            |req: np::ReleaseLeaseRequest| server.release_lease(principal, req)
        ),
        // DEK fetch.
        "fetch_dek" => unary_verb!("fetch_dek", payload_bytes, |req: np::FetchDekRequest| {
            server.fetch_dek(principal, req)
        }),
        "batch_fetch_dek" => unary_verb!(
            "batch_fetch_dek",
            payload_bytes,
            |req: np::BatchFetchDekRequest| server.batch_fetch_dek(principal, req)
        ),
        // Topology.
        "get_topology" => unary_verb!(
            "get_topology",
            payload_bytes,
            |req: np::GetTopologyRequest| server.get_topology(principal, req)
        ),
        // Cluster-wide flush — no principal, no body.
        "fsync" => match server.fsync().await {
            Ok(resp) => match postcard::to_allocvec(&resp) {
                Ok(bytes) => (WireStatus::Ok, bytes),
                Err(e) => (
                    WireStatus::Internal,
                    format!("postcard encode failed in fsync: {e}").into_bytes(),
                ),
            },
            Err(status) => (
                status_to_wire(&status),
                status.message().as_bytes().to_vec(),
            ),
        },
        _ => (WireStatus::UnknownVerb, verb_tag.as_bytes().to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_gateway::InMemoryGateway;
    use crate::native::signing_keys::SigningKeys;
    use crate::native::tcp_framed::TcpFramedPrincipal;
    use kiseki_chunk::ChunkStore;
    use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_composition::namespace::Namespace;
    use kiseki_crypto::keys::SystemMasterKey;
    use kiseki_proto::native_contract::ConnectionId;
    use std::sync::Arc;

    async fn make_server() -> Arc<ServerImpl> {
        let gw = Arc::new(InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
        ));
        gw.add_namespace(Namespace {
            id: NamespaceId(uuid::Uuid::from_bytes([2; 16])),
            tenant_id: OrgId(uuid::Uuid::from_bytes([1; 16])),
            shard_id: ShardId(uuid::Uuid::nil()),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        })
        .await;
        let signing = Arc::new(SigningKeys::new(
            &SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
            60_000,
        ));
        Arc::new(ServerImpl::new(
            gw as Arc<dyn crate::ops::GatewayOps>,
            signing,
        ))
    }

    fn anon_principal() -> TcpFramedPrincipal {
        TcpFramedPrincipal::new("", ConnectionId(0))
    }

    fn ctrl(tenant: OrgId) -> np::ControlFields {
        np::ControlFields {
            tenant_id: Some(kiseki_proto::v1::OrgId {
                value: tenant.0.to_string(),
            }),
            idempotency_key: vec![0xAB; 8],
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
        }
    }

    fn ns_proto(ns: NamespaceId) -> kiseki_proto::v1::NamespaceId {
        kiseki_proto::v1::NamespaceId {
            value: ns.0.to_string(),
        }
    }

    /// End-to-end through the dispatch boundary: postcard-encode a
    /// PutObjectRequest, run dispatch, postcard-decode the response.
    /// Proves the dispatch path closes the loop on a real verb.
    #[tokio::test]
    async fn put_object_dispatches_and_returns_postcard_response() {
        let server = make_server().await;
        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
        let req = np::PutObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(ns_proto(ns)),
            name: "alpha".into(),
            data: b"hello".to_vec(),
        };
        let payload = postcard::to_allocvec(&req).expect("encode req");
        let principal = anon_principal();
        let (status, body) =
            dispatch_verb(&server, &principal, "put_object", &payload).await;
        assert_eq!(status, WireStatus::Ok, "body: {:?}", String::from_utf8_lossy(&body));
        let resp: np::PutObjectResponse = postcard::from_bytes(&body).expect("decode resp");
        assert_eq!(resp.size, 5);
        assert!(resp.composition_id.is_some());
    }

    /// Round-trip a PUT then a GET via the dispatch table — proves
    /// the same handler logic the gRPC adapter sees is reached.
    #[tokio::test]
    async fn put_then_get_round_trip_via_dispatch() {
        let server = make_server().await;
        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));

        let put_payload = postcard::to_allocvec(&np::PutObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(ns_proto(ns)),
            name: "key".into(),
            data: b"value".to_vec(),
        })
        .unwrap();
        let principal = anon_principal();
        let (s1, b1) = dispatch_verb(&server, &principal, "put_object", &put_payload).await;
        assert_eq!(s1, WireStatus::Ok);
        let put_resp: np::PutObjectResponse = postcard::from_bytes(&b1).unwrap();
        let comp = put_resp.composition_id.expect("comp id");

        let get_payload = postcard::to_allocvec(&np::GetObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(ns_proto(ns)),
            range_start: 0,
            range_end: 0,
            key: Some(np::get_object_request::Key::CompositionId(comp)),
        })
        .unwrap();
        let (s2, b2) = dispatch_verb(&server, &principal, "get_object", &get_payload).await;
        assert_eq!(s2, WireStatus::Ok);
        let get_resp: np::GetObjectResponse = postcard::from_bytes(&b2).unwrap();
        assert_eq!(get_resp.data, b"value");
    }

    /// Unknown verb → WireStatus::UnknownVerb. Body carries the
    /// verb name so a server-version mismatch is debuggable.
    #[tokio::test]
    async fn unknown_verb_returns_unknownverb_with_tag_in_body() {
        let server = make_server().await;
        let principal = anon_principal();
        let (status, body) = dispatch_verb(&server, &principal, "not_a_real_verb", b"").await;
        assert_eq!(status, WireStatus::UnknownVerb);
        assert_eq!(body, b"not_a_real_verb");
    }

    /// Streaming verbs were intentionally not added to the dispatch
    /// table per §2.2 (TCP-framed buffers). Confirm `put_object_stream`
    /// surfaces as UnknownVerb so a client mistakenly sending the
    /// gRPC stream verb name fails closed rather than hanging.
    #[tokio::test]
    async fn streaming_verb_names_surface_as_unknown_verb() {
        let server = make_server().await;
        let principal = anon_principal();
        for verb in [
            "put_object_stream",
            "get_object_stream",
            "put_part",
            "read_stream",
            "write_stream",
        ] {
            let (status, _body) = dispatch_verb(&server, &principal, verb, b"").await;
            assert_eq!(
                status,
                WireStatus::UnknownVerb,
                "streaming verb {verb} should be UnknownVerb on TCP-framed",
            );
        }
    }

    /// Malformed postcard payload → ProtocolError. Body carries the
    /// reason so operators can debug schema skew.
    #[tokio::test]
    async fn corrupt_payload_returns_protocol_error() {
        let server = make_server().await;
        let principal = anon_principal();
        // Random bytes that won't decode as PutObjectRequest.
        let (status, body) =
            dispatch_verb(&server, &principal, "put_object", &[0xFF; 8]).await;
        assert_eq!(status, WireStatus::ProtocolError);
        let msg = String::from_utf8_lossy(&body);
        assert!(msg.contains("put_object"), "reason should name the verb: {msg}");
        assert!(msg.contains("postcard decode failed"), "reason: {msg}");
    }

    /// Handler returning Status::permission_denied (e.g.
    /// san_payload_tenant_mismatch) maps to
    /// WireStatus::PermissionDenied; reason carries through.
    #[tokio::test]
    async fn handler_status_maps_to_wire_status() {
        let server = make_server().await;
        // Principal whose canonical SAN tenant id != payload tenant.
        let principal = TcpFramedPrincipal::new(
            "spiffe://kiseki/tenant/00000000-0000-0000-0000-000000000999",
            ConnectionId(0),
        );
        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
        let payload = postcard::to_allocvec(&np::PutObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(ns_proto(ns)),
            name: "x".into(),
            data: b"y".to_vec(),
        })
        .unwrap();
        let (status, body) = dispatch_verb(&server, &principal, "put_object", &payload).await;
        assert_eq!(status, WireStatus::PermissionDenied);
        let reason = String::from_utf8_lossy(&body);
        assert!(
            reason.contains("san_payload_tenant_mismatch"),
            "expected SAN/payload mismatch reason, got: {reason}",
        );
    }

    /// `fsync` doesn't take a body or principal. Confirm dispatch
    /// reaches the handler and round-trips an Ok response.
    #[tokio::test]
    async fn fsync_dispatches_without_principal_or_body() {
        let server = make_server().await;
        let principal = anon_principal();
        let (status, body) = dispatch_verb(&server, &principal, "fsync", b"").await;
        assert_eq!(status, WireStatus::Ok);
        let resp: np::FsyncResponse = postcard::from_bytes(&body).expect("decode");
        assert_eq!(resp.fsynced_lsn, 0);
    }

    #[test]
    fn status_to_wire_covers_all_tonic_codes() {
        // Pin the mapping so a future tonic update that adds a Code
        // doesn't silently default to Internal without a conscious
        // decision. Each variant present in tonic 0.14 is asserted
        // here; if rustc complains about a missing match arm in
        // status_to_wire, this test forces a re-review.
        use tonic::Code;
        let cases: Vec<(Code, WireStatus)> = vec![
            (Code::Ok, WireStatus::Ok),
            (Code::Cancelled, WireStatus::Aborted),
            (Code::Aborted, WireStatus::Aborted),
            (Code::InvalidArgument, WireStatus::InvalidArgument),
            (Code::NotFound, WireStatus::NotFound),
            (Code::AlreadyExists, WireStatus::AlreadyExists),
            (Code::PermissionDenied, WireStatus::PermissionDenied),
            (Code::ResourceExhausted, WireStatus::ResourceExhausted),
            (Code::FailedPrecondition, WireStatus::PreconditionFailed),
            (Code::OutOfRange, WireStatus::OutOfRange),
            (Code::Unavailable, WireStatus::Unavailable),
            (Code::DeadlineExceeded, WireStatus::Unavailable),
            (Code::Unauthenticated, WireStatus::Unauthenticated),
            (Code::Internal, WireStatus::Internal),
            (Code::Unknown, WireStatus::Internal),
            (Code::Unimplemented, WireStatus::Internal),
            (Code::DataLoss, WireStatus::Internal),
        ];
        for (code, want) in cases {
            let st = Status::new(code, "x");
            assert_eq!(status_to_wire(&st), want, "code {code:?}");
        }
    }
}
