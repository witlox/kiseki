//! TCP+TLS transport with mTLS (I-Auth1, I-K13).
//!
//! Reference transport implementation. Uses `rustls` (with `aws-lc-rs`
//! crypto backend) for TLS 1.3/1.2. Requires both client and server to
//! present certificates signed by the Cluster CA.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use kiseki_common::ids::OrgId;

use crate::config::TlsConfig;
use crate::error::TransportError;
use crate::traits::{Connection, PeerIdentity, Transport};

/// Timeout configuration for transport connections.
#[derive(Clone, Copy, Debug)]
pub struct TimeoutConfig {
    /// TCP connection timeout. Default: 5 seconds.
    pub connect: Duration,
    /// TLS handshake timeout. Default: 10 seconds.
    pub handshake: Duration,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            handshake: Duration::from_secs(10),
        }
    }
}

/// TCP+TLS transport with mutual TLS authentication.
#[derive(Debug, Clone)]
pub struct TcpTlsTransport {
    config: TlsConfig,
    timeouts: TimeoutConfig,
}

impl TcpTlsTransport {
    /// Create a new TCP+TLS transport with default timeouts.
    #[must_use]
    pub fn new(config: TlsConfig) -> Self {
        Self {
            config,
            timeouts: TimeoutConfig::default(),
        }
    }

    /// Create with explicit timeout configuration.
    #[must_use]
    pub fn with_timeouts(config: TlsConfig, timeouts: TimeoutConfig) -> Self {
        Self { config, timeouts }
    }
}

impl Transport for TcpTlsTransport {
    type Conn = TcpTlsConnection;

    async fn connect(&self, addr: SocketAddr) -> Result<TcpTlsConnection, TransportError> {
        // TCP connection with timeout.
        let tcp = tokio::time::timeout(self.timeouts.connect, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                TransportError::Timeout(format!(
                    "TCP connect to {addr} timed out after {:?}",
                    self.timeouts.connect
                ))
            })?
            .map_err(|e| TransportError::ConnectionFailed(format!("{addr}: {e}")))?;

        // TLS handshake with timeout.
        let server_name = ServerName::IpAddress(addr.ip().into());

        let tls = tokio::time::timeout(
            self.timeouts.handshake,
            self.config.connector().connect(server_name, tcp),
        )
        .await
        .map_err(|_| {
            TransportError::Timeout(format!(
                "TLS handshake with {addr} timed out after {:?}",
                self.timeouts.handshake
            ))
        })?
        .map_err(|e| TransportError::TlsHandshakeFailed(e.to_string()))?;

        // Extract peer identity from the server's certificate.
        let identity = extract_peer_identity(&tls)?;

        Ok(TcpTlsConnection {
            stream: tls,
            identity,
            remote: addr,
        })
    }

    fn name(&self) -> &'static str {
        "tcp-tls"
    }
}

/// An authenticated TCP+TLS connection.
pub struct TcpTlsConnection {
    stream: TlsStream<TcpStream>,
    identity: PeerIdentity,
    remote: SocketAddr,
}

impl Connection for TcpTlsConnection {
    fn peer_identity(&self) -> &PeerIdentity {
        &self.identity
    }

    fn remote_addr(&self) -> SocketAddr {
        self.remote
    }
}

impl AsyncRead for TcpTlsConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TcpTlsConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl std::fmt::Debug for TcpTlsConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpTlsConnection")
            .field("remote", &self.remote)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// Extract tenant identity from the peer's TLS certificate.
///
/// Parses the X.509 leaf certificate using `x509-parser`:
/// 1. OU (Organizational Unit) from the subject → `OrgId`
/// 2. Fallback: SPIFFE URI from SANs (`spiffe://cluster/org/<id>`) → `OrgId` (I-Auth3)
/// 3. CN (Common Name) from the subject → `common_name`
/// 4. SHA-256 fingerprint of the DER-encoded leaf cert
fn extract_peer_identity(tls: &TlsStream<TcpStream>) -> Result<PeerIdentity, TransportError> {
    let (_, conn) = tls.get_ref();
    let certs = conn
        .peer_certificates()
        .ok_or(TransportError::ClientCertRequired)?;

    let leaf_der = certs.first().ok_or(TransportError::ClientCertRequired)?;

    // SHA-256 fingerprint.
    let fingerprint = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, leaf_der.as_ref());
    let mut cert_fingerprint = [0u8; 32];
    cert_fingerprint.copy_from_slice(fingerprint.as_ref());

    // Parse X.509.
    let (_, cert) = x509_parser::parse_x509_certificate(leaf_der.as_ref())
        .map_err(|e| TransportError::CertNotTrusted(format!("X.509 parse failed: {e}")))?;

    // Extract CN from subject.
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
        .unwrap_or("unknown")
        .to_owned();

    // Extract OrgId: try OU first, then SPIFFE SAN.
    let org_id = extract_org_from_ou(&cert)
        .or_else(|| extract_org_from_spiffe_san(&cert))
        .unwrap_or_else(|| {
            // Final fallback: derive from fingerprint (for certs without OU/SPIFFE).
            OrgId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_X500,
                &cert_fingerprint,
            ))
        });

    Ok(PeerIdentity {
        org_id,
        common_name: cn,
        cert_fingerprint,
    })
}

/// Extract `OrgId` from the OU (Organizational Unit) field.
fn extract_org_from_ou(cert: &x509_parser::certificate::X509Certificate<'_>) -> Option<OrgId> {
    let ou_str = cert
        .subject()
        .iter_organizational_unit()
        .next()?
        .as_str()
        .ok()?;

    // Try parsing as UUID first; fall back to UUID v5 derivation.
    if let Ok(uuid) = uuid::Uuid::parse_str(ou_str) {
        Some(OrgId(uuid))
    } else {
        Some(OrgId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_X500,
            ou_str.as_bytes(),
        )))
    }
}

/// Extract `OrgId` from a SPIFFE SAN URI: `spiffe://cluster/org/<id>` (I-Auth3).
fn extract_org_from_spiffe_san(
    cert: &x509_parser::certificate::X509Certificate<'_>,
) -> Option<OrgId> {
    use x509_parser::extensions::GeneralName;

    let san_ext = cert
        .extensions()
        .iter()
        .find(|ext| ext.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)?;

    let parsed = san_ext.parsed_extension();
    if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) = parsed {
        for name in &san.general_names {
            if let GeneralName::URI(uri) = name {
                if let Some(org_str) = uri.strip_prefix("spiffe://cluster/org/") {
                    if let Ok(uuid) = uuid::Uuid::parse_str(org_str) {
                        return Some(OrgId(uuid));
                    }
                    return Some(OrgId(uuid::Uuid::new_v5(
                        &uuid::Uuid::NAMESPACE_X500,
                        org_str.as_bytes(),
                    )));
                }
            }
        }
    }

    None
}
