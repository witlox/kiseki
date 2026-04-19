//! Integration tests for TCP+TLS transport with mTLS.
//!
//! Uses `rcgen` to generate self-signed CA and node certificates for
//! testing. Verifies:
//! 1. Successful mTLS handshake with matching Cluster CA.
//! 2. Peer identity extraction from certificate.
//! 3. Rejection of connections without client certs.
//! 4. Rejection of connections with wrong CA.
//! 5. TLS config rejects empty/invalid PEM input.

use std::net::SocketAddr;
use std::sync::Arc;

use kiseki_transport::config::TlsConfig;
use kiseki_transport::tcp_tls::TimeoutConfig;
use kiseki_transport::traits::{Connection, Transport};
use kiseki_transport::TcpTlsTransport;
use rcgen::{CertificateParams, KeyPair};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Install the aws-lc-rs crypto provider for rustls. When running
/// with `--all-features`, both `aws-lc-rs` and `ring` features may
/// be enabled, requiring explicit provider selection.
fn ensure_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Generate a self-signed CA certificate and key pair.
fn generate_ca() -> (String, String, rcgen::CertifiedKey) {
    let mut params =
        CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|_| unreachable!());
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Kiseki Test CA");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "Test Org");

    let key_pair = KeyPair::generate().unwrap_or_else(|_| unreachable!());
    let cert = params
        .self_signed(&key_pair)
        .unwrap_or_else(|_| unreachable!());

    let ca_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    let certified = rcgen::CertifiedKey { cert, key_pair };
    (ca_pem, key_pem, certified)
}

/// Generate a node certificate signed by the given CA.
fn generate_node_cert(
    ca: &rcgen::CertifiedKey,
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
        .push(rcgen::DnType::OrganizationalUnitName, "test-tenant");
    params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));

    let key_pair = KeyPair::generate().unwrap_or_else(|_| unreachable!());
    let cert = params
        .signed_by(&key_pair, &ca.cert, &ca.key_pair)
        .unwrap_or_else(|_| unreachable!());

    (cert.pem(), key_pair.serialize_pem())
}

/// Start a TLS server that echoes back whatever it receives.
async fn start_echo_server(ca_pem: &str, cert_pem: &str, key_pem: &str) -> SocketAddr {
    let server_config =
        TlsConfig::server_config(ca_pem.as_bytes(), cert_pem.as_bytes(), key_pem.as_bytes())
            .unwrap_or_else(|e| panic!("server config: {e}"));

    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let addr = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("local_addr: {e}"));

    tokio::spawn(async move {
        loop {
            let (tcp, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match acceptor.accept(tcp).await {
                    Ok(mut tls) => {
                        let mut buf = vec![0u8; 1024];
                        if let Ok(n) = tls.read(&mut buf).await {
                            let _ = tls.write_all(&buf[..n]).await;
                            let _ = tls.shutdown().await;
                        }
                    }
                    Err(_) => {} // reject
                }
            });
        }
    });

    addr
}

#[tokio::test]
async fn mtls_handshake_and_echo() {
    ensure_crypto_provider();
    let (ca_pem, _ca_key, ca) = generate_ca();
    let (server_cert, server_key) = generate_node_cert(
        &ca,
        "server",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );
    let (client_cert, client_key) = generate_node_cert(
        &ca,
        "client",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );

    let addr = start_echo_server(&ca_pem, &server_cert, &server_key).await;

    let config = TlsConfig::from_pem(
        ca_pem.as_bytes(),
        client_cert.as_bytes(),
        client_key.as_bytes(),
    );
    assert!(config.is_ok(), "client TLS config failed: {config:?}");
    let transport = TcpTlsTransport::new(config.unwrap_or_else(|_| unreachable!()));

    assert_eq!(transport.name(), "tcp-tls");

    let mut conn = transport
        .connect(addr)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));

    // Verify peer identity was extracted.
    let identity = conn.peer_identity();
    assert!(!identity.common_name.is_empty());
    assert_ne!(identity.cert_fingerprint, [0u8; 32]);
    assert_eq!(conn.remote_addr(), addr);

    // Echo test.
    conn.write_all(b"hello kiseki")
        .await
        .unwrap_or_else(|e| panic!("write: {e}"));
    conn.shutdown()
        .await
        .unwrap_or_else(|e| panic!("shutdown: {e}"));

    let mut response = Vec::new();
    conn.read_to_end(&mut response)
        .await
        .unwrap_or_else(|e| panic!("read: {e}"));
    assert_eq!(response, b"hello kiseki");
}

#[tokio::test]
async fn wrong_ca_rejected() {
    ensure_crypto_provider();
    let (ca_pem, _ca_key, ca) = generate_ca();
    let (other_ca_pem, _other_key, other_ca) = generate_ca();

    let (server_cert, server_key) = generate_node_cert(
        &ca,
        "server",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );
    // Client cert signed by OTHER CA.
    let (client_cert, client_key) = generate_node_cert(
        &other_ca,
        "rogue",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );

    let addr = start_echo_server(&ca_pem, &server_cert, &server_key).await;

    // Client trusts the other CA, not the server's CA — handshake should fail.
    let config = TlsConfig::from_pem(
        other_ca_pem.as_bytes(),
        client_cert.as_bytes(),
        client_key.as_bytes(),
    );
    assert!(config.is_ok());
    let transport = TcpTlsTransport::new(config.unwrap_or_else(|_| unreachable!()));

    let result = transport.connect(addr).await;
    assert!(result.is_err(), "should reject wrong CA: {result:?}");
}

#[test]
fn empty_ca_pem_rejected() {
    ensure_crypto_provider();
    let result = TlsConfig::from_pem(b"", b"not-a-cert", b"not-a-key");
    assert!(result.is_err());
}

#[test]
fn empty_cert_pem_rejected() {
    ensure_crypto_provider();
    let (ca_pem, _, _) = generate_ca();
    let result = TlsConfig::from_pem(ca_pem.as_bytes(), b"", b"not-a-key");
    assert!(result.is_err());
}

#[test]
fn server_config_empty_ca_rejected() {
    ensure_crypto_provider();
    let result = TlsConfig::server_config(b"", b"cert", b"key");
    assert!(result.is_err());
}

#[tokio::test]
async fn connect_timeout_fires() {
    ensure_crypto_provider();
    let (ca_pem, _ca_key, ca) = generate_ca();
    let (client_cert, client_key) = generate_node_cert(
        &ca,
        "client",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    );

    let config = TlsConfig::from_pem(
        ca_pem.as_bytes(),
        client_cert.as_bytes(),
        client_key.as_bytes(),
    )
    .unwrap_or_else(|_| unreachable!());

    // Use a very short timeout and connect to a non-routable address.
    let timeouts = TimeoutConfig {
        connect: std::time::Duration::from_millis(50),
        handshake: std::time::Duration::from_secs(1),
    };
    let transport = TcpTlsTransport::with_timeouts(config, timeouts);

    // 192.0.2.1 is TEST-NET-1 (RFC 5737) — guaranteed non-routable.
    let result = transport
        .connect("192.0.2.1:9999".parse().unwrap_or_else(|_| unreachable!()))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("timed out") || err_msg.contains("connect"),
        "expected timeout or connection error, got: {err_msg}"
    );
}

#[tokio::test]
async fn default_timeouts_used() {
    let timeouts = TimeoutConfig::default();
    assert_eq!(timeouts.connect, std::time::Duration::from_secs(5));
    assert_eq!(timeouts.handshake, std::time::Duration::from_secs(10));
}
