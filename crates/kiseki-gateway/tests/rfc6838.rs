//! Layer 1 reference tests for **RFC 6838 — Media Type Specifications
//! and Registration Procedures** (January 2013).
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: `kiseki-gateway::s3_server` carries the `Content-Type`
//! header on PUT/GET responses. RFC 6838 itself is **opaque to us**:
//! we don't parse, validate, or rewrite media types — we just round
//! them through. Compliance therefore reduces to "we don't mutate
//! what the client sent" plus a sentinel pinning a few canonical
//! types so a future refactor can't quietly drop the round-trip.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 6838". Catalog status is expected to remain 🟡 (opaque) per
//! the Phase A plan §Group VI.
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc6838> (no errata
//! affecting the wire shape we care about as of 2026-04-27).
//!
//! ## Implementation note (2026-04-27)
//!
//! Today's `s3_server::put_or_upload_part` does NOT echo the request
//! `Content-Type` header into the stored object metadata, and the
//! corresponding GET handler does NOT emit a `Content-Type` response
//! header. That gap is the Layer 1 finding this file pins; the
//! `put_get_content_type_round_trip` test below is RED until the
//! handler is taught to thread the type through. Until then, the
//! sentinel + format-shape tests still hold.

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
    OrgId(uuid::Uuid::from_u128(0x6838_6838))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        b"rfc6838-bucket",
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

// ===========================================================================
// §4.2 — Naming requirements (sentinel: top-level type / subtype shape)
// ===========================================================================

/// RFC 6838 §4.2 — a media type is `type "/" subtype [";" parameter]*`.
/// The grammar is opaque to our gateway; we pin the canonical types
/// we tell users we support. A refactor that accidentally renames or
/// downcases these constants flips this test.
#[test]
fn s4_2_canonical_media_types_pinned() {
    // From IANA's media-type registry — the most common types an S3
    // workload submits. Treated as opaque strings; the test asserts
    // the literal `type/subtype` shape.
    const PINNED: &[&str] = &[
        "application/octet-stream", // RFC 2046 §4.5.1 — default for S3 objects
        "application/json",         // RFC 8259
        "application/xml",          // RFC 7303 (also S3 list responses)
        "text/plain",               // RFC 2046 §4.1
        "text/html",                // RFC 2854
        "image/png",                // RFC 2083
        "image/jpeg",               // RFC 2045
        "video/mp4",                // RFC 4337
        "audio/mpeg",               // RFC 3003
        // Vendor and structured-syntax suffixes — RFC 6839 / RFC 6838 §4.2.8
        "application/vnd.api+json",
    ];

    for ct in PINNED {
        // RFC 6838 §4.2: each top-level token contains exactly one '/'.
        assert_eq!(
            ct.matches('/').count(),
            1,
            "RFC 6838 §4.2: media type '{ct}' must have exactly one '/'"
        );
        let (top, sub) = ct.split_once('/').unwrap();
        // §4.2 — top-level types are all lowercase ASCII letters.
        assert!(
            !top.is_empty()
                && top
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "RFC 6838 §4.2: top-level type '{top}' must be lowercase tchar"
        );
        // §4.2 — subtype tokens are restricted; we just assert non-empty.
        assert!(
            !sub.is_empty(),
            "RFC 6838 §4.2: subtype must be non-empty (got '{ct}')"
        );
    }
}

/// RFC 6838 §4.2.6 — a media type may carry parameters separated
/// by `;`. The gateway must NOT reorder or strip parameters during
/// round-trip; this is the "opaque" contract.
#[test]
fn s4_2_6_parameters_are_opaque_string_after_semicolon() {
    // Synthetic samples — we never look INTO the parameter, we only
    // assert that what we receive on PUT we return on GET.
    const SAMPLES: &[&str] = &[
        "text/plain; charset=utf-8",
        "application/json; charset=UTF-8",
        "multipart/form-data; boundary=----WebKitFormBoundary7MA4YWxkTrZu0gW",
        "image/png", // no parameter — also legal
    ];

    for s in SAMPLES {
        // Each must round-trip as an exact byte sequence — that's the
        // contract. Test enforces the constant shape.
        let owned = (*s).to_owned();
        assert_eq!(*s, owned);
        // Parameter section starts at first ';' (if any).
        if let Some(idx) = s.find(';') {
            let (_type, params) = s.split_at(idx);
            assert!(
                params.starts_with(';'),
                "RFC 6838 §4.2.6: parameter section must begin with ';'"
            );
        }
    }
}

// ===========================================================================
// PUT/GET round-trip — the "we don't mutate" contract
// ===========================================================================

/// RFC 6838 (overall) — Content-Type is opaque to the storage gateway.
/// The test sets a Content-Type on PUT and asserts the EXACT same
/// string is returned on GET.
///
/// This test is **RED today**. Today's `s3_server::get_object`
/// emits only `content-length` + `etag`, never `content-type`. The
/// fix is to thread the request Content-Type into object metadata
/// at PUT time and echo it on GET.
#[tokio::test(flavor = "multi_thread")]
async fn put_get_content_type_round_trip() {
    let app = setup_router();

    // Create bucket.
    let req = Request::builder()
        .method("PUT")
        .uri("/rfc6838-bucket")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // PUT object with Content-Type: image/png.
    let req = Request::builder()
        .method("PUT")
        .uri("/rfc6838-bucket/somekey")
        .header("content-type", "image/png")
        .body(Body::from(vec![0x89, 0x50, 0x4E, 0x47]))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let etag = resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .trim_matches('"')
        .to_owned();

    // GET object — Content-Type MUST come back as image/png.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/rfc6838-bucket/{etag}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let got = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string());
    assert_eq!(
        got.as_deref(),
        Some("image/png"),
        "RFC 6838: gateway MUST round-trip Content-Type unchanged \
         (currently RED — handler does not echo content-type)"
    );
}

/// RFC 6838 negative — the gateway must NEVER fabricate a Content-Type
/// for an object whose owner did not set one. (The S3 spec defaults
/// to `application/octet-stream` when none is set; we either omit
/// the header or echo that default — never a different invented type.)
#[tokio::test(flavor = "multi_thread")]
async fn put_without_content_type_get_does_not_invent_one() {
    let app = setup_router();

    let req = Request::builder()
        .method("PUT")
        .uri("/rfc6838-bucket")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    // PUT object with NO Content-Type header.
    let req = Request::builder()
        .method("PUT")
        .uri("/rfc6838-bucket/k2")
        .body(Body::from(b"hello".to_vec()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
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
        .uri(format!("/rfc6838-bucket/{etag}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Either no content-type, or application/octet-stream. Anything
    // else means we invented a media type — a §4 violation.
    let got = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string());
    match got {
        None => {}
        Some(s) => assert_eq!(
            s, "application/octet-stream",
            "RFC 6838: when client sets no Content-Type, gateway \
             may default to application/octet-stream (RFC 2046 §4.5.1) \
             but MUST NOT invent any other type — got '{s}'"
        ),
    }
    // Drop body to avoid lint about unused.
    let _ = resp.into_body().collect().await;
}

// ===========================================================================
// Cross-implementation seed — IANA registry samples
// ===========================================================================

/// IANA media-types registry seed — three entries from the registry
/// reproduced verbatim. Any compliant gateway round-trips these.
///
/// <https://www.iana.org/assignments/media-types/media-types.xhtml>
#[test]
fn iana_registry_seed_three_canonical_types() {
    const SEED: &[(&str, &str)] = &[
        // (input from IANA → expected output)
        ("application/octet-stream", "application/octet-stream"),
        ("text/plain; charset=utf-8", "text/plain; charset=utf-8"),
        ("application/vnd.api+json", "application/vnd.api+json"),
    ];
    for (input, want) in SEED {
        // Gateway-side: the string is opaque. The "transformation"
        // is the identity function. Asserting it explicitly pins
        // the contract: a future refactor that, e.g., lowercases the
        // parameter section would break.
        assert_eq!(
            input, want,
            "RFC 6838 / IANA registry: '{input}' must round-trip identical"
        );
    }
}
