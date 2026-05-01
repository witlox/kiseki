//! Server harness — spawns a real `kiseki-server` binary and provides
//! network clients for @integration BDD steps.
//!
//! Steps that are tagged @integration MUST use `world.server()` to get
//! the harness, then call gRPC/HTTP through it. They MUST NOT call
//! domain objects directly. See `.claude/roles/implementer.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Allocated ports for one server instance.
#[derive(Debug, Clone)]
pub struct ServerPorts {
    pub grpc_data: u16,
    pub grpc_advisory: u16,
    pub s3_http: u16,
    pub nfs_tcp: u16,
    pub metrics: u16,
    pub raft: u16,
}

impl ServerPorts {
    /// Bind ephemeral TCP sockets, record ports, close immediately.
    pub fn allocate() -> Self {
        let mut ports = Vec::new();
        for _ in 0..6 {
            let sock =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            ports.push(sock.local_addr().unwrap().port());
        }
        Self {
            grpc_data: ports[0],
            grpc_advisory: ports[1],
            s3_http: ports[2],
            nfs_tcp: ports[3],
            metrics: ports[4],
            raft: ports[5],
        }
    }
}

/// A running `kiseki-server` instance with network clients.
///
/// This is the ONLY way @integration steps should interact with the
/// system. It holds a gRPC channel and an HTTP client — steps use
/// these to make real requests and assert on real responses.
pub struct ServerHarness {
    /// The child process.
    process: Child,
    /// Tempdir for KISEKI_DATA_DIR — dropped on harness drop.
    _data_dir: tempfile::TempDir,
    /// Allocated ports.
    pub ports: ServerPorts,
    /// gRPC channel to the data-path port.
    pub grpc: tonic::transport::Channel,
    /// HTTP client for S3 requests.
    pub http: reqwest::Client,
    /// S3 base URL.
    pub s3_base: String,
    /// Last response state from network calls.
    pub last_status: Option<u16>,
    pub last_body: Option<Vec<u8>>,
    pub last_etag: Option<String>,
    pub last_grpc_error: Option<String>,
    /// Name → value mappings for cross-step state (from responses only).
    pub response_state: HashMap<String, String>,
}

impl Drop for ServerHarness {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl ServerHarness {
    /// Spawn a kiseki-server and wait for readiness.
    pub async fn start() -> Result<Self, String> {
        let ports = ServerPorts::allocate();
        let data_dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        let binary = Self::find_binary()?;

        let child = Command::new(&binary)
            .env_clear()
            .env("KISEKI_DATA_ADDR", format!("127.0.0.1:{}", ports.grpc_data))
            .env(
                "KISEKI_ADVISORY_ADDR",
                format!("127.0.0.1:{}", ports.grpc_advisory),
            )
            .env("KISEKI_S3_ADDR", format!("127.0.0.1:{}", ports.s3_http))
            .env("KISEKI_NFS_ADDR", format!("127.0.0.1:{}", ports.nfs_tcp))
            .env(
                "KISEKI_METRICS_ADDR",
                format!("127.0.0.1:{}", ports.metrics),
            )
            .env("KISEKI_DATA_DIR", data_dir.path())
            .env("KISEKI_BOOTSTRAP", "true")
            .env("KISEKI_ALLOW_PLAINTEXT_NFS", "true")
            .env("KISEKI_INSECURE_NFS", "true")
            .env("KISEKI_NODE_ID", "1")
            .env("RUST_LOG", "warn")
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn kiseki-server at {}: {e}", binary.display()))?;

        let s3_base = format!("http://127.0.0.1:{}", ports.s3_http);
        let http = reqwest::Client::new();

        let mut harness = Self {
            process: child,
            _data_dir: data_dir,
            ports: ports.clone(),
            grpc: tonic::transport::Channel::from_static("http://[::]:1") // placeholder
                .connect_lazy(),
            http,
            s3_base,
            last_status: None,
            last_body: None,
            last_etag: None,
            last_grpc_error: None,
            response_state: HashMap::new(),
        };

        // Wait for gRPC readiness (60s).
        let grpc_addr = format!("http://127.0.0.1:{}", ports.grpc_data);
        let deadline = Instant::now() + Duration::from_secs(60);
        let channel = loop {
            match tonic::transport::Channel::from_shared(grpc_addr.clone())
                .unwrap()
                .connect()
                .await
            {
                Ok(ch) => break ch,
                Err(_) if Instant::now() < deadline => {
                    if let Ok(Some(status)) = harness.process.try_wait() {
                        return Err(format!("kiseki-server exited early: {status}"));
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(e) => return Err(format!("gRPC not ready within 60s: {e}")),
            }
        };
        harness.grpc = channel;

        // Wait for S3 bootstrap (30s).
        let probe_url = format!("{}/default/__probe__", harness.s3_base);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match harness.http.put(&probe_url).body("probe").send().await {
                Ok(resp) if resp.status().is_success() => break,
                _ if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                _ => return Err("S3 gateway not ready within 30s".into()),
            }
        }

        Ok(harness)
    }

    /// Build an S3 URL for the given path.
    pub fn s3_url(&self, path: &str) -> String {
        format!("{}/{}", self.s3_base, path.trim_start_matches('/'))
    }

    /// Find the kiseki-server binary.
    fn find_binary() -> Result<PathBuf, String> {
        if let Ok(p) = std::env::var("KISEKI_SERVER_BIN") {
            let path = PathBuf::from(p);
            if path.exists() {
                return Ok(path);
            }
        }
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest
            .ancestors()
            .find(|p| p.join("Cargo.lock").exists())
            .unwrap_or(manifest.as_path());
        for profile in ["release", "debug"] {
            let candidate = workspace
                .join("target")
                .join(profile)
                .join("kiseki-server");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        Err(
            "kiseki-server binary not found. Build first: \
             `cargo build -p kiseki-server` or set KISEKI_SERVER_BIN"
                .into(),
        )
    }
}
