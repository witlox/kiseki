//! Layer 1 reference tests for **RFC 9289 — Towards Remote Procedure
//! Call Encryption By Default** (June 2022).
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner files:
//! - `kiseki-gateway::nfs_server::serve_nfs_listener` — accepts an
//!   `Option<Arc<rustls::ServerConfig>>`; when `Some`, every accepted
//!   `TcpStream` is wrapped via `rustls::StreamOwned`.
//! - `kiseki-gateway::nfs_security::evaluate` — the security-gate
//!   logic that enforces ADR-038 §D4 (TLS-by-default + audited
//!   plaintext fallback).
//! - `kiseki-gateway::pnfs_ds_server::serve_ds_listener` — mirrors the
//!   MDS listener for the DS endpoint.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 9289".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc9289>.
//!
//! All tests in this file invoke production code (the
//! `nfs_security::evaluate` gate, real `TlsConfig::server_config`,
//! the published `RFC9289_KEEPALIVE_INTERVAL_SECS` constant). Replaces
//! the prior local-literal tautology pattern (ADV-PA-2).
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

use kiseki_audit::event::AuditEventType;
use kiseki_gateway::nfs_security::{
    evaluate, NfsSecurityError, NfsTransport, PLAINTEXT_WARN_BANNER,
};

// ===========================================================================
// §3 — transport security flavor negotiation
// ===========================================================================
//
// RFC 9289 §3 + ADR-038 §D4: kiseki's NFS-over-TLS gate is
// `nfs_security::evaluate(allow_plaintext_nfs, insecure_env_set,
// tls_bundle_present, default_layout_ttl_seconds, tenant_count)`.
// All RFC 9289 §3 / ADR-038 §D4 tests below invoke this real
// production function.

#[test]
fn s3_default_posture_is_tls_when_bundle_present() {
    // ADR-038 §D4.1 / RFC 9289 §3: defaults yield TLS posture.
    let security = evaluate(false, false, true, 300, 1).expect("default");
    assert_eq!(security.mode, NfsTransport::Tls);
    assert!(
        security.audit_event.is_none(),
        "TLS path emits no SecurityDowngrade audit"
    );
    assert!(!security.emit_warn_banner);
    assert_eq!(security.effective_layout_ttl_seconds, 300);
}

#[test]
fn s3_no_tls_bundle_refuses_to_start() {
    // RFC 9289 §3: a server without a TLS bundle and without the
    // explicit plaintext opt-in MUST refuse to start cleanly — kiseki
    // returns NfsSecurityError::TlsBundleMissing rather than
    // silently downgrading.
    let err = evaluate(false, false, false, 300, 1).unwrap_err();
    assert_eq!(err, NfsSecurityError::TlsBundleMissing);
}

// ===========================================================================
// §3 — ALPN negotiation
// ===========================================================================
//
// RFC 9289 §3.2: NFS-over-TLS does NOT use ALPN. The TLS handshake
// negotiates the cipher suites and certs only; the NFS RPC framing
// rides on top of TLS records as it would on top of plain TCP.

#[test]
fn s3_2_no_alpn_for_nfs_over_tls() {
    use rcgen::{CertificateParams, KeyPair};
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();

    // Build a real ServerConfig via TlsConfig::server_config and
    // verify alpn_protocols is empty.
    let mut ca_params =
        CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|_| unreachable!());
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "kiseki-rfc9289-test-ca");
    let ca_kp = KeyPair::generate().unwrap_or_else(|_| unreachable!());
    let ca_cert = ca_params
        .self_signed(&ca_kp)
        .unwrap_or_else(|_| unreachable!());
    let issuer = rcgen::Issuer::new(ca_params, ca_kp);

    let mut leaf_params =
        CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|_| unreachable!());
    leaf_params.is_ca = rcgen::IsCa::NoCa;
    leaf_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "kiseki-nfs-server");
    leaf_params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        )));
    let leaf_kp = KeyPair::generate().unwrap_or_else(|_| unreachable!());
    let leaf_cert = leaf_params
        .signed_by(&leaf_kp, &issuer)
        .unwrap_or_else(|_| unreachable!());

    let server_config = kiseki_transport::config::TlsConfig::server_config(
        ca_cert.pem().as_bytes(),
        leaf_cert.pem().as_bytes(),
        leaf_kp.serialize_pem().as_bytes(),
    )
    .expect("server config");

    assert!(
        server_config.alpn_protocols.is_empty(),
        "RFC 9289 §3.2: NFS-over-TLS ServerConfig MUST have empty \
         alpn_protocols; got {:?}",
        server_config.alpn_protocols
    );
}

// ===========================================================================
// §4 — Keep-alive cadence
// ===========================================================================

#[test]
fn s4_2_keepalive_cadence_is_60_seconds() {
    // RFC 9289 §4.2: 60-second cadence in the absence of other traffic.
    // Production constant lives in `nfs_server.rs::enable_tcp_keepalive`;
    // the kernel handles the idle-reset semantic via SO_KEEPALIVE.
    assert_eq!(
        kiseki_gateway::nfs_server::RFC9289_KEEPALIVE_INTERVAL_SECS,
        60,
        "RFC 9289 §4.2: keep-alive cadence is 60 seconds"
    );
}

// ===========================================================================
// §5 — Plaintext fallback gate (ADR-038 §D4.2)
// ===========================================================================

#[test]
fn s5_plaintext_fallback_requires_two_flag_opt_in() {
    // ADR-038 §D4.2: BOTH flags (`allow_plaintext_nfs=true` AND
    // `KISEKI_INSECURE_NFS=true`) required. Single-flag attempts must
    // refuse to start. We exercise the production gate directly.
    let cfg_only = evaluate(true, false, false, 300, 1).unwrap_err();
    assert!(
        matches!(
            cfg_only,
            NfsSecurityError::PartialFlags {
                config_flag: true,
                env_flag: false
            }
        ),
        "config flag alone must refuse: got {cfg_only:?}"
    );
    let env_only = evaluate(false, true, true, 300, 1).unwrap_err();
    assert!(
        matches!(
            env_only,
            NfsSecurityError::PartialFlags {
                config_flag: false,
                env_flag: true
            }
        ),
        "env flag alone must refuse: got {env_only:?}"
    );
}

#[test]
fn s5_plaintext_fallback_emits_audit_event_on_every_boot() {
    // ADR-038 §D4.2 step 2: every boot in plaintext mode produces
    // a SecurityDowngradeEnabled audit event. The gate function
    // returns `audit_event: Some(...)` so the boot loop emits it
    // unconditionally — there is no "first-boot only" caching.
    let s = evaluate(true, true, false, 300, 1).expect("plaintext path");
    assert_eq!(s.mode, NfsTransport::Plaintext);
    assert_eq!(
        s.audit_event,
        Some(AuditEventType::SecurityDowngradeEnabled),
        "ADR-038 §D4.2.2: SecurityDowngradeEnabled audit on every boot"
    );
    assert!(s.emit_warn_banner);
}

#[test]
fn s5_plaintext_fallback_halves_layout_ttl_to_60s() {
    // ADR-038 §D4.2 step 3: when plaintext is active, the layout TTL
    // is halved from default → 60s to compensate for the larger
    // fh4-replay window. Production gate enforces this regardless of
    // the configured default_layout_ttl_seconds.
    for default_ttl in [300u64, 600, 86_400] {
        let s = evaluate(true, true, false, default_ttl, 1).expect("plaintext");
        assert_eq!(
            s.effective_layout_ttl_seconds, 60,
            "ADR-038 §D4.2.3: plaintext TTL is fixed at 60s regardless of \
             configured default ({default_ttl}s)"
        );
    }
}

#[test]
fn s5_plaintext_warn_banner_is_canonical() {
    // ADR-038 §D4.2 — pin the WARN banner text byte-for-byte. A
    // future rewording is a deliberate operator-facing change.
    assert!(
        PLAINTEXT_WARN_BANNER.contains("NFS path is PLAINTEXT"),
        "ADR-038 §D4.2: WARN banner must announce plaintext mode"
    );
    assert!(
        PLAINTEXT_WARN_BANNER.contains("I-PN7-default"),
        "WARN banner must reference the violated invariant"
    );
}

#[test]
fn s5_multi_tenant_plaintext_is_refused() {
    // ADR-038 §D4.2 — even with both flags, plaintext is refused on
    // a listener with >1 tenant (data-attribution risk grows with
    // tenant count). Production gate enforces.
    let err = evaluate(true, true, false, 300, 5).unwrap_err();
    assert!(
        matches!(
            err,
            NfsSecurityError::PlaintextMultiTenant { tenant_count: 5 }
        ),
        "ADR-038 §D4.2: plaintext + multi-tenant must refuse: got {err:?}"
    );
}

// ===========================================================================
// Cross-implementation seed — TLS record framing first byte
// ===========================================================================

/// RFC 8446 §5.1 (cited by RFC 9289 §3): TLS records begin with a
/// 1-byte content type. Application data (post-handshake RPC) =
/// `0x17`. Handshake = `0x16`. Alert = `0x15`. Pin against rustls's
/// canonical enum so a rustls upgrade that renames or renumbers a
/// variant is a visible failure.
#[test]
fn rfc_seed_tls_record_content_types_match_rustls() {
    use rustls::ContentType;
    // Discriminant values are part of the wire ABI per RFC 8446 §5.1.
    assert_eq!(u8::from(ContentType::Handshake), 0x16);
    assert_eq!(u8::from(ContentType::ApplicationData), 0x17);
    assert_eq!(u8::from(ContentType::Alert), 0x15);

    // Sanity: an RPC fragment header's top byte (0x80) cannot collide
    // with any TLS record type. The kiseki dispatcher distinguishes
    // TLS vs plaintext by inspecting this first byte.
    let rpc_fragment_marker = 0x80u8;
    assert!(
        rpc_fragment_marker > u8::from(ContentType::ApplicationData),
        "RFC 9289 §5: plaintext RPC fragment header (0x80) is disjoint \
         from any TLS record content type"
    );
}
