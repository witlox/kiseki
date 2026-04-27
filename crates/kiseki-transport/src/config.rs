//! TLS configuration for mTLS with Cluster CA validation.
//!
//! Spec: I-Auth1, I-K13.

use std::io::BufReader;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsConnector;

use crate::error::TransportError;

/// TLS configuration for a Kiseki node (client or server role).
///
/// Holds the Cluster CA trust root, the node's own certificate chain,
/// and private key. Both client and server sides require mTLS — the
/// server demands a client cert, and the client validates the server
/// against the Cluster CA.
#[derive(Clone)]
pub struct TlsConfig {
    /// TLS connector for outbound connections.
    connector: TlsConnector,
    /// Cluster CA root certificates (for verification).
    ca_certs: Vec<CertificateDer<'static>>,
}

impl TlsConfig {
    /// Build a TLS configuration from PEM-encoded materials.
    ///
    /// - `ca_pem`: Cluster CA certificate(s) in PEM format.
    /// - `cert_pem`: This node's certificate chain in PEM format.
    /// - `key_pem`: This node's private key in PEM format.
    pub fn from_pem(
        ca_pem: &[u8],
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> Result<Self, TransportError> {
        // Parse CA certs.
        let ca_certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(ca_pem))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| TransportError::ConfigError(format!("CA PEM parse: {e}")))?;

        if ca_certs.is_empty() {
            return Err(TransportError::ConfigError(
                "no CA certificates found".into(),
            ));
        }

        // Build root cert store from Cluster CA.
        let mut root_store = rustls::RootCertStore::empty();
        for cert in &ca_certs {
            root_store
                .add(cert.clone())
                .map_err(|e| TransportError::ConfigError(format!("CA add: {e}")))?;
        }

        // Parse node cert chain.
        let node_certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(cert_pem))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| TransportError::ConfigError(format!("cert PEM parse: {e}")))?;

        if node_certs.is_empty() {
            return Err(TransportError::ConfigError(
                "no node certificates found".into(),
            ));
        }

        // Parse private key.
        let key = rustls_pemfile::private_key(&mut BufReader::new(key_pem))
            .map_err(|e| TransportError::ConfigError(format!("key PEM parse: {e}")))?
            .ok_or_else(|| TransportError::ConfigError("no private key found".into()))?;

        // Build client config with mTLS.
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(node_certs, key)
            .map_err(|e| TransportError::ConfigError(format!("client config: {e}")))?;

        Ok(Self {
            connector: TlsConnector::from(Arc::new(client_config)),
            ca_certs,
        })
    }

    /// Build a server TLS configuration that requires client certificates.
    ///
    /// Optionally accepts CRL PEM data for certificate revocation checking.
    /// If `crl_pem` is `Some`, revoked client certificates will be rejected
    /// at TLS handshake time.
    ///
    /// Returns a `rustls::ServerConfig` for use in listeners.
    pub fn server_config(
        ca_pem: &[u8],
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> Result<rustls::ServerConfig, TransportError> {
        Self::server_config_with_crl(ca_pem, cert_pem, key_pem, None)
    }

    /// Build a server TLS configuration with optional CRL checking.
    pub fn server_config_with_crl(
        ca_pem: &[u8],
        cert_pem: &[u8],
        key_pem: &[u8],
        crl_pem: Option<&[u8]>,
    ) -> Result<rustls::ServerConfig, TransportError> {
        use rustls::pki_types::CertificateRevocationListDer;

        // Parse CA for client cert verification.
        let ca_certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(ca_pem))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| TransportError::ConfigError(format!("CA PEM parse: {e}")))?;

        let mut root_store = rustls::RootCertStore::empty();
        for cert in &ca_certs {
            root_store
                .add(cert.clone())
                .map_err(|e| TransportError::ConfigError(format!("CA add: {e}")))?;
        }

        let mut verifier_builder =
            rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store));

        // Add CRLs if provided.
        if let Some(crl_data) = crl_pem {
            let crls: Vec<CertificateRevocationListDer<'static>> =
                rustls_pemfile::crls(&mut BufReader::new(crl_data))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| TransportError::ConfigError(format!("CRL PEM parse: {e}")))?;
            if !crls.is_empty() {
                verifier_builder = verifier_builder.with_crls(crls);
            }
        }

        let client_verifier = verifier_builder
            .build()
            .map_err(|e| TransportError::ConfigError(format!("client verifier: {e}")))?;

        // Parse server cert chain and key.
        let server_certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(cert_pem))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| TransportError::ConfigError(format!("cert PEM parse: {e}")))?;

        let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut BufReader::new(key_pem))
            .map_err(|e| TransportError::ConfigError(format!("key PEM parse: {e}")))?
            .ok_or_else(|| TransportError::ConfigError("no private key found".into()))?;

        // ADR-038 §D4.1 / RFC 8446 §B.4 — restrict to TLS 1.3 only.
        // The mainline NFS-over-TLS path targets kernel 6.5+ which
        // negotiates TLS 1.3 exclusively; allowing TLS 1.2 would
        // expose us to the legacy cipher-suite surface (CBC-mode
        // suites etc.) for no compatibility benefit.
        //
        // Belt and suspenders: filter the CryptoProvider's
        // `cipher_suites` to TLS 1.3 only AND pass `&[TLS13]` to
        // `with_protocol_versions`. The provider filter is the
        // load-bearing one (it removes the suites entirely so a
        // misconfigured peer can't even propose them); the version
        // filter pins the ClientHello supported_versions extension.
        let mut provider = rustls::crypto::aws_lc_rs::default_provider();
        provider
            .cipher_suites
            .retain(|cs| cs.version() == &rustls::version::TLS13);
        rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| TransportError::ConfigError(format!("tls 1.3-only: {e}")))?
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(server_certs, key)
            .map_err(|e| TransportError::ConfigError(format!("server config: {e}")))
    }

    /// Get the TLS connector for outbound connections.
    pub(crate) fn connector(&self) -> &TlsConnector {
        &self.connector
    }
}

impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConfig")
            .field("ca_count", &self.ca_certs.len())
            .finish_non_exhaustive()
    }
}
