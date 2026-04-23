//! Server configuration.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Server configuration — populated from environment or defaults.
pub struct ServerConfig {
    /// Address for the data-path gRPC listener.
    pub data_addr: SocketAddr,
    /// Address for the advisory gRPC listener (isolated runtime).
    pub advisory_addr: SocketAddr,
    /// Address for the advisory TCP stream listener (non-gRPC clients).
    pub advisory_stream_addr: SocketAddr,
    /// S3 HTTP gateway address.
    pub s3_addr: SocketAddr,
    /// NFS server address.
    pub nfs_addr: SocketAddr,
    /// Prometheus metrics HTTP address.
    pub metrics_addr: SocketAddr,
    /// TLS configuration paths (None = plaintext, for development only).
    pub tls: Option<TlsFiles>,
    /// Data directory for persistent storage (redb). None = in-memory only.
    pub data_dir: Option<std::path::PathBuf>,
    /// This node's Raft ID (0 = single-node mode).
    pub node_id: u64,
    /// Raft peer addresses (comma-separated "id=addr" pairs).
    pub raft_peers: Vec<(u64, String)>,
    /// Raft RPC listen address.
    pub raft_addr: Option<SocketAddr>,
    /// Create a well-known bootstrap shard on startup (for e2e tests).
    pub bootstrap: bool,
    /// Metadata soft limit percentage (ADR-030, default 50).
    pub meta_soft_limit_pct: u8,
    /// Metadata hard limit percentage (ADR-030, default 75).
    pub meta_hard_limit_pct: u8,
    /// Raw block device paths for `DeviceBackend` (comma-separated).
    /// When set, kiseki manages these devices directly instead of using
    /// the `data_dir` filesystem.
    pub raw_devices: Vec<String>,
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

        let advisory_stream_addr = std::env::var("KISEKI_ADVISORY_STREAM_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:9102".into())
            .parse()
            .expect("invalid KISEKI_ADVISORY_STREAM_ADDR");

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

        let metrics_addr = std::env::var("KISEKI_METRICS_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:9090".into())
            .parse()
            .expect("invalid KISEKI_METRICS_ADDR");

        let data_dir = std::env::var("KISEKI_DATA_DIR")
            .ok()
            .map(std::path::PathBuf::from);

        let node_id = std::env::var("KISEKI_NODE_ID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let raft_peers = std::env::var("KISEKI_RAFT_PEERS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(|entry| {
                let (id_str, addr) = entry.split_once('=')?;
                let id: u64 = id_str.parse().ok()?;
                Some((id, addr.to_owned()))
            })
            .collect();

        let raft_addr = std::env::var("KISEKI_RAFT_ADDR")
            .ok()
            .and_then(|v| v.parse().ok());

        let bootstrap = std::env::var("KISEKI_BOOTSTRAP").is_ok_and(|v| v == "true" || v == "1");

        let meta_soft_limit_pct = std::env::var("KISEKI_META_SOFT_LIMIT_PCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50);

        let meta_hard_limit_pct = std::env::var("KISEKI_META_HARD_LIMIT_PCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(75);

        let raw_devices = std::env::var("KISEKI_RAW_DEVICES")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        Self {
            data_addr,
            advisory_addr,
            advisory_stream_addr,
            s3_addr,
            nfs_addr,
            metrics_addr,
            tls,
            data_dir,
            node_id,
            raft_peers,
            raft_addr,
            bootstrap,
            meta_soft_limit_pct,
            meta_hard_limit_pct,
            raw_devices,
        }
    }
}
