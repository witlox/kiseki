#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Layer 1 reference tests for **RFC 7578 — Returning Values from
//! Forms: multipart/form-data** (July 2015).
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: `kiseki-gateway::s3_server`. RFC 7578 backs the AWS S3
//! "POST policy" upload form (browser-based uploads to S3). It is
//! **NOT IMPLEMENTED** in kiseki today — the catalog row is ❌ per
//! the Phase A plan §Group VI (RFC 7578 stays ❌ if multipart isn't
//! implemented).
//!
//! ## Why this file exists at all
//!
//! ADR-023 §D2 requires every catalog row carry a Layer 1 reference
//! test, even ❌ rows. For a not-implemented row, the contract is
//! the **rejection path**: a `POST /<bucket>` with
//! `Content-Type: multipart/form-data` MUST be rejected — not
//! silently mis-parsed, not 500'd as "internal error", but explicitly
//! "we don't support this".
//!
//! ## When this file expands
//!
//! When kiseki implements POST-policy uploads:
//!   1. RFC 7578 §4.1 boundary-delimiter parsing (positive +
//!      negative round-trip).
//!   2. RFC 7578 §4.2 Content-Disposition `form-data; name="file"`
//!      header parsing.
//!   3. RFC 7578 §4.3 multiple file parts in one POST.
//!   4. RFC 7578 §4.4 Content-Type per part.
//!   5. RFC 7578 §4.7 percent-encoding of non-ASCII filenames
//!      (also relates to RFC 8187).
//!   6. RFC 7578 §5 security: size caps, Content-Length verification,
//!      boundary collision rejection.
//!
//! Every TODO above adds 2-3 tests; the catalog row moves ❌ → 🟡 → ✅.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 7578".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc7578>.
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
    OrgId(uuid::Uuid::from_u128(0x7578_7578))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        b"rfc7578-bucket",
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

// A canonical RFC 7578 §4.1 multipart body. Only used as the
// rejected-path payload — we never parse it. Reproduced verbatim
// from the IETF tools example.
const MULTIPART_BODY: &[u8] = b"\
------WebKitFormBoundary7MA4YWxkTrZu0gW\r\n\
Content-Disposition: form-data; name=\"key\"\r\n\
\r\n\
my-uploaded-object.txt\r\n\
------WebKitFormBoundary7MA4YWxkTrZu0gW\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\n\
Content-Type: text/plain\r\n\
\r\n\
hello world\r\n\
------WebKitFormBoundary7MA4YWxkTrZu0gW--\r\n";

// ===========================================================================
// Not-implemented contract — the canonical rejection path
// ===========================================================================

/// RFC 7578 §4 — a `multipart/form-data` POST against the bucket
/// root is the AWS S3 "POST policy" upload form. Until kiseki
/// implements it, the gateway must reject the request — NOT silently
/// 500, NOT 200-with-no-effect. Acceptable rejection codes:
///
///   - `501 Not Implemented` (HTTP / RFC 9110 §15.6.2) — preferred.
///   - `400 Bad Request` (S3 default for unrecognized POST shape).
///   - `405 Method Not Allowed` (axum default if no POST handler).
///
/// What the test asserts: NOT 200, NOT 5xx-internal-error.
///
/// **Today's behavior (2026-04-27)**: the bucket-root POST handler
/// is not registered (the s3_router only puts POST on
/// `/:bucket/:key` for multipart-upload completion). axum returns
/// 405 Method Not Allowed — that satisfies the contract. This test
/// pins it; a future change that adds a POST handler must also
/// implement RFC 7578 properly or fail this test.
#[tokio::test(flavor = "multi_thread")]
async fn s4_multipart_post_at_bucket_root_is_rejected_not_silently_accepted() {
    let app = setup_router();

    // Pre-create the bucket so we exclude "bucket not found" from
    // the failure modes the test could see.
    let req = Request::builder()
        .method("PUT")
        .uri("/rfc7578-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/rfc7578-bucket")
        .header(
            "content-type",
            "multipart/form-data; boundary=----WebKitFormBoundary7MA4YWxkTrZu0gW",
        )
        .body(Body::from(MULTIPART_BODY.to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let _ = resp.into_body().collect().await;

    // The contract: NOT a silent 200. The request was multipart,
    // we don't speak it, the response must reflect that.
    assert_ne!(
        status,
        StatusCode::OK,
        "RFC 7578: gateway MUST NOT silently accept a multipart \
         POST until the format is implemented (got 200 OK — \
         body was likely lost without effect)"
    );
    // The contract: NOT a server-side 5xx (those imply we tried
    // and failed mid-parse, leaking server state to the client).
    assert!(
        !status.is_server_error() || status == StatusCode::NOT_IMPLEMENTED,
        "RFC 7578 / RFC 9110 §15.6: rejection of an unimplemented \
         feature MUST be 501 Not Implemented, NOT a generic 5xx \
         (got {status})"
    );

    // Acceptable rejection codes per the doc-comment above.
    assert!(
        matches!(
            status,
            StatusCode::NOT_IMPLEMENTED | StatusCode::BAD_REQUEST | StatusCode::METHOD_NOT_ALLOWED
        ),
        "RFC 7578 not-implemented contract: expected 501/400/405, got {status}"
    );
}

/// RFC 7578 §4 — same contract for an object-level POST that carries
/// `multipart/form-data`. (The S3 POST upload form usually targets
/// the bucket root, but a misuse against `/<bucket>/<key>` should
/// also be rejected, not silently swallowed.)
#[tokio::test(flavor = "multi_thread")]
async fn s4_multipart_post_at_object_path_is_rejected() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/rfc7578-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/rfc7578-bucket/some-key")
        .header(
            "content-type",
            "multipart/form-data; boundary=----WebKitFormBoundary7MA4YWxkTrZu0gW",
        )
        .body(Body::from(MULTIPART_BODY.to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let _ = resp.into_body().collect().await;

    // The /:bucket/:key POST path is wired for multipart-upload
    // completion (with ?uploads or ?uploadId query param). A
    // multipart/form-data body without those params should error.
    assert_ne!(
        status,
        StatusCode::OK,
        "RFC 7578: object-level POST with multipart/form-data \
         must not be silently accepted (got 200)"
    );
}

// ===========================================================================
// §4.1 — boundary-delimiter shape (sentinel; not yet exercised by code)
// ===========================================================================

/// RFC 7578 §4.1 — the body is a sequence of "parts" separated by
/// `--<boundary>` lines, terminated by `--<boundary>--`. The boundary
/// is declared in the request `Content-Type: multipart/form-data;
/// boundary=...`. Pin the canonical shape so a future parser knows
/// the contract.
///
/// Today this is shape-only — when an actual parser lands, expand
/// to round-trip + boundary-collision negative tests.
#[test]
fn s4_1_boundary_shape_canonical_form_pinned() {
    // The body MUST start with `--<boundary>\r\n` and end with
    // `--<boundary>--\r\n`. Verify our seed satisfies both rules.
    let boundary = b"----WebKitFormBoundary7MA4YWxkTrZu0gW";
    let body = MULTIPART_BODY;

    let opener = {
        let mut v = vec![b'-', b'-'];
        v.extend_from_slice(boundary);
        v.extend_from_slice(b"\r\n");
        v
    };
    assert!(
        body.starts_with(&opener),
        "RFC 7578 §4.1: body must open with `--<boundary>\\r\\n`"
    );

    let closer = {
        let mut v = vec![b'-', b'-'];
        v.extend_from_slice(boundary);
        v.extend_from_slice(b"--\r\n");
        v
    };
    assert!(
        body.ends_with(&closer),
        "RFC 7578 §4.1: body must end with `--<boundary>--\\r\\n`"
    );

    // §4.1: boundary itself is 1..=70 bytes per RFC 2046 §5.1.1.
    assert!(
        (1..=70).contains(&boundary.len()),
        "RFC 2046 §5.1.1 (referenced by RFC 7578): boundary length \
         MUST be 1..=70 (got {})",
        boundary.len()
    );
}

// ===========================================================================
// Cross-implementation seed — RFC 7578 §4.2 example
// ===========================================================================

/// RFC 7578 §4.2 example, paraphrased here as a seed. Pins the
/// `Content-Disposition: form-data; name="..."` header shape.
/// When the real parser lands, this test asserts it parses
/// `name="file"` + the optional `filename="..."` parameter.
#[test]
fn rfc_example_s4_2_content_disposition_shape() {
    const SEED: &[(&str, &str)] = &[
        // (header value → expected `name` parameter)
        ("form-data; name=\"file\"", "file"),
        ("form-data; name=\"key\"", "key"),
        ("form-data; name=\"file\"; filename=\"hello.txt\"", "file"),
    ];
    for (header, expected_name) in SEED {
        // Shape check: every value MUST start with `form-data;`.
        assert!(
            header.starts_with("form-data;"),
            "RFC 7578 §4.2: every part has Content-Disposition \
             starting with 'form-data;' (got '{header}')"
        );
        assert!(
            header.contains(&format!("name=\"{expected_name}\"")),
            "RFC 7578 §4.2: name parameter must be present and \
             quoted (expected name=\"{expected_name}\")"
        );
    }
}
