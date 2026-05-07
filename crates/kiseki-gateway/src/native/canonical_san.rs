//! Canonical SAN URI rules for the native gateway (I-NG1, gate-1 F-H3).
//!
//! A "canonical SAN URI" is a SPIFFE-style URI in the exact form
//! `spiffe://<authority>/tenant/<tenant_id>` where:
//!
//! - **Scheme** is exactly `spiffe` (lower case, no `SPIFFE://`).
//! - **Authority** is lower case (no `Kiseki/`, no IDN homographs;
//!   ASCII-only).
//! - **No trailing slash** on the path.
//! - **No percent-encoding of unreserved characters**. Per RFC 3986,
//!   unreserved = `A-Z` / `a-z` / `0-9` / `-` / `.` / `_` / `~`. A
//!   percent-encoded unreserved (e.g. `org%2Dpharma`) is REJECTED at
//!   the boundary — the byte already had a canonical representation.
//! - **Tenant id is NFC-normalized**. We don't actively normalize on
//!   the wire — instead, we reject anything that isn't already in NFC.
//!   (NFD or other forms are spec-invalid for SPIFFE-style identifiers.)
//! - **ASCII-only** in the authority and tenant id. Cyrillic homographs
//!   or any byte > 0x7f get rejected as `NonAsciiCharacter`.
//!
//! The `canonicalize` function returns either a [`CanonicalSanUri`]
//! (the *byte-equal* form a token will be bound to) or a
//! [`SanError`] with the specific rule that fired. The interceptor
//! emits the rule name as the `reason` on the `PermissionDenied` audit
//! event.

#![allow(clippy::module_name_repetitions)]

use std::fmt;

/// A canonicalized SPIFFE-format SAN URI for a kiseki tenant. Produced
/// only by [`canonicalize`]; the inner string is byte-equal to what
/// the cert presents (no in-place mutation) — if `canonicalize`
/// returns `Ok`, the input was *already* canonical.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CanonicalSanUri(String);

impl CanonicalSanUri {
    /// Borrow the canonical form. Use this when comparing tokens'
    /// `cert_san_canonical` field against the live connection's SAN.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return the `<tenant_id>` segment.
    #[must_use]
    pub fn tenant_id(&self) -> &str {
        // Always present after a successful canonicalize.
        let trailer = self.0.trim_start_matches("spiffe://");
        // path is `<authority>/tenant/<tenant_id>`
        trailer.split("/tenant/").nth(1).unwrap_or("")
    }
}

impl fmt::Display for CanonicalSanUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Rules that can fire during canonicalization. The `reason` field on
/// the audit event mirrors the variant name, so renaming a variant is
/// a breaking change for log pipelines.
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SanError {
    #[error("missing scheme")]
    MissingScheme,
    #[error("scheme not lowercased")]
    SchemeNotLowercased,
    #[error("scheme not 'spiffe' (got {got:?})")]
    UnsupportedScheme { got: String },
    #[error("authority not lowercased")]
    AuthorityNotLowercased,
    #[error("authority empty")]
    AuthorityEmpty,
    #[error("missing /tenant/ path segment")]
    MissingTenantSegment,
    #[error("trailing slash")]
    TrailingSlash,
    #[error("percent-encoded unreserved character")]
    PercentEncodedUnreserved,
    #[error("non-ASCII character")]
    NonAsciiCharacter,
    #[error("non-NFC unicode")]
    NotNfc,
    #[error("tenant id empty")]
    TenantEmpty,
    #[error("malformed: {0}")]
    Malformed(String),
}

/// Validate that `uri` is in canonical SPIFFE-tenant form. Returns the
/// same string wrapped in [`CanonicalSanUri`] on success — never
/// in-place mutates, since the canonical form is what every signed
/// token's `cert_san_canonical` must compare byte-equal to.
#[allow(clippy::missing_errors_doc)]
pub fn canonicalize(uri: &str) -> Result<CanonicalSanUri, SanError> {
    // Sniff the scheme.
    let colon = uri.find(':').ok_or(SanError::MissingScheme)?;
    let scheme = &uri[..colon];
    if scheme.is_empty() {
        return Err(SanError::MissingScheme);
    }
    if scheme != scheme.to_ascii_lowercase() {
        return Err(SanError::SchemeNotLowercased);
    }
    if scheme != "spiffe" {
        return Err(SanError::UnsupportedScheme {
            got: scheme.to_string(),
        });
    }
    let after_scheme = &uri[colon + 1..];
    let after = after_scheme
        .strip_prefix("//")
        .ok_or_else(|| SanError::Malformed("missing '//' after scheme".into()))?;
    // Authority is everything up to the first '/'.
    let (authority, path) = match after.find('/') {
        Some(slash) => (&after[..slash], &after[slash..]),
        None => (after, ""),
    };
    if authority.is_empty() {
        return Err(SanError::AuthorityEmpty);
    }
    // ASCII-only check on the whole URI: catches IDN homographs
    // anywhere (authority OR tenant id).
    if !uri.is_ascii() {
        return Err(SanError::NonAsciiCharacter);
    }
    if authority != authority.to_ascii_lowercase() {
        return Err(SanError::AuthorityNotLowercased);
    }
    // No trailing slash.
    if uri.ends_with('/') {
        return Err(SanError::TrailingSlash);
    }
    // Path must be `/tenant/<tenant_id>`.
    let Some(rest) = path.strip_prefix("/tenant/") else {
        return Err(SanError::MissingTenantSegment);
    };
    if rest.is_empty() {
        return Err(SanError::TenantEmpty);
    }
    // Reject any further `/` — multiple path segments after tenant_id
    // would change the audit-event principal interpretation.
    if rest.contains('/') {
        return Err(SanError::Malformed(
            "extra path segment after tenant id".into(),
        ));
    }
    // Reject percent-encoded unreserved bytes anywhere in the URI.
    reject_percent_encoded_unreserved(uri)?;
    // NFC check — for ASCII-only inputs this is always satisfied;
    // since we already require ASCII-only, this is structurally
    // satisfied. Kept here as a placeholder so a future relaxation
    // of the ASCII rule does not silently bypass NFC.
    Ok(CanonicalSanUri(uri.to_string()))
}

fn reject_percent_encoded_unreserved(uri: &str) -> Result<(), SanError> {
    let bytes = uri.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(SanError::Malformed(
                    "incomplete percent-encoding".into(),
                ));
            }
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];
            let hi_v = decode_hex(hi)
                .ok_or_else(|| SanError::Malformed("invalid percent-encoding".into()))?;
            let lo_v = decode_hex(lo)
                .ok_or_else(|| SanError::Malformed("invalid percent-encoding".into()))?;
            let decoded = (hi_v << 4) | lo_v;
            if is_unreserved(decoded) {
                return Err(SanError::PercentEncodedUnreserved);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    Ok(())
}

fn decode_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn is_unreserved(b: u8) -> bool {
    matches!(b,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn canonical_form_passes() {
        let s = canonicalize("spiffe://kiseki/tenant/org-pharma").unwrap();
        assert_eq!(s.as_str(), "spiffe://kiseki/tenant/org-pharma");
        assert_eq!(s.tenant_id(), "org-pharma");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn canonical_form_with_dots_and_underscores_passes() {
        let s = canonicalize("spiffe://kiseki/tenant/org_x.y-1").unwrap();
        assert_eq!(s.tenant_id(), "org_x.y-1");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trailing_slash_rejected() {
        assert_eq!(
            canonicalize("spiffe://kiseki/tenant/org-pharma/").unwrap_err(),
            SanError::TrailingSlash
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upper_case_scheme_rejected() {
        assert_eq!(
            canonicalize("SPIFFE://kiseki/tenant/org-pharma").unwrap_err(),
            SanError::SchemeNotLowercased
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upper_case_authority_rejected() {
        assert_eq!(
            canonicalize("spiffe://Kiseki/tenant/org-pharma").unwrap_err(),
            SanError::AuthorityNotLowercased
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cyrillic_homograph_rejected() {
        // 'к' is U+043A CYRILLIC SMALL LETTER KA, looks like ASCII 'k'.
        let homograph = "spiffe://кiseki/tenant/org-pharma";
        assert_eq!(
            canonicalize(homograph).unwrap_err(),
            SanError::NonAsciiCharacter
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn percent_encoded_hyphen_rejected() {
        // org%2Dpharma — %2D is '-', an unreserved character.
        assert_eq!(
            canonicalize("spiffe://kiseki/tenant/org%2Dpharma").unwrap_err(),
            SanError::PercentEncodedUnreserved
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsupported_scheme_rejected() {
        let err = canonicalize("https://kiseki/tenant/org-pharma").unwrap_err();
        assert!(matches!(err, SanError::UnsupportedScheme { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_tenant_segment_rejected() {
        assert_eq!(
            canonicalize("spiffe://kiseki/whatever/org-pharma").unwrap_err(),
            SanError::MissingTenantSegment
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_tenant_rejected() {
        assert_eq!(
            canonicalize("spiffe://kiseki/tenant/").unwrap_err(),
            SanError::TrailingSlash
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn extra_path_segment_rejected() {
        let err = canonicalize("spiffe://kiseki/tenant/org-x/extra").unwrap_err();
        assert!(matches!(err, SanError::Malformed(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn percent_encoded_reserved_byte_passes() {
        // %2F is '/', which is RESERVED — encoded form is allowed.
        // We don't decode it (would change the structure), but we
        // shouldn't reject it as percent-encoded-unreserved.
        // This tenant id contains a literal `%2F` byte sequence; the
        // canonical authority + scheme is still fine.
        let s = canonicalize("spiffe://kiseki/tenant/foo%2Fbar").unwrap();
        assert_eq!(s.tenant_id(), "foo%2Fbar");
    }
}
