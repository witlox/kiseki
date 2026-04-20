//! Server configuration.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Server configuration — populated from environment or defaults.
pub struct ServerConfig {
    /// Address for the data-path gRPC listener.
    pub data_addr: SocketAddr,
    /// Address for the advisory gRPC listener (isolated runtime).
    pub advisory_addr: SocketAddr,
    /// S3 HTTP gateway address.
    pub s3_addr: SocketAddr,
    /// NFS server address.
    pub nfs_addr: SocketAddr,
    /// TLS configuration paths (None = plaintext, for development only).
    pub tls: Option<TlsFiles>,
    /// Create a well-known bootstrap shard on startup (for e2e tests).
    pub bootstrap: bool,
}

/// Paths to TLS certificate files.
#[allow(clippy::struct_field_names)] // all fields are paths — the suffix is intentional
pub struct TlsFiles {
    /// Cluster CA certificate PEM.
    pub ca_path: PathBuf,
    /// This node's certificate chain PEM.
    pub cert_path: PathBuf,
    /// This node's private key PEM.
    pub key_path: PathBuf,
    /// Optional CRL PEM for certificate revocation.
    pub crl_path: Option<PathBuf>,
}

impl ServerConfig {
    /// Load config from environment variables with sensible defaults.
    ///
    /// TLS is enabled if `KISEKI_CA_PATH`, `KISEKI_CERT_PATH`, and
    /// `KISEKI_KEY_PATH` are all set. Otherwise the server runs in
    /// plaintext mode (development only — logged as a warning).
    pub fn from_env() -> Self {
        let data_addr = std::env::var("KISEKI_DATA_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:9100".into())
            .parse()
            .expect("invalid KISEKI_DATA_ADDR");

        let advisory_addr = std::env::var("KISEKI_ADVISORY_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:9101".into())
            .parse()
            .expect("invalid KISEKI_ADVISORY_ADDR");

        let tls = match (
            std::env::var("KISEKI_CA_PATH"),
            std::env::var("KISEKI_CERT_PATH"),
            std::env::var("KISEKI_KEY_PATH"),
        ) {
            (Ok(ca), Ok(cert), Ok(key)) => Some(TlsFiles {
                ca_path: PathBuf::from(ca),
                cert_path: PathBuf::from(cert),
                key_path: PathBuf::from(key),
                crl_path: std::env::var("KISEKI_CRL_PATH").ok().map(PathBuf::from),
            }),
            _ => None,
        };

        let s3_addr = std::env::var("KISEKI_S3_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:9000".into())
            .parse()
            .expect("invalid KISEKI_S3_ADDR");

        let nfs_addr = std::env::var("KISEKI_NFS_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:2049".into())
            .parse()
            .expect("invalid KISEKI_NFS_ADDR");

        let bootstrap = std::env::var("KISEKI_BOOTSTRAP")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        Self {
            data_addr,
            advisory_addr,
            s3_addr,
            nfs_addr,
            tls,
            bootstrap,
        }
    }
}
