//! Layer 1 reference tests for **RFC 9110 — HTTP Semantics** (June
//! 2022), **RFC 9111 — HTTP Caching**, and **RFC 9112 — HTTP/1.1**.
//! These three obsolete RFC 7230-7235 and are the live HTTP/1.1
//! contract.
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: `kiseki-gateway::s3_server` — the axum router that
//! implements HEAD/GET/PUT/POST/DELETE on `/<bucket>/<key>`. axum
//! delegates HTTP/1.1 framing (RFC 9112 §6 chunked, §7 trailers,
//! ...) to hyper; we focus the tests on the **semantics** RFC 9110
//! pins (status codes, ETag, conditional requests, Range) since
//! those are where our handler code emits the bytes.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 9110 / 9111 / 9112". Status expected to be 🟡 → ✅ once
//! the Range / If-Modified-Since / chunked-encoding gaps below are
//! closed.
//!
//! Spec text:
//!   - <https://www.rfc-editor.org/rfc/rfc9110>
//!   - <https://www.rfc-editor.org/rfc/rfc9111>
//!   - <https://www.rfc-editor.org/rfc/rfc9112>

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
    OrgId(uuid::Uuid::from_u128(0x9110_9110))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        b"rfc9110-bucket",
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
    let gw = InMemoryGateway::new(compositions, Box::new(chunks), master_key);
    let s3gw = S3Gateway::new(gw);
    s3_router(s3gw, test_tenant())
}

/// PUT a small object and return its ETag (without quotes). Used
/// as a fixture in the conditional-request tests below.
async fn put_and_get_etag(app: &axum::Router, body: &[u8]) -> String {
    // Create bucket once (idempotent for our purposes — duplicate is 409).
    let req = Request::builder()
        .method("PUT")
        .uri("/rfc9110-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = Request::builder()
        .method("PUT")
        .uri("/rfc9110-bucket/anykey")
        .body(Body::from(body.to_vec()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .trim_matches('"')
        .to_owned()
}

// ===========================================================================
// §8.8.3 — ETag header field
// ===========================================================================

/// RFC 9110 §8.8.3 — ETag is `entity-tag = [ weak ] opaque-tag`,
/// where `weak = "W/"` and `opaque-tag = DQUOTE *etagc DQUOTE`.
/// Strong validators are `"…"`, weak are `W/"…"`. The bytes between
/// the quotes are opaque to the client.
///
/// Our PUT handler emits a strong ETag (`"<uuid>"`). Pin the shape.
#[tokio::test(flavor = "multi_thread")]
async fn s8_8_3_etag_format_is_quoted_or_weak_quoted() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/rfc9110-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = Request::builder()
        .method("PUT")
        .uri("/rfc9110-bucket/etag-key")
        .body(Body::from(b"hello".to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    let etag = resp.headers().get("etag").expect(
        "RFC 9110 §8.8.3: PUT response on a successful upload SHOULD \
         include ETag (S3 always does)",
    );
    let etag_str = etag.to_str().unwrap();

    // Shape: must start and end with DQUOTE, optionally prefixed with `W/`.
    let inner = if let Some(stripped) = etag_str.strip_prefix("W/") {
        stripped
    } else {
        etag_str
    };
    assert!(
        inner.starts_with('"') && inner.ends_with('"') && inner.len() >= 2,
        "RFC 9110 §8.8.3: ETag must be a quoted opaque-tag (got '{etag_str}')"
    );

    // The opaque-tag content per §8.8.3 ABNF: %x21 / %x23-7E / etc.
    // We just spot-check no DQUOTE inside.
    let body = &inner[1..inner.len() - 1];
    assert!(
        !body.contains('"'),
        "RFC 9110 §8.8.3: opaque-tag must not contain unescaped DQUOTE"
    );
    assert!(!body.is_empty(), "ETag body must be non-empty");
}

/// RFC 9110 §8.8.3.2 — entity-tag comparison: "strong comparison"
/// requires both validators be strong AND opaque-tags equal.
/// `W/"abc"` vs `"abc"` are NOT strong-equal but ARE weak-equal.
///
/// Cross-implementation seed: a real curl-style ETag comparison.
#[test]
fn s8_8_3_2_etag_strong_vs_weak_comparison_seed() {
    fn parse(etag: &str) -> (bool, &str) {
        if let Some(rest) = etag.strip_prefix("W/") {
            (true, rest.trim_matches('"'))
        } else {
            (false, etag.trim_matches('"'))
        }
    }
    fn strong_eq(a: &str, b: &str) -> bool {
        let (wa, ta) = parse(a);
        let (wb, tb) = parse(b);
        !wa && !wb && ta == tb
    }
    fn weak_eq(a: &str, b: &str) -> bool {
        let (_, ta) = parse(a);
        let (_, tb) = parse(b);
        ta == tb
    }

    // Strong-eq: both strong with same tag.
    assert!(
        strong_eq("\"abc\"", "\"abc\""),
        "strong_eq: \"abc\" == \"abc\""
    );
    // Strong-eq fails: one is weak.
    assert!(
        !strong_eq("W/\"abc\"", "\"abc\""),
        "strong_eq: W/\"abc\" != \"abc\""
    );
    // Strong-eq fails: both weak.
    assert!(
        !strong_eq("W/\"abc\"", "W/\"abc\""),
        "strong_eq: W/\"abc\" never strong"
    );
    // Weak-eq: any combination of W/ + same tag.
    assert!(weak_eq("W/\"abc\"", "W/\"abc\""));
    assert!(weak_eq("W/\"abc\"", "\"abc\""));
    assert!(weak_eq("\"abc\"", "\"abc\""));
    // Weak-eq fails: tags differ.
    assert!(!weak_eq("\"abc\"", "\"def\""));
}

// ===========================================================================
// §13 — Conditional requests
// ===========================================================================

/// RFC 9110 §13.1.1 — `If-Match` with a value that does NOT match
/// the current ETag MUST yield 412 Precondition Failed.
#[tokio::test(flavor = "multi_thread")]
async fn s13_1_1_if_match_mismatch_returns_412() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"data").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("if-match", "\"deadbeef\"")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PRECONDITION_FAILED,
        "RFC 9110 §13.1.1: If-Match mismatch must return 412"
    );
}

/// RFC 9110 §13.1.1 — `If-Match: *` matches if and only if the
/// resource exists. Since the GET path here uses the ETag as the key,
/// the resource exists, and `*` MUST match.
#[tokio::test(flavor = "multi_thread")]
async fn s13_1_1_if_match_star_matches_existing_resource() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"data").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("if-match", "*")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "RFC 9110 §13.1.1: If-Match: * must match any existing resource"
    );
}

/// RFC 9110 §13.1.2 — `If-None-Match` with a matching ETag MUST
/// return 304 Not Modified for safe methods (GET/HEAD).
#[tokio::test(flavor = "multi_thread")]
async fn s13_1_2_if_none_match_match_returns_304() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"data").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("if-none-match", format!("\"{etag}\""))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_MODIFIED,
        "RFC 9110 §13.1.2: If-None-Match match on GET must return 304"
    );
}

/// RFC 9110 §13.1.2 — `If-None-Match: *` is documented as "the
/// representation does not exist". For an existing resource, this
/// MUST yield 304 (for GET/HEAD).
#[tokio::test(flavor = "multi_thread")]
async fn s13_1_2_if_none_match_star_on_existing_returns_304() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"data").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("if-none-match", "*")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

/// RFC 9110 §13.1.3 — `If-Modified-Since` is evaluated only on
/// safe methods. Today's `s3_server::get_object` does NOT inspect
/// `If-Modified-Since`. This test asserts the contract; it is RED
/// until the handler reads the header and compares against the
/// stored Last-Modified.
#[tokio::test(flavor = "multi_thread")]
async fn s13_1_3_if_modified_since_far_future_returns_304() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"data").await;

    // A date far in the future — every reasonable Last-Modified is
    // before this, so per §13.1.3 the response MUST be 304.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("if-modified-since", "Fri, 31 Dec 2099 23:59:59 GMT")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_MODIFIED,
        "RFC 9110 §13.1.3: If-Modified-Since in the far future \
         must return 304 (currently RED — handler does not inspect \
         the header)"
    );
}

/// RFC 9110 §13.1.4 — `If-Unmodified-Since` with a date in the
/// distant past MUST yield 412 (the resource has been modified
/// since that point).
#[tokio::test(flavor = "multi_thread")]
async fn s13_1_4_if_unmodified_since_distant_past_returns_412() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"data").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("if-unmodified-since", "Thu, 01 Jan 1970 00:00:00 GMT")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PRECONDITION_FAILED,
        "RFC 9110 §13.1.4: If-Unmodified-Since at epoch must return 412 \
         (currently RED — handler does not inspect the header)"
    );
}

// ===========================================================================
// §14 — Range requests (partial GET)
// ===========================================================================

/// RFC 9110 §14.1.2 — Range syntax: `bytes=<first>-<last>`. A valid
/// range request on an existing resource MUST return 206 Partial
/// Content with the requested byte slice.
///
/// **Today's gap**: `s3_server::get_object` does not honor `Range:`.
/// This test is RED until partial-GET support lands.
#[tokio::test(flavor = "multi_thread")]
async fn s14_byte_range_returns_206_partial_content() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"0123456789").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("range", "bytes=0-4")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PARTIAL_CONTENT,
        "RFC 9110 §14.1.2: byte-range GET must return 206 \
         (currently RED — Range: header is not parsed)"
    );

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body.as_ref(),
        b"01234",
        "RFC 9110 §14.1.2: bytes=0-4 must return 5 bytes (inclusive range)"
    );
}

/// RFC 9110 §14.1.2 — multi-range requests: `bytes=0-4,6-9`. The
/// server MAY return either one Range or a `multipart/byteranges`
/// body. We assert the response is NOT a generic 200 (the gap).
#[tokio::test(flavor = "multi_thread")]
async fn s14_multi_byte_range_returns_206_or_416() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"0123456789").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("range", "bytes=0-4,6-9")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "RFC 9110 §14.1.2: a Range request MUST NOT return 200 OK \
         with the full body — must be 206 or 416 \
         (currently RED — handler ignores Range:)"
    );
}

/// RFC 9110 §14.1.2 — suffix-range: `bytes=-N` requests the last N
/// bytes. For a 10-byte object, `bytes=-3` MUST yield "789".
#[tokio::test(flavor = "multi_thread")]
async fn s14_suffix_byte_range() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"0123456789").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("range", "bytes=-3")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PARTIAL_CONTENT,
        "RFC 9110 §14.1.2: suffix-range bytes=-3 must return 206 \
         (currently RED — Range: header is not parsed)"
    );
}

/// RFC 9110 §15.5.17 — 416 Range Not Satisfiable: when the range's
/// first-byte-pos is at or past the resource's representation
/// length. For a 10-byte object, `bytes=100-200` MUST yield 416.
#[tokio::test(flavor = "multi_thread")]
async fn s14_unsatisfiable_range_returns_416() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"0123456789").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .header("range", "bytes=100-200")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::RANGE_NOT_SATISFIABLE,
        "RFC 9110 §15.5.17: out-of-range Range request must return 416 \
         (currently RED — handler ignores Range:)"
    );
}

// ===========================================================================
// §15 — Status codes the gateway emits
// ===========================================================================

/// RFC 9110 §15.3.1 — 200 OK on successful GET.
#[tokio::test(flavor = "multi_thread")]
async fn s15_3_1_200_ok_on_successful_get() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"data").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// RFC 9110 §15.3.5 — 204 No Content on successful DELETE.
#[tokio::test(flavor = "multi_thread")]
async fn s15_3_5_204_no_content_on_delete() {
    let app = setup_router();
    let etag = put_and_get_etag(&app, b"data").await;

    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/rfc9110-bucket/{etag}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "RFC 9110 §15.3.5: successful DELETE returns 204"
    );
}

/// RFC 9110 §15.4.5 / §15.5.5 — pin every status code we emit so
/// a future refactor cannot quietly renumber. (Non-runtime check;
/// confirms the constants we depend on.)
#[test]
fn s15_status_codes_we_emit_pinned() {
    // 2xx
    assert_eq!(StatusCode::OK.as_u16(), 200);
    assert_eq!(StatusCode::CREATED.as_u16(), 201);
    assert_eq!(StatusCode::NO_CONTENT.as_u16(), 204);
    assert_eq!(StatusCode::PARTIAL_CONTENT.as_u16(), 206);
    // 3xx
    assert_eq!(StatusCode::NOT_MODIFIED.as_u16(), 304);
    // 4xx
    assert_eq!(StatusCode::BAD_REQUEST.as_u16(), 400);
    assert_eq!(StatusCode::FORBIDDEN.as_u16(), 403);
    assert_eq!(StatusCode::NOT_FOUND.as_u16(), 404);
    assert_eq!(StatusCode::CONFLICT.as_u16(), 409);
    assert_eq!(StatusCode::PRECONDITION_FAILED.as_u16(), 412);
    assert_eq!(StatusCode::RANGE_NOT_SATISFIABLE.as_u16(), 416);
    // 5xx
    assert_eq!(StatusCode::INTERNAL_SERVER_ERROR.as_u16(), 500);
}

/// RFC 9110 §15.5.5 — 404 Not Found for an object that doesn't
/// exist. Our handler emits 404 when the key is not a valid UUID
/// or doesn't resolve.
#[tokio::test(flavor = "multi_thread")]
async fn s15_5_5_404_not_found_on_missing_object() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/rfc9110-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let nonexistent_uuid = uuid::Uuid::nil().to_string();
    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc9110-bucket/{nonexistent_uuid}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "RFC 9110 §15.5.5: 404 for non-existent object"
    );
}

/// RFC 9110 §15.5.10 — 409 Conflict on duplicate bucket creation
/// (S3 semantics: BucketAlreadyExists).
#[tokio::test(flavor = "multi_thread")]
async fn s15_5_10_409_conflict_on_duplicate_bucket() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/rfc9110-bucket")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second PUT — must conflict.
    let req = Request::builder()
        .method("PUT")
        .uri("/rfc9110-bucket")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "RFC 9110 §15.5.10: duplicate-bucket PUT must return 409"
    );
}

// ===========================================================================
// §6.6 / RFC 9112 §7 — chunked transfer-encoding
// ===========================================================================

/// RFC 9112 §7 (referenced from RFC 9110 §6.1) — a PUT with
/// `Transfer-Encoding: chunked` MUST be accepted; the body is
/// rebuilt by hyper before reaching the handler. Our handler
/// receives `body: Bytes` and never sees the framing.
///
/// This test sends a chunked body via axum/tower and verifies the
/// content arrives intact. (axum-via-`oneshot` invokes the
/// service directly with whatever body we hand it; hyper performs
/// the actual transfer-encoding decode in the real server. For
/// Layer 1 we assert that the handler doesn't reject the request
/// on the absence of `Content-Length` — which would be a violation
/// of §6.6.)
#[tokio::test(flavor = "multi_thread")]
async fn s6_6_chunked_encoding_body_buffered_correctly() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/rfc9110-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    // PUT with explicit chunked encoding header and no Content-Length.
    let req = Request::builder()
        .method("PUT")
        .uri("/rfc9110-bucket/chunked-key")
        .header("transfer-encoding", "chunked")
        .body(Body::from(b"chunkdata".to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "RFC 9112 §7 / RFC 9110 §6.6: chunked PUT must succeed \
         (handler must not require Content-Length when TE: chunked is present)"
    );
}

// ===========================================================================
// Cross-implementation seed — RFC 9110 §8.8.3 example ETags
// ===========================================================================

/// RFC 9110 §8.8.3 verbatim examples. Pin them so a future ETag
/// comparator (when one lands in `s3_server`) is contracted to
/// match these.
#[test]
fn rfc_example_s8_8_3_etag_comparison_matrix() {
    // From RFC 9110 §8.8.3.2 Table 3 — etag comparison matrix.
    // (a, b, strong-eq, weak-eq)
    const MATRIX: &[(&str, &str, bool, bool)] = &[
        ("W/\"1\"", "W/\"1\"", false, true),
        ("W/\"1\"", "W/\"2\"", false, false),
        ("W/\"1\"", "\"1\"", false, true),
        ("\"1\"", "\"1\"", true, true),
    ];
    for (a, b, want_strong, want_weak) in MATRIX {
        let parse = |e: &str| -> (bool, String) {
            if let Some(rest) = e.strip_prefix("W/") {
                (true, rest.trim_matches('"').to_string())
            } else {
                (false, e.trim_matches('"').to_string())
            }
        };
        let (wa, ta) = parse(a);
        let (wb, tb) = parse(b);
        let strong_eq = !wa && !wb && ta == tb;
        let weak_eq = ta == tb;
        assert_eq!(
            strong_eq, *want_strong,
            "RFC 9110 §8.8.3.2 Table 3: strong({a}, {b})"
        );
        assert_eq!(
            weak_eq, *want_weak,
            "RFC 9110 §8.8.3.2 Table 3: weak({a}, {b})"
        );
    }
}
