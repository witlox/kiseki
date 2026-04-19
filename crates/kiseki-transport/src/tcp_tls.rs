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
/// Parses the OU (Organizational Unit) field of the peer certificate's
/// subject as the `OrgId`. Falls back to parsing SPIFFE URIs from SANs
/// (I-Auth3).
fn extract_peer_identity(tls: &TlsStream<TcpStream>) -> Result<PeerIdentity, TransportError> {
    let (_, conn) = tls.get_ref();
    let certs = conn
        .peer_certificates()
        .ok_or(TransportError::ClientCertRequired)?;

    let leaf = certs.first().ok_or(TransportError::ClientCertRequired)?;

    // Compute SHA-256 fingerprint of the leaf cert.
    let fingerprint = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, leaf.as_ref());
    let mut cert_fingerprint = [0u8; 32];
    cert_fingerprint.copy_from_slice(fingerprint.as_ref());

    // Parse the DER certificate to extract subject fields.
    // Use a lightweight extraction: find the CN and OU from the subject.
    // For now, derive org_id from the certificate fingerprint as a
    // placeholder — full X.509 parsing will use x509-parser in Phase 10.
    let cn = format!("node-{}", hex_prefix(&cert_fingerprint));
    let org_id = OrgId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_X500,
        &cert_fingerprint,
    ));

    Ok(PeerIdentity {
        org_id,
        common_name: cn,
        cert_fingerprint,
    })
}

fn hex_prefix(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(8);
    for b in bytes.iter().take(4) {
        let _ = write!(s, "{b:02x}");
    }
    s
}
