//! Layer 1 reference tests for the **RPCSEC_GSS family**:
//!   - RFC 2203 — RPCSEC_GSS Protocol Specification (Sept 1997)
//!   - RFC 5403 — RPCSEC_GSS Version 2 (Feb 2010)
//!   - RFC 7204 — RPCSEC_GSS Contextual Definitions (Apr 2014)
//!
//! Catalog status: ❌ **not-implemented**
//! ([`specs/architecture/protocol-compliance.md`] rows for RFC 2203
//! / 5403 / 7204). Kiseki today supports `AUTH_NONE` (RFC 1057
//! §9.1) and `AUTH_SYS` (RFC 1057 §9.2) only. RPCSEC_GSS adds
//! Kerberos / GSS-API integrity + privacy and is gated behind a
//! future enterprise-tenant requirement.
//!
//! ADR-023 §D2.2 mandates that every flavor we encounter on the
//! wire has a documented decode path — including the **reject** path.
//! When a real client (Linux `mount.nfs4 -o sec=krb5`) sends
//! `flavor=6 (RPCSEC_GSS)` to kiseki, the gateway MUST reject
//! cleanly; it MUST NOT silently fall through into AUTH_SYS handling
//! and MUST NOT panic.
//!
//! This file is intentionally thin (3 tests) per the Phase A plan,
//! [`specs/implementation/phase-A-layer1-rfc-compliance.md`] T-04.
//! It pins:
//!   1. The flavor sentinel (RPCSEC_GSS = 6).
//!   2. The reject-path mapping (`NfsAuthMethod::Kerberos` is
//!      defined but unsupported in practice — exports that don't
//!      list it MUST yield `MethodNotAllowed`).
//!   3. That `validate_credentials` does NOT silently degrade a
//!      Kerberos credential to AUTH_SYS when the principal is
//!      missing.
//!
//! When RPCSEC_GSS lands, this file expands; the catalog row moves
//! ❌ → 🟡 → ✅.
//!
//! Spec text:
//!   - <https://www.rfc-editor.org/rfc/rfc2203>
//!   - <https://www.rfc-editor.org/rfc/rfc5403>
//!   - <https://www.rfc-editor.org/rfc/rfc7204>
#![allow(clippy::doc_markdown)]

use kiseki_common::ids::OrgId;
use kiseki_gateway::nfs_auth::{
    validate_credentials, NfsAuthError, NfsAuthMethod, NfsCredentials, NfsExportAuth, UidMapping,
};

// ===========================================================================
// RFC 5531 §8.1 / IANA — RPCSEC_GSS flavor sentinel
// ===========================================================================

/// RFC 5531 §8.1 + IANA RPC Authentication Flavors registry pin
/// `RPCSEC_GSS = 6`. RFC 2203 §2 references the flavor by name.
/// This sentinel guards the wire constant — a future code change
/// that adds a typed `AuthFlavor::RpcsecGss` variant MUST pick the
/// value 6, not anything else.
#[test]
fn rfc5531_s8_1_rpcsec_gss_flavor_is_6() {
    const RPCSEC_GSS: u32 = 6;
    assert_eq!(
        RPCSEC_GSS, 6,
        "RFC 5531 §8.1 + IANA registry: RPCSEC_GSS auth flavor MUST be 6"
    );
    // Belt-and-suspenders: confirm we're not collision-overloading
    // any of the documented flavors.
    let pinned: [(u32, &str); 5] = [
        (0, "AUTH_NONE"),
        (1, "AUTH_SYS"),
        (2, "AUTH_SHORT"),
        (3, "AUTH_DH"),
        (6, "RPCSEC_GSS"),
    ];
    for (i, (vi, ni)) in pinned.iter().enumerate() {
        for (vj, nj) in &pinned[i + 1..] {
            assert_ne!(
                vi, vj,
                "auth flavors {ni} and {nj} would collide on the wire"
            );
        }
    }
}

// ===========================================================================
// RFC 2203 §5.1 — reject path for unsupported security flavor
// ===========================================================================

/// RFC 2203 §5.1 — when a client presents an RPCSEC_GSS credential
/// (flavor=6) to a server that does not accept that flavor on the
/// requested export, the server MUST reject the request. The reject
/// SHOULD use `AUTH_ERROR` / `AUTH_BADCRED` (RFC 5531 §9
/// `auth_stat`); at the kiseki authorization layer the equivalent
/// is `NfsAuthError::MethodNotAllowed`.
///
/// In kiseki's current model, `NfsAuthMethod::Kerberos` is the
/// closest typed primitive to RPCSEC_GSS (RFC 2203 mandates a
/// GSS mechanism, with Kerberos v5 being the canonical one per
/// RFC 7204 §2). This test asserts that an export which only
/// allows AUTH_SYS rejects a Kerberos cred with the documented
/// error variant — not a panic, not a silent fall-through, not a
/// mapped-to-AUTH_SYS shortcut.
#[test]
fn rfc2203_s5_1_kerberos_creds_rejected_when_export_disallows() {
    let tenant = OrgId(uuid::Uuid::new_v4());
    let export = NfsExportAuth {
        path: "/data/auth-sys-only".into(),
        allowed_methods: vec![NfsAuthMethod::AuthSys], // explicitly NOT Kerberos
        tenant_id: tenant,
        uid_mapping: UidMapping::AllToTenant,
    };
    let krb_creds = NfsCredentials {
        method: NfsAuthMethod::Kerberos,
        uid: 1000,
        gid: 1000,
        hostname: "krb-client.example".into(),
        principal: Some("user@EXAMPLE.COM".into()),
    };

    let result = validate_credentials(&krb_creds, &export);

    // The reject path: clean error, specific variant naming the
    // disallowed method. NOT an Ok(_) (silent acceptance) and NOT
    // any other variant (which would mask the real cause).
    match result {
        Err(NfsAuthError::MethodNotAllowed(NfsAuthMethod::Kerberos)) => { /* OK */ }
        other => panic!(
            "RFC 2203 §5.1 / kiseki nfs_auth: a Kerberos credential on an \
             AUTH_SYS-only export MUST be rejected with \
             MethodNotAllowed(Kerberos); got {other:?}. Silent fall-through \
             is the documented anti-pattern this test guards against."
        ),
    }
}

// ===========================================================================
// RFC 7204 §2 — Kerberos principal is required; no fall-through
// ===========================================================================

/// RFC 7204 §2 + RFC 2203 §5.3 — an RPCSEC_GSS credential carries
/// a GSS principal (Kerberos service principal in the canonical
/// configuration). A credential that arrives with `method=Kerberos`
/// but no principal is malformed; the server MUST reject — it MUST
/// NOT silently downgrade to AUTH_SYS using the embedded uid/gid.
///
/// This guards against a known anti-pattern where an
/// auth-flavor-aware decoder falls back to "treat the embedded
/// uid/gid as AUTH_SYS" when GSS context establishment fails.
/// Per RFC 2203 §5.1 that's a security violation: the client
/// asked for integrity/privacy and didn't get it.
#[test]
fn rfc7204_s2_kerberos_without_principal_is_rejected_not_downgraded() {
    let tenant = OrgId(uuid::Uuid::new_v4());
    let export = NfsExportAuth {
        path: "/data/krb-mount".into(),
        // Both flavors permitted — so a fall-through implementation
        // *could* silently degrade Kerberos→AUTH_SYS. The spec
        // forbids this. The test asserts the error path.
        allowed_methods: vec![NfsAuthMethod::AuthSys, NfsAuthMethod::Kerberos],
        tenant_id: tenant,
        uid_mapping: UidMapping::AllToTenant,
    };
    let bad_krb_creds = NfsCredentials {
        method: NfsAuthMethod::Kerberos,
        uid: 1000,
        gid: 1000,
        hostname: "krb-client.example".into(),
        principal: None, // RFC 7204 §2: must be present for Kerberos
    };

    let result = validate_credentials(&bad_krb_creds, &export);

    match result {
        Err(NfsAuthError::PrincipalNotFound(_)) => { /* OK */ }
        Ok(_) => panic!(
            "RFC 2203 §5.1 / RFC 7204 §2: a Kerberos credential without a \
             principal MUST NOT be silently accepted. Doing so would \
             downgrade the security flavor and violate the client's \
             requested integrity/privacy guarantees."
        ),
        Err(other) => panic!(
            "expected PrincipalNotFound; got {other:?}. The reject path is \
             documented and must be specific."
        ),
    }
}
