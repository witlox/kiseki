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
    /// Optional portmapper (RFC 1057) listener address. When set, the
    /// server binds a minimal portmapper that resolves NFS3 / MOUNT3
    /// over TCP to `nfs_addr`'s port. Required for unmodified Linux
    /// `mount -t nfs -o vers=3` clients (Bug 10). `None` disables the
    /// listener — clients must then mount with explicit
    /// `mountport=2049,port=2049,mountproto=tcp`. Conventional
    /// production binding is `0.0.0.0:111`, which requires
    /// `CAP_NET_BIND_SERVICE` or root.
    pub portmap_addr: Option<SocketAddr>,
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
    /// Optional override for fabric (chunk-fetch) peer addresses.
    /// Same `id=host:port` format as `raft_peers`. When unset, fabric
    /// addresses are derived from `raft_peers` by substituting the
    /// local node's `data_addr` port — which assumes every node binds
    /// the same data-path port. That holds in containerized/hostnamed
    /// deployments (docker-compose) but breaks for localhost
    /// multi-node where every node has a distinct `data_addr` port.
    /// The BDD `ClusterHarness` (and any single-host integration test)
    /// sets this explicitly.
    pub fabric_peers: Vec<(u64, String)>,
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
    /// Optional backup configuration (ADR-016). `None` = backups disabled.
    pub backup: Option<BackupSettings>,
    /// Optional SPIFFE workload-API socket / SVID-key path
    /// (`KISEKI_SPIFFE_SOCKET`). When set, takes precedence over
    /// mTLS-derived node identity for the Raft key store at-rest
    /// encryption (Phase 14e).
    pub spiffe_socket: Option<PathBuf>,
    /// pNFS Data Server listener address (ADR-038 §D9). Default `:2052`.
    /// `None` disables the DS endpoint; if pNFS is enabled, must be set.
    pub ds_addr: Option<SocketAddr>,
    /// Optional override for per-peer DS endpoints. Same
    /// `id=host:port` format as `raft_peers`. When unset, the
    /// runtime derives DS addrs from `raft_peers` by substituting
    /// `ds_addr`'s port — which assumes every node binds the same
    /// DS port (true for containerized/hostnamed deployments). For
    /// localhost-multi-node where every node has a distinct DS
    /// port (the BDD `ClusterHarness`), set this explicitly so
    /// `MdsLayoutManager` advertises the right per-node DS uaddrs
    /// in LAYOUTGET / GETDEVICEINFO replies.
    pub ds_peers: Vec<(u64, String)>,
    /// Whether pNFS layout delegation is offered to NFSv4.1 clients
    /// (ADR-038 §D9). Default `true`.
    pub pnfs_enabled: bool,
    /// pNFS configuration knobs (ADR-038 §D9).
    pub pnfs: PnfsSettings,
    /// Operator opt-in for the audited plaintext-NFS fallback
    /// (ADR-038 §D4.2). Has effect only if `KISEKI_INSECURE_NFS=true`
    /// is also set at process start. Default `false`.
    pub allow_plaintext_nfs: bool,
}

/// pNFS configuration (ADR-038 §D9).
#[derive(Clone)]
#[allow(dead_code)] // Phase 15a wires `stripe_size_bytes` only; the rest are read in Phase 15b.
pub struct PnfsSettings {
    /// Stripe size in bytes (1 MiB default).
    pub stripe_size_bytes: u64,
    /// Layout TTL in seconds. Auto-halved to 60s under
    /// the plaintext-NFS fallback (ADR-038 §D4.2).
    pub layout_ttl_seconds: u64,
    /// Maximum live entries in the MDS layout cache (I-PN8).
    pub layout_cache_max_entries: usize,
    /// Sweep interval for the layout cache (I-PN8). Defaults to
    /// `layout_ttl_seconds / 4`.
    pub layout_cache_sweep_interval_seconds: u64,
    /// Phase 15c.5 step 1: cap on stripes emitted per LAYOUTGET.
    /// Linux kernel sends `loga_length = u64::MAX` (RFC 5661
    /// §18.43.1 "rest of file" sentinel); without a cap the server
    /// OOMs trying to emit ~281e12 stripes.
    ///
    /// Phase 15c.5 step 1: cap on stripes emitted per LAYOUTGET.
    /// Default 64 (= 64 MiB extent at 1 MiB stripes), roughly 4×
    /// small-rsize Linux NFS readahead. Memory bounded at I-PN8
    /// 100k-entry cache cap × 64 stripes × ~200 bytes/stripe ≈
    /// 1.3 GiB worst case.
    ///
    /// Phase 15c.8 finding: sustained 8+ MiB sequential reads
    /// from a Linux 6.x kernel pNFS client hang in a
    /// LAYOUTGET/LAYOUTRETURN loop regardless of this cap (tried
    /// 1 and 64). The root cause is that our layout encoding
    /// emits one `layout4` segment per stripe with its own
    /// `ff_mirror4` carrying a single DS — Linux's flex-files
    /// driver doesn't dispatch reads efficiently across multiple
    /// per-stripe segments. The correct RFC 8435 encoding for
    /// striping uses ONE segment with multiple `ff_mirrors<>`
    /// driven by `stripe_unit` modulo `num_mirrors`, which would
    /// require a per-mirror DS that holds every Nth stripe — a
    /// substantial DS-side change. Tracked as Phase 15c.9 follow-
    /// up; for now `test_pnfs_plaintext_fallback` (1 MiB) is
    /// the witness that pNFS dispatch works.
    pub max_stripes_per_layout: usize,
}

impl Default for PnfsSettings {
    fn default() -> Self {
        Self {
            stripe_size_bytes: 1_048_576,
            layout_ttl_seconds: 300,
            layout_cache_max_entries: 100_000,
            layout_cache_sweep_interval_seconds: 75,
            max_stripes_per_layout: 64,
        }
    }
}

/// Backup destination + retention.
///
/// `KISEKI_BACKUP_BACKEND=fs` selects [`BackupBackend::FileSystem`] and
/// requires `KISEKI_BACKUP_DIR`.
///
/// `KISEKI_BACKUP_BACKEND=s3` selects [`BackupBackend::S3`] and requires
/// `KISEKI_BACKUP_S3_ENDPOINT`, `KISEKI_BACKUP_S3_REGION`,
/// `KISEKI_BACKUP_S3_BUCKET`, `KISEKI_BACKUP_S3_ACCESS_KEY`, and
/// `KISEKI_BACKUP_S3_SECRET_KEY`.
pub struct BackupSettings {
    /// Where to write snapshots.
    pub backend: BackupBackend,
    /// Days to retain snapshots before `cleanup_old` deletes them.
    pub retention_days: u32,
    /// Whether snapshots include shard data (vs metadata-only).
    pub include_data: bool,
    /// How often the runtime invokes `cleanup_old`. Defaults to 24h.
    pub cleanup_interval_secs: u64,
}

/// Where backup snapshots land.
pub enum BackupBackend {
    /// Local directory.
    FileSystem {
        /// Snapshots live under this directory.
        dir: PathBuf,
    },
    /// S3-compatible object store.
    S3 {
        /// Endpoint URL (e.g. `https://s3.us-east-1.amazonaws.com`).
        endpoint: String,
        /// Region.
        region: String,
        /// Bucket (must already exist).
        bucket: String,
        /// Access key id.
        access_key_id: String,
        /// Secret access key.
        secret_access_key: String,
    },
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
    #[allow(clippy::too_many_lines)] // env-loader: each KISEKI_* var is one line
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

        // KISEKI_PORTMAP_ADDR=disabled disables the listener; otherwise
        // defaults to 0.0.0.0:111 (privileged — needs CAP_NET_BIND_SERVICE).
        let portmap_addr = match std::env::var("KISEKI_PORTMAP_ADDR")
            .as_deref()
            .unwrap_or("0.0.0.0:111")
        {
            "disabled" | "" => None,
            other => Some(other.parse().expect("invalid KISEKI_PORTMAP_ADDR")),
        };

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

        let fabric_peers = std::env::var("KISEKI_FABRIC_PEERS")
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

        let backup = parse_backup_from_env();

        let spiffe_socket = std::env::var("KISEKI_SPIFFE_SOCKET")
            .ok()
            .map(PathBuf::from);

        let ds_addr = std::env::var("KISEKI_DS_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:2052".into())
            .parse()
            .ok();

        let ds_peers = std::env::var("KISEKI_DS_PEERS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(|entry| {
                let (id_str, addr) = entry.split_once('=')?;
                let id: u64 = id_str.parse().ok()?;
                Some((id, addr.to_owned()))
            })
            .collect();

        let pnfs_enabled =
            std::env::var("KISEKI_PNFS_ENABLED").map_or(true, |v| v == "true" || v == "1");

        let mut pnfs = PnfsSettings::default();
        if let Some(v) = std::env::var("KISEKI_PNFS_STRIPE_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            pnfs.stripe_size_bytes = v;
        }
        if let Some(v) = std::env::var("KISEKI_PNFS_LAYOUT_TTL_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            pnfs.layout_ttl_seconds = v;
            pnfs.layout_cache_sweep_interval_seconds = (v / 4).max(1);
        }

        let allow_plaintext_nfs =
            std::env::var("KISEKI_ALLOW_PLAINTEXT_NFS").is_ok_and(|v| v == "true" || v == "1");

        Self {
            data_addr,
            advisory_addr,
            advisory_stream_addr,
            s3_addr,
            nfs_addr,
            portmap_addr,
            metrics_addr,
            tls,
            data_dir,
            node_id,
            raft_peers,
            fabric_peers,
            raft_addr,
            bootstrap,
            meta_soft_limit_pct,
            meta_hard_limit_pct,
            raw_devices,
            backup,
            spiffe_socket,
            ds_addr,
            ds_peers,
            pnfs_enabled,
            pnfs,
            allow_plaintext_nfs,
        }
    }
}

fn parse_backup_from_env() -> Option<BackupSettings> {
    let backend_kind = std::env::var("KISEKI_BACKUP_BACKEND").ok()?;
    let backend = match backend_kind.as_str() {
        "fs" => {
            let dir = std::env::var("KISEKI_BACKUP_DIR").map(PathBuf::from).ok()?;
            BackupBackend::FileSystem { dir }
        }
        "s3" => BackupBackend::S3 {
            endpoint: std::env::var("KISEKI_BACKUP_S3_ENDPOINT").ok()?,
            region: std::env::var("KISEKI_BACKUP_S3_REGION").ok()?,
            bucket: std::env::var("KISEKI_BACKUP_S3_BUCKET").ok()?,
            access_key_id: std::env::var("KISEKI_BACKUP_S3_ACCESS_KEY").ok()?,
            secret_access_key: std::env::var("KISEKI_BACKUP_S3_SECRET_KEY").ok()?,
        },
        other => {
            tracing::warn!(
                backend = other,
                "ignoring KISEKI_BACKUP_BACKEND: expected 'fs' or 's3'"
            );
            return None;
        }
    };
    let retention_days = std::env::var("KISEKI_BACKUP_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7);
    let include_data =
        std::env::var("KISEKI_BACKUP_INCLUDE_DATA").is_ok_and(|v| v == "true" || v == "1");
    let cleanup_interval_secs = std::env::var("KISEKI_BACKUP_CLEANUP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(86_400); // 24h
    Some(BackupSettings {
        backend,
        retention_days,
        include_data,
        cleanup_interval_secs,
    })
}
