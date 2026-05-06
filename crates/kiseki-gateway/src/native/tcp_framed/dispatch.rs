//! Verb dispatch for the TCP-framed binding (ADR-042 §2.2, V3 wire).
//!
//! Per-frame work:
//! 1. Postcard-decode the request `meta` bytes against the verb's
//!    typed request struct.
//! 2. For request-bulk verbs (`put_object`, `write`): attach the
//!    request `bulk` bytes onto the typed struct's bulk field.
//! 3. Call the corresponding inherent method on [`ServerImpl`].
//! 4. For response-bulk verbs (`get_object`, `read`): take the bulk
//!    field OUT of the response, postcard-encode the rest as
//!    `meta_bytes`, return the bulk separately.
//! 5. For non-bulk verbs: postcard-encode the full response as
//!    `meta_bytes`, `bulk_bytes` is empty.
//!
//! Returns `(WireStatus, meta_bytes, bulk_bytes)` — the V3 frame
//! layout writes these as three iovecs in one syscall, no postcard
//! encode/decode of the bulk path on either side.

use kiseki_proto::native_contract::wire_tcp_framed::WireStatus;
use kiseki_proto::native_contract::RequestPrincipal;
use kiseki_proto::v1::native as np;
use tonic::Status;

use crate::native::server::ServerImpl;

/// V3 dispatch outcome: `(status, meta_bytes, bulk_bytes)`. Bulk is
/// empty for non-bulk verbs and on error responses.
pub type DispatchOutcome = (WireStatus, Vec<u8>, Vec<u8>);

/// Map a `tonic::Status` (returned by `ServerImpl` inherent methods)
/// to the wire's [`WireStatus`] byte. The reason string travels in
/// `meta_bytes`, encoded by the dispatch caller.
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
        Code::Internal | Code::Unknown | Code::Unimplemented | Code::DataLoss => {
            WireStatus::Internal
        }
    }
}

/// Build an error outcome: status mapped, meta = reason string,
/// bulk empty.
fn err_outcome(verb: &str, status: &Status) -> DispatchOutcome {
    let reason = status.message();
    let payload = if reason.is_empty() {
        format!("{verb}: handler returned status without message").into_bytes()
    } else {
        reason.as_bytes().to_vec()
    };
    (status_to_wire(status), payload, Vec::new())
}

fn protocol_error(verb: &str, e: impl std::fmt::Display) -> DispatchOutcome {
    (
        WireStatus::ProtocolError,
        format!("postcard decode failed in {verb}: {e}").into_bytes(),
        Vec::new(),
    )
}

fn internal_encode_error(verb: &str, e: impl std::fmt::Display) -> DispatchOutcome {
    (
        WireStatus::Internal,
        format!("postcard encode failed in {verb}: {e}").into_bytes(),
        Vec::new(),
    )
}

/// Macro: one match arm for a unary, NON-bulk verb. Decodes
/// `req_meta` as the typed request, calls the handler, encodes the
/// response as `meta_bytes`. `bulk_bytes` is always empty.
macro_rules! unary_verb {
    ($verb:literal, $req_meta:expr, $handler:expr) => {{
        let req = match postcard::from_bytes($req_meta) {
            Ok(r) => r,
            Err(e) => return protocol_error($verb, e),
        };
        match $handler(req).await {
            Ok(resp) => match postcard::to_allocvec(&resp) {
                Ok(meta) => (WireStatus::Ok, meta, Vec::new()),
                Err(e) => internal_encode_error($verb, e),
            },
            Err(status) => err_outcome($verb, &status),
        }
    }};
}

/// Dispatch one V3 TCP-framed RPC. Returns `(status, meta, bulk)`.
/// See module docs for the per-verb logic.
pub async fn dispatch_verb(
    server: &ServerImpl,
    principal: &dyn RequestPrincipal,
    verb_tag: &str,
    req_meta: &[u8],
    req_bulk: &[u8],
) -> DispatchOutcome {
    match verb_tag {
        // ---------------- Object verbs ----------------
        // Request-bulk verb: PutObjectRequest.data ← req_bulk.
        "put_object" => {
            let mut req: np::PutObjectRequest = match postcard::from_bytes(req_meta) {
                Ok(r) => r,
                Err(e) => return protocol_error("put_object", e),
            };
            // V3: attach bulk bytes onto the typed struct's bulk
            // field. The encoded `meta_bytes` had `data` empty; the
            // server reassembles here. One memcopy from the wire
            // buffer into the Vec<u8> of the request struct.
            req.data = req_bulk.to_vec();
            match server.put_object(principal, req).await {
                Ok(resp) => match postcard::to_allocvec(&resp) {
                    Ok(meta) => (WireStatus::Ok, meta, Vec::new()),
                    Err(e) => internal_encode_error("put_object", e),
                },
                Err(status) => err_outcome("put_object", &status),
            }
        }
        // Response-bulk verb: GetObjectResponse.data → bulk.
        "get_object" => {
            let req: np::GetObjectRequest = match postcard::from_bytes(req_meta) {
                Ok(r) => r,
                Err(e) => return protocol_error("get_object", e),
            };
            match server.get_object(principal, req).await {
                Ok(mut resp) => {
                    // V3 hot path: take the bulk OUT of the response
                    // before postcard-encoding, ship it raw on the
                    // wire as `bulk_bytes`. Skips one full-body
                    // postcard encode + matching decode on the
                    // client (84% of CPU pre-fix at 64 KiB).
                    let bulk = std::mem::take(&mut resp.data);
                    match postcard::to_allocvec(&resp) {
                        Ok(meta) => (WireStatus::Ok, meta, bulk),
                        Err(e) => internal_encode_error("get_object", e),
                    }
                }
                Err(status) => err_outcome("get_object", &status),
            }
        }
        "delete_object" => unary_verb!(
            "delete_object",
            req_meta,
            |req: np::DeleteObjectRequest| server.delete_object(principal, req)
        ),
        "head_object" => unary_verb!("head_object", req_meta, |req: np::HeadObjectRequest| {
            server.head_object(principal, req)
        }),
        "list_objects" => unary_verb!(
            "list_objects",
            req_meta,
            |req: np::ListObjectsRequest| server.list_objects(principal, req)
        ),
        "lookup_by_name" => unary_verb!(
            "lookup_by_name",
            req_meta,
            |req: np::LookupByNameRequest| server.lookup_by_name(principal, req)
        ),
        // ---------------- Multipart ----------------
        "init_multipart" => unary_verb!(
            "init_multipart",
            req_meta,
            |req: np::InitMultipartRequest| server.init_multipart(principal, req)
        ),
        "complete_multipart" => unary_verb!(
            "complete_multipart",
            req_meta,
            |req: np::CompleteMultipartRequest| server.complete_multipart(principal, req)
        ),
        "abort_multipart" => unary_verb!(
            "abort_multipart",
            req_meta,
            |req: np::AbortMultipartRequest| server.abort_multipart(principal, req)
        ),
        // ---------------- Lease ----------------
        "acquire_lease" => unary_verb!(
            "acquire_lease",
            req_meta,
            |req: np::AcquireLeaseRequest| server.acquire_lease(principal, req)
        ),
        "renew_lease" => unary_verb!("renew_lease", req_meta, |req: np::RenewLeaseRequest| {
            server.renew_lease(principal, req)
        }),
        "release_lease" => unary_verb!(
            "release_lease",
            req_meta,
            |req: np::ReleaseLeaseRequest| server.release_lease(principal, req)
        ),
        // ---------------- DEK fetch ----------------
        "fetch_dek" => unary_verb!("fetch_dek", req_meta, |req: np::FetchDekRequest| {
            server.fetch_dek(principal, req)
        }),
        "batch_fetch_dek" => unary_verb!(
            "batch_fetch_dek",
            req_meta,
            |req: np::BatchFetchDekRequest| server.batch_fetch_dek(principal, req)
        ),
        // ---------------- Topology ----------------
        "get_topology" => unary_verb!(
            "get_topology",
            req_meta,
            |req: np::GetTopologyRequest| server.get_topology(principal, req)
        ),
        // No-principal cluster-wide flush.
        "fsync" => match server.fsync().await {
            Ok(resp) => match postcard::to_allocvec(&resp) {
                Ok(meta) => (WireStatus::Ok, meta, Vec::new()),
                Err(e) => internal_encode_error("fsync", e),
            },
            Err(status) => err_outcome("fsync", &status),
        },
        _ => (
            WireStatus::UnknownVerb,
            verb_tag.as_bytes().to_vec(),
            Vec::new(),
        ),
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

    /// V3 PUT round-trip via dispatch: meta carries a PutObjectRequest
    /// with empty `data`; the actual bulk rides as `req_bulk`.
    /// Server reassembles, calls handler, returns
    /// (Ok, postcard(PutObjectResponse), empty bulk).
    #[tokio::test]
    async fn put_object_v3_dispatches_with_bulk_split() {
        let server = make_server().await;
        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
        let mut req = np::PutObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(ns_proto(ns)),
            name: "alpha".into(),
            data: Vec::new(), // empty in meta — bulk rides separately
        };
        let meta_bytes = postcard::to_allocvec(&req).unwrap();
        req.data = b"hello".to_vec(); // would-be bulk
        let bulk_bytes = req.data;
        let principal = anon_principal();
        let (status, resp_meta, resp_bulk) =
            dispatch_verb(&server, &principal, "put_object", &meta_bytes, &bulk_bytes).await;
        assert_eq!(status, WireStatus::Ok);
        assert!(resp_bulk.is_empty(), "PutObject response has no bulk");
        let resp: np::PutObjectResponse = postcard::from_bytes(&resp_meta).unwrap();
        assert_eq!(resp.size, 5);
        assert!(resp.composition_id.is_some());
    }

    /// V3 GET round-trip: server splits the response's `data` field
    /// out of the postcard meta and returns it as `bulk_bytes`.
    /// Client reassembles. The full GetObjectResponse rides the
    /// wire WITHOUT postcard-encoding the bulk bytes themselves.
    #[tokio::test]
    async fn get_object_v3_dispatches_with_bulk_split() {
        let server = make_server().await;
        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
        let principal = anon_principal();

        // Seed an object via dispatch.
        let put_req = np::PutObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(ns_proto(ns)),
            name: "k".into(),
            data: Vec::new(),
        };
        let put_meta = postcard::to_allocvec(&put_req).unwrap();
        let (s, _, _) =
            dispatch_verb(&server, &principal, "put_object", &put_meta, b"value").await;
        assert_eq!(s, WireStatus::Ok);

        // GET via dispatch: lookup by name.
        let get_req = np::GetObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(ns_proto(ns)),
            range_start: 0,
            range_end: 0,
            key: Some(np::get_object_request::Key::Name("k".into())),
        };
        let get_meta = postcard::to_allocvec(&get_req).unwrap();
        let (status, resp_meta, resp_bulk) =
            dispatch_verb(&server, &principal, "get_object", &get_meta, &[]).await;
        assert_eq!(status, WireStatus::Ok);
        // Bulk on the wire is the actual data.
        assert_eq!(resp_bulk, b"value");
        // Meta decodes as GetObjectResponse with EMPTY data — the
        // bulk has been hoisted out.
        let resp: np::GetObjectResponse = postcard::from_bytes(&resp_meta).unwrap();
        assert!(resp.data.is_empty(), "data field must be empty in meta");
        assert_eq!(resp.size, 5);
    }

    #[tokio::test]
    async fn unknown_verb_returns_unknown_verb_with_tag_in_meta() {
        let server = make_server().await;
        let principal = anon_principal();
        let (status, meta, bulk) =
            dispatch_verb(&server, &principal, "not_a_verb", &[], &[]).await;
        assert_eq!(status, WireStatus::UnknownVerb);
        assert_eq!(meta, b"not_a_verb");
        assert!(bulk.is_empty());
    }

    #[tokio::test]
    async fn corrupt_meta_returns_protocol_error() {
        let server = make_server().await;
        let principal = anon_principal();
        let (status, _meta, bulk) =
            dispatch_verb(&server, &principal, "lookup_by_name", &[0xFF; 4], &[]).await;
        assert_eq!(status, WireStatus::ProtocolError);
        assert!(bulk.is_empty());
    }

    #[tokio::test]
    async fn fsync_dispatches_without_principal_or_body() {
        let server = make_server().await;
        let principal = anon_principal();
        let (status, meta, bulk) =
            dispatch_verb(&server, &principal, "fsync", &[], &[]).await;
        assert_eq!(status, WireStatus::Ok);
        assert!(bulk.is_empty());
        let _resp: np::FsyncResponse = postcard::from_bytes(&meta).expect("decode");
    }

    #[tokio::test]
    async fn streaming_verbs_surface_as_unknown_verb() {
        let server = make_server().await;
        let principal = anon_principal();
        for verb in [
            "put_object_stream",
            "get_object_stream",
            "put_part",
            "read_stream",
            "write_stream",
        ] {
            let (status, _, _) = dispatch_verb(&server, &principal, verb, &[], &[]).await;
            assert_eq!(status, WireStatus::UnknownVerb);
        }
    }

    #[test]
    fn status_to_wire_covers_all_tonic_codes() {
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
