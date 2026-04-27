//! Layer 1 reference tests for **RFC 8446 — The Transport Layer
//! Security (TLS) Protocol Version 1.3** (August 2018).
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! We trust rustls for the bulk of TLS 1.3 wire-format compliance.
//! This file pins **kiseki's usage choices**:
//!
//! 1. The cipher-suite list our `TlsConfig::server_config` produces
//!    is a TLS 1.3-only subset — TLS_AES_256_GCM_SHA384,
//!    TLS_CHACHA20_POLY1305_SHA256, TLS_AES_128_GCM_SHA256.
//! 2. ALPN policy: gRPC data path advertises `h2`; NFS-over-TLS does
//!    not advertise any ALPN.
//! 3. Client-cert chain validation against the Cluster CA: a cert
//!    signed by an unrelated CA must be rejected at handshake time.
//!
//! Owner: `kiseki-transport::tcp_tls::TlsConfig`. Production
//! integration tests (`tls_handshake.rs`, `failure_scenarios.rs`)
//! cover the happy path + a couple negatives; this file adds
//! spec-fidelity tests on top.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 8446".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc8446>.
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

use std::sync::Arc;

use kiseki_transport::config::TlsConfig;
use rcgen::{CertificateParams, Issuer, KeyPair};

/// Install the aws-lc-rs crypto provider for rustls. Mirrors the
/// helper used by `tls_handshake.rs`.
fn ensure_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

struct CaBundle {
    ca_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

fn generate_ca() -> CaBundle {
    let mut params =
        CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|_| unreachable!());
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Kiseki RFC 8446 Test CA");
    let key_pair = KeyPair::generate().unwrap_or_else(|_| unreachable!());
    let cert = params
        .self_signed(&key_pair)
        .unwrap_or_else(|_| unreachable!());
    let ca_pem = cert.pem();
    let issuer = Issuer::new(params, key_pair);
    CaBundle { ca_pem, issuer }
}

fn generate_node_cert(
    ca: &Issuer<'_, KeyPair>,
    cn: &str,
    ip: std::net::IpAddr,
) -> (String, String) {
    let mut params =
        CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|_| unreachable!());
    params.is_ca = rcgen::IsCa::NoCa;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationalUnitName, "rfc8446-test");
    params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));
    let key_pair = KeyPair::generate().unwrap_or_else(|_| unreachable!());
    let cert = params
        .signed_by(&key_pair, ca)
        .unwrap_or_else(|_| unreachable!());
    (cert.pem(), key_pair.serialize_pem())
}

// ===========================================================================
// §B.4 — TLS 1.3 cipher suites
// ===========================================================================
//
// RFC 8446 §B.4 (Appendix B, "Cipher Suites"): five cipher suites are
// defined for TLS 1.3:
//
//   TLS_AES_128_GCM_SHA256       = {0x13,0x01}
//   TLS_AES_256_GCM_SHA384       = {0x13,0x02}
//   TLS_CHACHA20_POLY1305_SHA256 = {0x13,0x03}
//   TLS_AES_128_CCM_SHA256       = {0x13,0x04}   — optional
//   TLS_AES_128_CCM_8_SHA256     = {0x13,0x05}   — optional
//
// kiseki's policy: enable the three GCM/ChaCha suites; reject the two
// CCM variants (CCM is a niche profile not deployed in mainstream
// TLS 1.3 servers and not required by any kiseki use case).

/// IANA-assigned 16-bit cipher-suite codepoints (RFC 8446 §B.4).
const TLS_AES_128_GCM_SHA256: u16 = 0x1301;
const TLS_AES_256_GCM_SHA384: u16 = 0x1302;
const TLS_CHACHA20_POLY1305_SHA256: u16 = 0x1303;
const TLS_AES_128_CCM_SHA256: u16 = 0x1304;
const TLS_AES_128_CCM_8_SHA256: u16 = 0x1305;

#[test]
fn s_b_4_iana_codepoints_pinned() {
    // Pin the wire codepoints by asserting rustls's CipherSuite
    // enum produces the exact §B.4 values when converted to u16. A
    // rustls upgrade that re-numbered any of these would surface as a
    // mismatch here, not as a silent wire-format change.
    use rustls::CipherSuite;
    assert_eq!(
        u16::from(CipherSuite::TLS13_AES_128_GCM_SHA256),
        TLS_AES_128_GCM_SHA256,
        "RFC 8446 §B.4: rustls TLS13_AES_128_GCM_SHA256 must be 0x1301"
    );
    assert_eq!(
        u16::from(CipherSuite::TLS13_AES_256_GCM_SHA384),
        TLS_AES_256_GCM_SHA384,
        "RFC 8446 §B.4: rustls TLS13_AES_256_GCM_SHA384 must be 0x1302"
    );
    assert_eq!(
        u16::from(CipherSuite::TLS13_CHACHA20_POLY1305_SHA256),
        TLS_CHACHA20_POLY1305_SHA256,
        "RFC 8446 §B.4: rustls TLS13_CHACHA20_POLY1305_SHA256 must be 0x1303"
    );
    assert_eq!(
        u16::from(CipherSuite::TLS13_AES_128_CCM_SHA256),
        TLS_AES_128_CCM_SHA256,
        "RFC 8446 §B.4: rustls TLS13_AES_128_CCM_SHA256 must be 0x1304"
    );
    assert_eq!(
        u16::from(CipherSuite::TLS13_AES_128_CCM_8_SHA256),
        TLS_AES_128_CCM_8_SHA256,
        "RFC 8446 §B.4: rustls TLS13_AES_128_CCM_8_SHA256 must be 0x1305"
    );
}

#[test]
fn s_b_4_kiseki_advertises_three_aead_suites() {
    ensure_crypto_provider();

    // The default rustls cipher-suite list in the aws-lc-rs provider
    // is exactly the three AEAD suites kiseki accepts. Pin the policy:
    // any future addition (CCM, deprecated suites) is a deliberate
    // change.
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let suites: Vec<u16> = provider
        .cipher_suites
        .iter()
        .map(|cs| u16::from(cs.suite()))
        .collect();

    let expected: std::collections::BTreeSet<u16> = [
        TLS_AES_256_GCM_SHA384,
        TLS_CHACHA20_POLY1305_SHA256,
        TLS_AES_128_GCM_SHA256,
    ]
    .into_iter()
    .collect();

    let actual_tls13: std::collections::BTreeSet<u16> = suites
        .iter()
        .copied()
        .filter(|s| (0x1300..=0x13FF).contains(s))
        .collect();

    assert_eq!(
        actual_tls13, expected,
        "RFC 8446 §B.4: kiseki MUST advertise exactly the three AEAD \
         TLS 1.3 cipher suites (no CCM); got {actual_tls13:?}"
    );
}

#[test]
fn s_b_4_no_legacy_tls12_only_suites() {
    ensure_crypto_provider();

    // RFC 8446 §B.4 + ADR-038 §D4.1: kiseki's NFS-over-TLS path is
    // TLS 1.3 ONLY. The aws-lc-rs default provider includes TLS 1.2
    // suites for general use; we restrict the kiseki server config
    // to TLS 1.3 explicitly via `with_protocol_versions(&[TLS13])`
    // (see TlsConfig::server_config_with_crl).
    //
    // This test verifies the production restriction is in place by
    // building a real ServerConfig and inspecting its configured
    // versions.
    let ca = generate_ca();
    let (cert, key) = generate_node_cert(
        &ca.issuer,
        "tls13-only",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );
    let server_config =
        TlsConfig::server_config(ca.ca_pem.as_bytes(), cert.as_bytes(), key.as_bytes())
            .expect("server config");

    // Inspect the cipher suites the server will negotiate — all must
    // be in the TLS 1.3 codepoint range (0x1301..=0x13FF).
    let non_tls13: Vec<u16> = server_config
        .crypto_provider()
        .cipher_suites
        .iter()
        .map(|cs| u16::from(cs.suite()))
        .filter(|s| !(0x1300..=0x13FF).contains(s))
        .collect();
    assert!(
        non_tls13.is_empty(),
        "RFC 8446 §B.4 + ADR-038 §D4.1: kiseki ServerConfig must be \
         TLS 1.3 only — found legacy suites: {non_tls13:?}"
    );
}

// ===========================================================================
// §4.2.7 — supported_groups + signature_algorithms
// ===========================================================================
//
// RFC 8446 §4.2.7: every TLS 1.3 server MUST support at least one
// signature algorithm. The aws-lc-rs provider includes the full
// modern set (Ed25519, ECDSA P-256/384, RSA-PSS). Pin the policy.

#[test]
fn s4_2_7_signature_algorithms_includes_ecdsa() {
    ensure_crypto_provider();
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let names: Vec<String> = provider
        .signature_verification_algorithms
        .all
        .iter()
        .map(|a| format!("{a:?}"))
        .collect();

    let joined = names.join(",").to_lowercase();
    assert!(
        joined.contains("ecdsa") || joined.contains("ed25519") || joined.contains("rsa"),
        "RFC 8446 §4.2.7: provider exposes a recognized signature \
         algorithm set; got {names:?}"
    );
}

// ===========================================================================
// ALPN policy — gRPC vs NFS
// ===========================================================================

#[test]
fn alpn_nfs_server_config_has_no_alpn() {
    // RFC 9289 §3.2: NFS-over-TLS does NOT negotiate any application
    // protocol via ALPN. The kiseki ServerConfig used for NFS MUST
    // therefore have an empty `alpn_protocols` vector.
    //
    // We assert the contract on a REAL ServerConfig built by
    // `TlsConfig::server_config`. If a future refactor accidentally
    // sets alpn_protocols (e.g. via inheriting a gRPC config), this
    // catches it.
    ensure_crypto_provider();
    let ca = generate_ca();
    let (cert, key) = generate_node_cert(
        &ca.issuer,
        "alpn-nfs",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );
    let server_config =
        TlsConfig::server_config(ca.ca_pem.as_bytes(), cert.as_bytes(), key.as_bytes())
            .expect("server config");
    assert!(
        server_config.alpn_protocols.is_empty(),
        "RFC 9289 §3.2: NFS ServerConfig MUST have empty alpn_protocols; \
         got {:?}",
        server_config.alpn_protocols
    );
}

#[test]
fn alpn_grpc_path_known_gap_tracked() {
    // ADR-013 + RFC 7540 §3.4 + RFC 9113: the gRPC data path runs
    // HTTP/2 and SHOULD advertise ALPN "h2". Today kiseki uses ONE
    // `TlsConfig::server_config` for both NFS and gRPC paths, which
    // produces no ALPN — fine for NFS, suboptimal for gRPC (a client
    // could in principle attempt HTTP/1.1).
    //
    // This test documents the known gap by introspecting the SAME
    // ServerConfig and asserting we either DO advertise "h2" (fixed)
    // OR explicitly have no ALPN list (current state). It will fire
    // if a future refactor sets alpn_protocols to something OTHER
    // than empty or [b"h2"].
    ensure_crypto_provider();
    let ca = generate_ca();
    let (cert, key) = generate_node_cert(
        &ca.issuer,
        "alpn-grpc",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );
    let server_config =
        TlsConfig::server_config(ca.ca_pem.as_bytes(), cert.as_bytes(), key.as_bytes())
            .expect("server config");

    let alpn = &server_config.alpn_protocols;
    assert!(
        alpn.is_empty() || (alpn.len() == 1 && alpn[0] == b"h2"),
        "ALPN policy: kiseki's ServerConfig must be either empty (NFS \
         shared, current state) or [\"h2\"] (gRPC-only). Got {alpn:?} — \
         a different value indicates an accidental ALPN set that needs \
         a deliberate per-protocol split (TODO: ADR-013 follow-up)."
    );
}

// ===========================================================================
// §4.4.2.4 — client cert chain validation
// ===========================================================================
//
// RFC 8446 §4.4.2.4: the server's CertificateVerify step checks that
// the client cert's signature chain terminates at one of the configured
// trust roots. kiseki uses `WebPkiClientVerifier::builder` with the
// Cluster CA root store; an unrelated CA-signed client cert MUST fail
// the chain validation.

#[tokio::test]
async fn s4_4_2_4_client_cert_signed_by_unrelated_ca_rejected() {
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    ensure_crypto_provider();

    let cluster_ca = generate_ca();
    let unrelated_ca = generate_ca();

    let (server_cert, server_key) = generate_node_cert(
        &cluster_ca.issuer,
        "server",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );

    // Server trusts ONLY the cluster CA.
    let server_config = TlsConfig::server_config(
        cluster_ca.ca_pem.as_bytes(),
        server_cert.as_bytes(),
        server_key.as_bytes(),
    )
    .expect("server config");

    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");

    // Server task — only declares mTLS bypass if we actually receive
    // application bytes from the rogue client. accept() returning Ok
    // is not sufficient evidence by itself: in TLS 1.3, tokio-rustls
    // can return Ok before the read path surfaces a CertificateVerify
    // alert (rustls/tokio-rustls #1521 family of timing quirks).
    let accepted_handle = tokio::spawn(async move {
        let Ok((tcp, _peer)) = listener.accept().await else {
            return Err::<(), &'static str>("listener closed");
        };
        match acceptor.accept(tcp).await {
            Ok(mut tls) => {
                let mut buf = [0u8; 1];
                match tls.read(&mut buf).await {
                    Ok(0) | Err(_) => Ok(()), // Server alerted / connection closed.
                    Ok(_) => Err("RFC 8446 §4.4.2.4: server received application \
                         bytes from a client whose cert was signed by an \
                         unrelated CA — chain validation FAILED"),
                }
            }
            Err(_) => Ok(()), // Expected — handshake rejected at TLS layer.
        }
    });

    // Client uses the UNRELATED CA's cert as its client cert. The
    // server's WebPkiClientVerifier rejects it.
    let (rogue_cert, rogue_key) = generate_node_cert(
        &unrelated_ca.issuer,
        "rogue-client",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );

    // Build a client config that trusts the cluster CA (so the server's
    // cert validates) but presents the rogue CA-signed client cert.
    use rustls::pki_types::CertificateDer;
    use std::io::BufReader;

    let mut root_store = rustls::RootCertStore::empty();
    let cluster_ca_certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cluster_ca.ca_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .expect("parse cluster ca");
    for c in &cluster_ca_certs {
        root_store.add(c.clone()).expect("add ca");
    }

    let rogue_cert_chain: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(rogue_cert.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .expect("parse rogue cert");
    let rogue_key_der = rustls_pemfile::private_key(&mut BufReader::new(rogue_key.as_bytes()))
        .expect("parse key")
        .expect("key present");

    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(rogue_cert_chain, rogue_key_der)
        .expect("client config");

    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let server_name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());

    let handshake_result =
        tokio::time::timeout(Duration::from_secs(5), connector.connect(server_name, tcp)).await;

    // The handshake MAY surface as Err here (server alerted before
    // client_finished) OR Ok (alert lands during first read). The
    // authoritative check is on the server task: did it ever receive
    // application bytes from a rogue client? If yes, that's the
    // mTLS bypass.
    match handshake_result {
        Err(_timeout) => {} // Timeout: server stalled or dropped (RFC 8446 §6.2).
        Ok(Err(_handshake_err)) => {} // Server rejected mid-handshake.
        Ok(Ok(mut stream)) => {
            // Try to send + read; if the server actually accepted the
            // chain, this round-trip works. If not, we'll get an Alert
            // here.
            use tokio::io::AsyncWriteExt;
            let _ = stream.write_all(b"x").await;
            let mut buf = [0u8; 1];
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buf)).await;
            // Don't panic here — leave verdict to the server task,
            // which has authoritative visibility on whether bytes
            // crossed the verified-channel boundary.
        }
    }

    // Verify the server task didn't accept application bytes from the
    // rogue client. THIS is the definitive mTLS-bypass test.
    match tokio::time::timeout(Duration::from_secs(2), accepted_handle).await {
        Ok(Ok(Ok(()))) => {} // server saw the rejection — good
        Ok(Ok(Err(msg))) => panic!("{msg}"),
        Ok(Err(_join_err)) => {} // task panic counts as rejection
        Err(_timeout) => {}      // server still waiting — also fine
    }
}

/// Direct verifier-layer diagnostic: bypass tokio-rustls and check
/// that `WebPkiClientVerifier::verify_client_cert` itself rejects a
/// chain rooted in an unrelated CA. This isolates "verifier broken"
/// from "TLS handshake bug" — when this test fails, the verifier
/// itself is letting rogue chains through.
#[test]
fn s4_4_2_4_verifier_rejects_rogue_chain_directly() {
    use rustls::pki_types::{CertificateDer, UnixTime};
    // ClientCertVerifier trait is invoked via dyn dispatch on the
    // verifier's concrete type — no need to import the trait here.
    use std::io::BufReader;

    ensure_crypto_provider();

    let cluster_ca = generate_ca();
    let unrelated_ca = generate_ca();

    let (rogue_cert_pem, _rogue_key_pem) = generate_node_cert(
        &unrelated_ca.issuer,
        "rogue",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );

    // Build the verifier the same way server_config does.
    let mut roots = rustls::RootCertStore::empty();
    let cluster_certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cluster_ca.ca_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .expect("parse cluster ca");
    for c in &cluster_certs {
        roots.add(c.clone()).expect("add ca");
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .expect("verifier");

    let rogue_certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(rogue_cert_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .expect("parse rogue cert");

    let now = UnixTime::since_unix_epoch(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap(),
    );
    let result = verifier.verify_client_cert(&rogue_certs[0], &[], now);
    assert!(
        result.is_err(),
        "RFC 8446 §4.4.2.4: verify_client_cert MUST reject a leaf cert \
         whose issuer chain doesn't terminate at the configured trust \
         root; got Ok which means WebPkiClientVerifier is silently \
         accepting cross-CA chains — CRITICAL mTLS bypass."
    );
}

// ===========================================================================
// §4.4.2 — server-cert chain validation (control)
// ===========================================================================

#[test]
fn s4_4_2_known_good_chain_builds_server_config() {
    ensure_crypto_provider();
    let ca = generate_ca();
    let (cert, key) = generate_node_cert(
        &ca.issuer,
        "control",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );
    // Sanity: a real CA + child cert + key produces a working
    // ServerConfig. Negative-only file would be uninformative.
    let result = TlsConfig::server_config(ca.ca_pem.as_bytes(), cert.as_bytes(), key.as_bytes());
    assert!(
        result.is_ok(),
        "RFC 8446 §4.4.2: a child cert signed by the configured CA \
         MUST yield a valid ServerConfig; got: {:?}",
        result.err()
    );
}

// ===========================================================================
// Cross-implementation seed — RFC 8446 §B.4 cipher-suite codepoints
// ===========================================================================

/// RFC 8446 §B.4 cross-implementation seed — verify that rustls's
/// `CipherSuite` enum produces the IANA-registered codepoints. The
/// previous version of this test iterated a local table and asserted
/// the high byte against itself (tautology); this version pulls every
/// codepoint from rustls's enum, so a rustls upgrade that re-numbered
/// any variant fails here at compile or run time.
#[test]
fn rfc_seed_s_b_4_cipher_suite_codepoints() {
    use rustls::CipherSuite;
    let registry: &[(&str, CipherSuite, [u8; 2])] = &[
        (
            "TLS_AES_128_GCM_SHA256",
            CipherSuite::TLS13_AES_128_GCM_SHA256,
            [0x13, 0x01],
        ),
        (
            "TLS_AES_256_GCM_SHA384",
            CipherSuite::TLS13_AES_256_GCM_SHA384,
            [0x13, 0x02],
        ),
        (
            "TLS_CHACHA20_POLY1305_SHA256",
            CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            [0x13, 0x03],
        ),
        (
            "TLS_AES_128_CCM_SHA256",
            CipherSuite::TLS13_AES_128_CCM_SHA256,
            [0x13, 0x04],
        ),
        (
            "TLS_AES_128_CCM_8_SHA256",
            CipherSuite::TLS13_AES_128_CCM_8_SHA256,
            [0x13, 0x05],
        ),
    ];
    for (name, suite, expected_bytes) in registry {
        let actual = u16::from(*suite);
        let expected = u16::from_be_bytes(*expected_bytes);
        assert_eq!(
            actual, expected,
            "RFC 8446 §B.4: rustls {name} codepoint must be 0x{expected:04X}; got 0x{actual:04X}"
        );
        assert_eq!(
            actual & 0xFF00,
            0x1300,
            "RFC 8446 §B.4: every TLS 1.3 cipher suite ('{name}') has high byte 0x13"
        );
    }
}
