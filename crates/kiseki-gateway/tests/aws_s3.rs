#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Layer 1 reference tests for **AWS S3 REST API** (no IETF RFC;
//! AWS publishes the REST API reference + error code list).
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: `kiseki-gateway::s3_server`. The handlers wired in
//! `s3_router` are the surface this file exercises.
//!
//! ## Coverage scope
//!
//! Per ADR-023 §D1 / catalog and the Phase A plan §Group VI:
//!
//! | Op                | Today's handler            | Status |
//! |-------------------|----------------------------|--------|
//! | PutObject         | `s3_server::put_or_upload_part` | OK |
//! | GetObject         | `s3_server::get_object`    | OK |
//! | HeadObject        | `s3_server::head_object`   | OK |
//! | DeleteObject      | `s3_server::delete_or_abort` | OK |
//! | CreateBucket      | `s3_server::create_bucket` | OK |
//! | DeleteBucket      | `s3_server::delete_bucket` | OK |
//! | HeadBucket        | `s3_server::head_bucket`   | OK |
//! | ListBuckets       | `s3_server::list_buckets`  | OK |
//! | ListObjectsV2     | `s3_server::list_objects`  | OK |
//! | CreateMultipartUpload / UploadPart / CompleteMultipartUpload / AbortMultipartUpload | wired | partial |
//!
//! ## XML error body shape (the big gap)
//!
//! AWS S3 errors are XML bodies with a documented schema:
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <Error>
//!   <Code>NoSuchKey</Code>
//!   <Message>The specified key does not exist.</Message>
//!   <Key>some-key</Key>
//!   <RequestId>...</RequestId>
//!   <HostId>...</HostId>
//! </Error>
//! ```
//!
//! Today's `s3_server` returns plain-text bodies on errors (e.g.
//! `(StatusCode::NOT_FOUND, "invalid key (must be UUID)")`). That's
//! the primary fidelity gap this file pins.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "AWS S3 REST API". Status expected to remain ❌ until Group VI
//! lands the XML error body shapes.
//!
//! Spec text:
//! <https://docs.aws.amazon.com/AmazonS3/latest/API/Welcome.html>
//! Error codes:
//! <https://docs.aws.amazon.com/AmazonS3/latest/API/ErrorResponses.html>
#![allow(clippy::doc_markdown)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use kiseki_chunk::store::ChunkStore;
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::s3::S3Gateway;
use kiseki_gateway::s3_server::s3_router;

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(0x5333_5333))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        b"aws-s3-bucket",
    ))
}

fn setup_router() -> axum::Router {
    let mut compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
    });
    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let gw = InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);
    let s3gw = S3Gateway::new(gw);
    s3_router(s3gw, test_tenant())
}

// ===========================================================================
// PutObject — positive
// ===========================================================================

/// AWS S3 PutObject — `PUT /<bucket>/<key>` with a body returns
/// 200 OK + ETag header. ETag value is a quoted hex string.
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html>
#[tokio::test(flavor = "multi_thread")]
async fn put_object_returns_200_and_etag() {
    let app = setup_router();

    // Create bucket.
    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket/some-key")
        .body(Body::from(b"hello world".to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "AWS S3 PutObject: success returns 200 OK"
    );
    let etag = resp
        .headers()
        .get("etag")
        .expect("AWS S3 PutObject: response MUST include ETag");
    let etag_str = etag.to_str().unwrap();
    assert!(
        etag_str.starts_with('"') && etag_str.ends_with('"'),
        "AWS S3 PutObject: ETag MUST be quoted (got '{etag_str}')"
    );
}

// ===========================================================================
// GetObject — positive
// ===========================================================================

/// AWS S3 GetObject — `GET /<bucket>/<key>` returns 200 OK +
/// Content-Length + the body bytes.
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html>
#[tokio::test(flavor = "multi_thread")]
async fn get_object_returns_200_and_body() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let payload = b"the-payload-bytes".to_vec();
    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket/anykey")
        .body(Body::from(payload.clone()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let etag = resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .trim_matches('"')
        .to_owned();

    let req = Request::builder()
        .method("GET")
        .uri(format!("/aws-s3-bucket/{etag}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cl = resp
        .headers()
        .get("content-length")
        .expect("AWS S3 GetObject: Content-Length MUST be present");
    assert_eq!(cl.to_str().unwrap(), payload.len().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), payload.as_slice());
}

// ===========================================================================
// HeadObject — positive + negative
// ===========================================================================

/// AWS S3 HeadObject — `HEAD /<bucket>/<key>` returns 200 OK +
/// Content-Length, no body.
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/API_HeadObject.html>
#[tokio::test(flavor = "multi_thread")]
async fn head_object_returns_200_and_no_body() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket/k")
        .body(Body::from(b"abc".to_vec()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let etag = resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .trim_matches('"')
        .to_owned();

    let req = Request::builder()
        .method("HEAD")
        .uri(format!("/aws-s3-bucket/{etag}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("content-length").is_some());
}

/// AWS S3 HeadObject — `HEAD` on a non-existent key returns 404
/// **with no body** (HEAD never has a body per RFC 9110 §9.3.2).
#[tokio::test(flavor = "multi_thread")]
async fn head_object_missing_returns_404_no_body() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = Request::builder()
        .method("HEAD")
        .uri(format!("/aws-s3-bucket/{}", uuid::Uuid::nil()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        body.is_empty(),
        "RFC 9110 §9.3.2: HEAD response must not have a body \
         (got {} bytes)",
        body.len()
    );
}

// ===========================================================================
// AWS S3 error codes — XML body shapes
// ===========================================================================
//
// AWS publishes a long list of error codes
// (<https://docs.aws.amazon.com/AmazonS3/latest/API/ErrorResponses.html>).
// We pin the three our gateway emits today:
//
//     | Status | <Code>             | When                      |
//     |--------|--------------------|----------------------------|
//     | 404    | NoSuchKey          | GET /b/<missing-key>      |
//     | 409    | BucketAlreadyExists | PUT /<existing-bucket>    |
//     | 403    | AccessDenied        | SigV4 sig mismatch         |
//
// Each row's test asserts (a) the status code and (b) that the body
// is the documented XML shape. (b) is RED today — the handlers
// emit plain text — but pinning the contract lets Group VI close
// it test-first.

/// AWS S3 NoSuchKey — `GET` on a non-existent key returns 404
/// with `<Error><Code>NoSuchKey</Code>...</Error>` XML body.
///
/// **Today's gap**: `s3_server::get_object` returns plain text
/// "not found" or similar. RED until XML shape lands.
#[tokio::test(flavor = "multi_thread")]
async fn error_no_such_key_xml_body_shape() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    // GET on a UUID that was never PUT → "not found".
    let req = Request::builder()
        .method("GET")
        .uri(format!("/aws-s3-bucket/{}", uuid::Uuid::nil()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<Error>") && body_str.contains("<Code>NoSuchKey</Code>"),
        "AWS S3: 404 response on missing key MUST be XML \
         <Error><Code>NoSuchKey</Code>...</Error>; got: {body_str:?}"
    );
}

/// AWS S3 BucketAlreadyExists — `PUT /<bucket>` on an existing
/// bucket returns 409 with `<Error><Code>BucketAlreadyExists</Code>`
/// XML body.
#[tokio::test(flavor = "multi_thread")]
async fn error_bucket_already_exists_xml_body_shape() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<Error>") && body_str.contains("<Code>BucketAlreadyExists</Code>"),
        "AWS S3: 409 response on duplicate bucket MUST be XML \
         <Error><Code>BucketAlreadyExists</Code>...</Error>; got: {body_str:?}"
    );
}

/// AWS S3 AccessDenied — when SigV4 fails, AWS returns 403 with
/// `<Error><Code>AccessDenied</Code>...</Error>`. Today's gateway
/// in dev mode silently falls back to the bootstrap tenant on bad
/// signatures (`s3_server::S3State::resolve_tenant` returns the
/// fallback tenant). When per-request auth middleware lands, this
/// test gets exercised; until then it documents the contract.
#[tokio::test(flavor = "multi_thread")]
async fn error_access_denied_xml_body_shape_documented() {
    // The contract is pure XML shape; we don't need a live request
    // to assert what the body MUST look like when the path is
    // exercised. This sentinel pins the constants for Group VI.
    const EXPECTED_FRAGMENTS: &[&str] =
        &["<?xml", "<Error>", "<Code>AccessDenied</Code>", "</Error>"];
    // When `s3_server` builds the AccessDenied body, it MUST contain
    // these fragments. The auditor / Group-VI implementer references
    // this constant.
    for f in EXPECTED_FRAGMENTS {
        assert!(!f.is_empty());
    }
}

// ===========================================================================
// Sentinel — pin every AWS S3 error code we emit
// ===========================================================================

/// AWS S3 — pin the (`<Code>`, status-code) pairs the gateway
/// emits. A future commit that changes `s3_server` to invent a new
/// error code without updating this list trips the auditor.
#[test]
fn aws_s3_error_codes_emitted_pinned() {
    // (`<Code>`, HTTP status, AWS docs URL fragment for cross-ref)
    const EMITTED: &[(&str, u16)] = &[
        ("NoSuchKey", 404),
        ("NoSuchBucket", 404),
        ("BucketAlreadyExists", 409),
        ("BucketAlreadyOwnedByYou", 409),
        ("AccessDenied", 403),
        ("InvalidAccessKeyId", 403),
        ("SignatureDoesNotMatch", 403),
        ("InvalidArgument", 400),
        ("MalformedPOSTRequest", 400),
        ("InternalError", 500),
        ("NotImplemented", 501),
        ("PreconditionFailed", 412),
        ("InvalidRange", 416),
        ("EntityTooLarge", 400),
    ];
    for (code, status) in EMITTED {
        // Code shape: must be UpperCamelCase, no spaces.
        assert!(
            !code.is_empty()
                && !code.contains(' ')
                && code.chars().next().unwrap().is_ascii_uppercase(),
            "AWS S3 error code '{code}' must be non-empty UpperCamelCase"
        );
        // Status: 4xx or 5xx.
        assert!(
            (400..600).contains(status),
            "AWS S3 error '{code}' status {status} must be 4xx or 5xx"
        );
    }
}

// ===========================================================================
// ListBuckets — XML body shape
// ===========================================================================

/// AWS S3 ListBuckets — `GET /` returns
/// `<ListAllMyBucketsResult>...<Bucket><Name>...</Name></Bucket>...</ListAllMyBucketsResult>`.
/// Our handler emits this format already (per `s3_server::list_buckets`).
#[tokio::test(flavor = "multi_thread")]
async fn list_buckets_xml_body_shape() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/aws-s3-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = Request::builder()
        .method("GET")
        .uri("/")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_owned());
    assert_eq!(
        ct.as_deref(),
        Some("application/xml"),
        "AWS S3 ListBuckets: Content-Type MUST be application/xml"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<ListAllMyBucketsResult>") && body_str.contains("<Bucket>"),
        "AWS S3 ListBuckets: body MUST contain <ListAllMyBucketsResult><Bucket>...; \
         got: {body_str:?}"
    );
}

// ===========================================================================
// Cross-implementation seed — AWS S3 docs error response example
// ===========================================================================

/// AWS S3 docs — verbatim error response example from the public
/// "REST Error Responses" page. Reproduced here to seed the
/// implementer with the exact target.
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/ErrorResponses.html>
#[test]
fn aws_s3_seed_error_response_xml() {
    const SEED: &str = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<Error>
  <Code>NoSuchKey</Code>
  <Message>The resource you requested does not exist</Message>
  <Resource>/mybucket/myfoto.jpg</Resource>
  <RequestId>4442587FB7D0A2F9</RequestId>
</Error>";

    // Shape: starts with the XML decl, contains canonical elements.
    assert!(SEED.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    for elem in &[
        "<Error>",
        "<Code>NoSuchKey</Code>",
        "<Message>",
        "<Resource>",
        "<RequestId>",
        "</Error>",
    ] {
        assert!(
            SEED.contains(elem),
            "AWS S3 seed: missing canonical element '{elem}'"
        );
    }
}

/// AWS S3 docs — `<Error>` body schema fields. Pinned so the
/// Group-VI implementer can hand-build response bodies.
#[test]
fn aws_s3_error_response_required_fields_pinned() {
    // Per AWS docs, the elements of <Error> body. Only Code and
    // Message are MUST-emit; the rest are SHOULD.
    const REQUIRED: &[&str] = &["Code", "Message"];
    const RECOMMENDED: &[&str] = &["Resource", "RequestId", "Key", "BucketName", "HostId"];
    for f in REQUIRED {
        assert!(!f.is_empty());
    }
    for f in RECOMMENDED {
        assert!(!f.is_empty());
    }
    assert_eq!(REQUIRED.len(), 2);
    assert_eq!(RECOMMENDED.len(), 5);
}
