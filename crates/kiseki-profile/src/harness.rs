//! Single-node `kiseki-server` lifecycle for the profiling driver.
//!
//! Mirrors `kiseki-acceptance::steps::harness` but trimmed to the
//! pieces the profile driver actually needs: bind ephemeral ports,
//! spawn the binary, wait for `/health`, drop = SIGTERM.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Ports {
    pub grpc_data: u16,
    pub grpc_advisory: u16,
    pub s3_http: u16,
    pub nfs_tcp: u16,
    pub metrics: u16,
    pub raft: u16,
    pub ds_tcp: u16,
}

impl Ports {
    pub fn allocate() -> Self {
        let mut listeners = Vec::with_capacity(7);
        let mut ports = Vec::with_capacity(7);
        for _ in 0..7 {
            let sock = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            ports.push(sock.local_addr().unwrap().port());
            listeners.push(sock);
        }
        // Listeners drop here; the kernel may recycle these ports
        // before the child binds, but for a single profiling
        // process the chance of collision is negligible.
        drop(listeners);
        Self {
            grpc_data: ports[0],
            grpc_advisory: ports[1],
            s3_http: ports[2],
            nfs_tcp: ports[3],
            metrics: ports[4],
            raft: ports[5],
            ds_tcp: ports[6],
        }
    }
}

pub struct ProfileServer {
    process: Child,
    _data_dir: tempfile::TempDir,
    pub s3_base: String,
    pub nfs_addr: std::net::SocketAddr,
    pub ds_addr: std::net::SocketAddr,
    pub ports: Ports,
}

impl ProfileServer {
    pub async fn start(server_bin: Option<&Path>) -> Result<Self, String> {
        let binary = match server_bin {
            Some(p) => p.to_path_buf(),
            None => find_server_binary()?,
        };
        let data_dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        let ports = Ports::allocate();
        let mut cmd = Command::new(&binary);
        cmd.env_clear()
            .env("KISEKI_DATA_ADDR", format!("127.0.0.1:{}", ports.grpc_data))
            .env(
                "KISEKI_ADVISORY_ADDR",
                format!("127.0.0.1:{}", ports.grpc_advisory),
            )
            .env("KISEKI_S3_ADDR", format!("127.0.0.1:{}", ports.s3_http))
            .env("KISEKI_NFS_ADDR", format!("127.0.0.1:{}", ports.nfs_tcp))
            .env("KISEKI_DS_ADDR", format!("127.0.0.1:{}", ports.ds_tcp))
            .env(
                "KISEKI_METRICS_ADDR",
                format!("127.0.0.1:{}", ports.metrics),
            )
            .env("KISEKI_RAFT_ADDR", format!("127.0.0.1:{}", ports.raft))
            .env("KISEKI_DATA_DIR", data_dir.path())
            .env("KISEKI_NODE_ID", "1")
            .env("KISEKI_BOOTSTRAP", "true")
            .env("KISEKI_ALLOW_PLAINTEXT_NFS", "true")
            .env("KISEKI_INSECURE_NFS", "true")
            .env(
                "RUST_LOG",
                std::env::var("KISEKI_PROFILE_RUST_LOG").unwrap_or_else(|_| "warn".into()),
            )
            .env("PATH", std::env::var("PATH").unwrap_or_default());
        // Forward optional self-profiling env vars. The server's
        // pprof guard reads `KISEKI_PPROF_OUT` and dumps a flamegraph
        // SVG at that path on SIGTERM; dhat reads `DHAT_OUTPUT_FILE`.
        for var in ["KISEKI_PPROF_OUT", "DHAT_OUTPUT_FILE"] {
            if let Ok(v) = std::env::var(var) {
                cmd.env(var, v);
            }
        }
        let child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn kiseki-server at {}: {e}", binary.display()))?;

        let s3_base = format!("http://127.0.0.1:{}", ports.s3_http);
        let nfs_addr: std::net::SocketAddr =
            format!("127.0.0.1:{}", ports.nfs_tcp).parse().unwrap();
        let ds_addr: std::net::SocketAddr = format!("127.0.0.1:{}", ports.ds_tcp).parse().unwrap();
        let mut server = Self {
            process: child,
            _data_dir: data_dir,
            s3_base,
            nfs_addr,
            ds_addr,
            ports,
        };

        // /health readiness — same probe the BDD harness uses.
        let http = reqwest::Client::new();
        let url = server.metrics_url() + "/health";
        let url = url.replace("/metrics/health", "/health");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(resp) = http.get(&url).send().await {
                if resp.status().is_success() {
                    break;
                }
            }
            if let Some(status) = server
                .process
                .try_wait()
                .map_err(|e| format!("try_wait: {e}"))?
            {
                return Err(format!("kiseki-server exited early: {status}"));
            }
            if Instant::now() >= deadline {
                return Err(format!("kiseki-server /health never reached ready: {url}"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // S3 PUT probe — confirms the data path is end-to-end.
        let probe_url = format!("{}/default/_profile_probe", server.s3_base);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(resp) = http.put(&probe_url).body(b"x".to_vec()).send().await {
                if resp.status().is_success() {
                    break;
                }
            }
            if Instant::now() >= deadline {
                return Err("S3 gateway not ready within 30s".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(server)
    }

    pub fn metrics_url(&self) -> String {
        format!("http://127.0.0.1:{}/metrics", self.ports.metrics)
    }
}

impl Drop for ProfileServer {
    fn drop(&mut self) {
        // SIGTERM via libc::kill so the child flushes stdout/stderr
        // (a SIGKILL via Child::kill loses any in-flight tracing).
        // Wait up to 30 s for the child to exit gracefully — pprof
        // flamegraph rendering on a 30-second profile sample can
        // take several seconds (frame symbolication + SVG write).
        // The BDD harness uses a tight 2 s window because it doesn't
        // need pprof; the profile harness gives the render time to
        // finish so the SVG actually lands on disk.
        send_sigterm(self.process.id());
        for _ in 0..600 {
            if matches!(self.process.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn send_sigterm(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: libc::kill is async-signal-safe and the process id
        // is a u32 obtained from std::process::Child::id() which the
        // kernel guarantees is valid during Drop.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn send_sigterm(_pid: u32) {}

fn find_server_binary() -> Result<PathBuf, String> {
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
        let candidate = workspace.join("target").join(profile).join("kiseki-server");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("kiseki-server binary not found. Build first: \
         `cargo build -p kiseki-server` or set KISEKI_SERVER_BIN"
        .into())
}
