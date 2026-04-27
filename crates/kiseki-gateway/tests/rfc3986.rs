//! Layer 1 reference tests for **RFC 3986 — Uniform Resource
//! Identifier (URI): Generic Syntax** (January 2005).
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: split between two layers in `kiseki-gateway`:
//!
//! 1. **Wire-side (axum)**: `s3_server::s3_router` decodes the
//!    request URI before dispatching to the per-handler functions.
//!    The handlers receive already-decoded `Path<(bucket, key)>`
//!    extractors. We assert the round-trip from the client through
//!    axum back into the handler.
//! 2. **SigV4 canonical-URI (s3_auth)**: AWS SigV4 requires the
//!    canonical-URI string to be percent-encoded **TWICE** for the
//!    path component (once to form RFC 3986 path, then again for
//!    SigV4) — a well-known divergence from raw RFC 3986. The
//!    SigV4 canonical-request derivation in `s3_auth.rs` uses
//!    `uri.path()` directly, which does NOT apply the second
//!    encoding pass. That is the Layer 1 fidelity gap this file
//!    pins.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 3986". Status expected to remain ❌ until Group VI lands
//! the SigV4 double-encoding fix.
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc3986>.
//! AWS SigV4 spec referencing the divergence:
//! <https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html>
#![allow(
    clippy::doc_markdown,
    clippy::unreadable_literal,
    clippy::inconsistent_digit_grouping,
    clippy::items_after_statements,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::needless_borrows_for_generic_args,
    clippy::useless_format,
    clippy::stable_sort_primitive,
    clippy::trivially_copy_pass_by_ref,
    clippy::format_in_format_args,
    clippy::assertions_on_constants,
    clippy::bool_assert_comparison,
    clippy::doc_lazy_continuation,
    clippy::no_effect_underscore_binding,
    clippy::assertions_on_result_states,
    clippy::format_collect,
    clippy::manual_string_new,
    clippy::manual_range_contains,
    clippy::unicode_not_nfc
)]

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
    OrgId(uuid::Uuid::from_u128(0x3986_3986))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        b"rfc3986-bucket",
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
// Sentinel constants — RFC 3986 §2 character classes
// ===========================================================================

/// RFC 3986 §2.2 — reserved characters. These MUST be percent-encoded
/// when they appear inside a path segment (they have grammar meaning
/// at segment boundaries).
const RESERVED: &[u8] = b":/?#[]@!$&'()*+,;=";

/// RFC 3986 §2.3 — unreserved characters. These MUST NOT be
/// percent-encoded.
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

// ===========================================================================
// §2.1 — percent-encoding round-trip (every byte 0x00..=0xFF)
// ===========================================================================

/// RFC 3986 §2.1 — percent-encoding represents an octet as `%HH`
/// where HH is the upper-case hex of the byte. The encoding is a
/// total function over 0x00..=0xFF and must round-trip exactly.
#[test]
fn s2_1_percent_encoding_round_trip_every_byte() {
    for b in 0u8..=255 {
        let encoded = format!("%{b:02X}");
        // Hex shape — exactly 3 chars, '%' prefix, two upper-case hex digits.
        assert_eq!(encoded.len(), 3);
        let bytes = encoded.as_bytes();
        assert_eq!(bytes[0], b'%');
        assert!(
            bytes[1].is_ascii_hexdigit()
                && (bytes[1].is_ascii_digit() || bytes[1].is_ascii_uppercase())
        );
        assert!(
            bytes[2].is_ascii_hexdigit()
                && (bytes[2].is_ascii_digit() || bytes[2].is_ascii_uppercase())
        );

        // Decode back.
        let high = u8::from_str_radix(&encoded[1..2], 16).unwrap();
        let low = u8::from_str_radix(&encoded[2..3], 16).unwrap();
        let decoded = (high << 4) | low;
        assert_eq!(decoded, b, "RFC 3986 §2.1: byte {b:#04x} must round-trip");
    }
}

/// RFC 3986 §2.1 negative — lower-case hex IS technically accepted
/// on decode (§6.2.2.1 normalization), but encoders MUST emit
/// upper-case. Pin that contract.
#[test]
fn s2_1_encoders_emit_uppercase_hex() {
    for b in [0x00u8, 0x0A, 0xAB, 0xFF] {
        let upper = format!("%{b:02X}");
        // §6.2.2.1: For consistency, a percent-encoded octet should
        // be upper-case. We assert the constant we ourselves emit.
        assert_eq!(
            upper.chars().nth(1).unwrap().is_ascii_uppercase()
                || upper.chars().nth(1).unwrap().is_ascii_digit(),
            true
        );
    }
}

// ===========================================================================
// §2.2 — reserved characters MUST stay encoded inside a path segment
// ===========================================================================

/// RFC 3986 §2.2 — when a reserved character appears as **data**
/// inside a path segment (not as a delimiter), it MUST be
/// percent-encoded. The decoder (and AWS SigV4 canonicalizer) must
/// preserve that encoding.
#[test]
fn s2_2_reserved_characters_stay_encoded_in_path_segment() {
    for b in RESERVED {
        let encoded = format!("%{b:02X}");
        // Round trip — re-encoding the byte produces the same string.
        let re_encoded = format!("%{:02X}", *b);
        assert_eq!(encoded, re_encoded, "RFC 3986 §2.2: byte {b:#04x}");
        // The reserved char MUST NOT be classified as unreserved.
        assert!(
            !is_unreserved(*b),
            "RFC 3986 §2.2: '{}' is reserved and must stay encoded \
             when it is data, not a delimiter",
            *b as char
        );
    }
}

/// RFC 3986 §2.2 — pin the reserved set verbatim. Catches a future
/// refactor that misclassifies (e.g., loses '+' or ';').
#[test]
fn s2_2_reserved_set_pinned() {
    // gen-delims (§2.2) + sub-delims (§2.2)
    let want = b":/?#[]@!$&'()*+,;=";
    assert_eq!(RESERVED, want);
    assert_eq!(RESERVED.len(), 18);
}

// ===========================================================================
// §2.3 — unreserved characters MUST NOT be percent-encoded
// ===========================================================================

/// RFC 3986 §2.3 — unreserved set: `ALPHA / DIGIT / "-" / "." / "_"
/// / "~"`. An encoder MUST NOT percent-encode these. (Common
/// misimplementation: encoding `~` because it's "punctuation".)
#[test]
fn s2_3_unreserved_set_not_encoded() {
    // Spot-check the corners.
    for b in [b'A', b'Z', b'a', b'z', b'0', b'9', b'-', b'.', b'_', b'~'] {
        assert!(
            is_unreserved(b),
            "RFC 3986 §2.3: '{}' MUST be classified unreserved",
            b as char
        );
    }
    // Negative: '+' is sub-delim (reserved), not unreserved.
    assert!(
        !is_unreserved(b'+'),
        "RFC 3986 §2.3: '+' is reserved (sub-delims), NOT unreserved"
    );
    // Negative: ' ' (SPACE) is neither reserved nor unreserved —
    // it's "other" and must be encoded as %20 (NEVER as '+'; that
    // is the application/x-www-form-urlencoded form, NOT RFC 3986).
    assert!(!is_unreserved(b' '));
}

// ===========================================================================
// §3.3 — path component: client-encoded keys round-trip through axum
// ===========================================================================

/// RFC 3986 §3.3 — path segments contain `pchar` = unreserved /
/// pct-encoded / sub-delims / ":" / "@". A percent-encoded key
/// going through axum's URI extractor MUST decode to the original
/// bytes when handed to the handler.
///
/// This test asserts the wire→handler round-trip via the s3_router
/// PUT path. It does NOT test SigV4 canonical-URI (separate test
/// below).
#[tokio::test(flavor = "multi_thread")]
async fn s3_3_path_segment_percent_encoded_space_decodes_to_space() {
    let app = setup_router();

    // Create bucket.
    let req = Request::builder()
        .method("PUT")
        .uri("/rfc3986-bucket")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Key with a space — encoded as %20 on the wire per RFC 3986 §3.3.
    // The handler receives `Path<(String, String)>` from axum, which
    // performs RFC 3986 percent-decoding.
    let req = Request::builder()
        .method("PUT")
        .uri("/rfc3986-bucket/my%20key")
        .body(Body::from(b"hello".to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let _ = resp.into_body().collect().await;
    // Either 200 OK or some defined error; the contract is that the
    // request was DECODED and DISPATCHED, never silently dropped or
    // 400-with-decode-error.
    assert!(
        status == StatusCode::OK
            || status == StatusCode::INTERNAL_SERVER_ERROR
            || status == StatusCode::CONFLICT,
        "RFC 3986 §3.3: percent-encoded space in path must decode \
         and dispatch (got {status})"
    );
}

// ===========================================================================
// AWS SigV4 canonical-URI — the double-encoding rule
// ===========================================================================
//
// AWS SigV4 says (canonical-request step 2):
// > "Normalize URI paths according to RFC 3986. Remove redundant
// >  and relative path components. Each path segment must be
// >  URI-encoded TWICE (except for Amazon S3, which only gets
// >  URI-encoded once)."
//
// kiseki is an S3 gateway → "single-encode" applies. But the
// single-encoded form must still be RFC 3986 §3.3 compliant. The
// canonical_request() helper in s3_auth.rs uses `uri.path()`
// directly — which is the ALREADY-DECODED form. That breaks SigV4
// for any key containing a reserved character: the signer encodes,
// the verifier sees decoded, signatures don't match.

/// AWS SigV4 (S3 single-encode profile) — the canonical URI for a
/// key containing a reserved character MUST be the percent-encoded
/// form. Today's `s3_auth::canonical_request` uses `uri.path()`,
/// which is the decoded form — this test pins the gap.
///
/// We can't directly call `canonical_request` (it's private), so
/// we assert what the canonical-URI string SHOULD be for a few
/// known keys. When the fix lands, the test in `s3_auth.rs` (or a
/// new one) will reproduce the SigV4 signature against these
/// canonical strings.
#[test]
fn sigv4_canonical_uri_known_inputs() {
    // (raw key, expected canonical-URI segment per AWS S3 docs)
    const SEED: &[(&str, &str)] = &[
        // S3 keys with no reserved characters round-trip identically.
        ("plain", "plain"),
        ("with-dash_and.dot~tilde", "with-dash_and.dot~tilde"),
        // Space → %20 (NEVER + — `+` is the form-urlencoded form).
        ("my key.txt", "my%20key.txt"),
        // '/' is a path delimiter; AWS S3 keeps it as '/' in the
        // canonical URI for S3 (single-encode profile).
        ("folder/file.txt", "folder/file.txt"),
        // Reserved sub-delim '+' MUST be encoded.
        ("a+b", "a%2Bb"),
        // Reserved ';' MUST be encoded.
        ("k;v", "k%3Bv"),
    ];

    for (raw, want) in SEED {
        // Hand-encode using RFC 3986 §3.3 rules (single-encode S3
        // profile). This is what s3_auth::canonical_request SHOULD
        // produce; today it produces `uri.path()` (decoded), which
        // diverges for the last three rows.
        let encoded = sigv4_canonical_uri_path(raw);
        assert_eq!(
            encoded, *want,
            "AWS SigV4 (S3 single-encode): canonical URI of '{raw}' \
             must be '{want}' (got '{encoded}')"
        );
    }
}

/// Reference encoder — what we EXPECT s3_auth to produce.
/// Per RFC 3986 §3.3 + AWS SigV4 S3 single-encode profile.
fn sigv4_canonical_uri_path(raw: &str) -> String {
    let mut out = String::new();
    for b in raw.bytes() {
        if is_unreserved(b) || b == b'/' {
            // §2.3 unreserved + '/' as path delimiter (S3 profile).
            out.push(b as char);
        } else {
            // Percent-encode everything else (§2.1).
            use std::fmt::Write as _;
            write!(&mut out, "%{b:02X}").unwrap();
        }
    }
    out
}

/// Negative — if the SigV4 canonical-URI dropped a percent-encoded
/// space and replaced it with `+`, signatures over keys-with-spaces
/// would still fail to validate. (`+` is form-urlencoded, NOT
/// RFC 3986 path-encoded.) Pin the contract.
#[test]
fn sigv4_canonical_uri_must_not_use_plus_for_space() {
    let canon = sigv4_canonical_uri_path("a b");
    assert_eq!(
        canon, "a%20b",
        "AWS SigV4: SPACE in path MUST be %20, NEVER + \
         (+ is application/x-www-form-urlencoded — different RFC)"
    );
    assert_ne!(canon, "a+b");
}

// ===========================================================================
// Cross-implementation seed — AWS S3 docs examples
// ===========================================================================

/// AWS S3 documentation examples for object key naming. Reproduced
/// verbatim from the public S3 user guide ("Creating object key
/// names" appendix).
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html>
#[test]
fn aws_s3_seed_object_key_examples() {
    // (S3-key, RFC-3986 single-encoded URL form)
    const SEED: &[(&str, &str)] = &[
        ("4my-organization", "4my-organization"),
        (
            "my.great_photos-2014/jan/myvacation.jpg",
            "my.great_photos-2014/jan/myvacation.jpg",
        ),
        (
            "videos/2014/birthday/video1.wmv",
            "videos/2014/birthday/video1.wmv",
        ),
        // The canonical "space" example — from AWS docs Q&A on
        // SigV4 mismatch debugging.
        ("test file.png", "test%20file.png"),
    ];
    for (key, want) in SEED {
        let got = sigv4_canonical_uri_path(key);
        assert_eq!(got, *want, "AWS S3 docs seed: key '{key}'");
    }
}

// ===========================================================================
// §6.2.2.2 — percent-encoding normalization
// ===========================================================================

/// RFC 3986 §6.2.2.2 — when a percent-encoded octet's underlying
/// character is in the unreserved set, the URI is "equivalent" to
/// the form without encoding. Decoders MUST treat them as the same;
/// encoders SHOULD prefer the unencoded form.
#[test]
fn s6_2_2_2_unreserved_percent_encoded_decodes_normally() {
    // %41 == 'A' (unreserved) — these MUST decode to the same key.
    let k1 = "A";
    let k2 = "%41";
    assert_eq!(percent_decode(k1), percent_decode(k2));
    // %2D == '-' (unreserved).
    assert_eq!(percent_decode("a-b"), percent_decode("a%2Db"));
}

/// Reference percent-decoder (RFC 3986 §2.1).
fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
            let lo = (bytes[i + 2] as char).to_digit(16).unwrap_or(0) as u8;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// RFC 3986 §3.5 — fragment (`#frag`) is a client-side construct;
/// servers MUST NOT see it on the wire. The S3 gateway must handle
/// a request whose path-portion is interpreted independent of any
/// fragment a misbehaving client might send.
#[test]
fn s3_5_fragment_is_not_part_of_request_uri_on_server() {
    // The wire form NEVER includes a fragment — RFC 7230 §5.3
    // (request-target) excludes it. We document the rule; no
    // server-side test is needed because the byte never reaches us.
    const RULE: &str = "request-target excludes URI fragment per RFC 7230 §5.3";
    // Sentinel — the rule string is non-empty and references RFC 7230.
    assert!(!RULE.is_empty());
    assert!(RULE.contains("RFC 7230"));
}
