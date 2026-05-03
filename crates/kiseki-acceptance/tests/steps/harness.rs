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
    /// pNFS Data Server port (ADR-038 §D9). The production default
    /// is `:2052`; the harness allocates an ephemeral port per node
    /// so multiple servers in a multi-node cluster can each bind
    /// their own DS listener without colliding on 2052.
    pub ds_tcp: u16,
}

impl ServerPorts {
    /// Bind ephemeral TCP sockets, record ports, close immediately.
    ///
    /// **Single-node only.** Multi-node callers must use
    /// [`PortReservation::allocate`] + [`PortReservation::release`] to
    /// hold all ports open across allocations — without that, the
    /// kernel can recycle a freshly-released ephemeral port to a
    /// later `allocate()` call, and the child whose port was reused
    /// dies on bind with `EADDRINUSE`. Surfaced when scaling
    /// `ClusterHarness` from 3 → 6 nodes (36 ports/cluster).
    pub fn allocate() -> Self {
        PortReservation::allocate().release()
    }
}

/// Holds 6 bound `TcpListener`s for the lifetime of the reservation.
/// `release()` drops the listeners and hands back the port numbers
/// for immediate child-process binding. The window between
/// `release()` and the child's `bind()` is microseconds; concurrent
/// reservations across multiple nodes never collide because each
/// reservation keeps its own listeners alive until release.
pub struct PortReservation {
    _listeners: Vec<std::net::TcpListener>,
    ports: ServerPorts,
}

impl PortReservation {
    /// Bind 7 ephemeral TCP sockets and hold them open.
    pub fn allocate() -> Self {
        let mut listeners = Vec::with_capacity(7);
        let mut ports = Vec::with_capacity(7);
        for _ in 0..7 {
            let sock = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            ports.push(sock.local_addr().unwrap().port());
            listeners.push(sock);
        }
        Self {
            _listeners: listeners,
            ports: ServerPorts {
                grpc_data: ports[0],
                grpc_advisory: ports[1],
                s3_http: ports[2],
                nfs_tcp: ports[3],
                metrics: ports[4],
                raft: ports[5],
                ds_tcp: ports[6],
            },
        }
    }

    /// Drop the held listeners and return the port numbers. Call
    /// this immediately before spawning the child.
    #[must_use]
    pub fn release(self) -> ServerPorts {
        // `_listeners` drops here, freeing the ports for the child to bind.
        self.ports
    }

    /// Borrow the port numbers without releasing the listeners.
    /// Used to build env strings (KISEKI_RAFT_PEERS, etc.) that
    /// reference every node's ports while reservations are still live.
    #[must_use]
    pub fn ports(&self) -> &ServerPorts {
        &self.ports
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
            // ADR-038 §D9: pNFS Data Server port. Default in
            // production is 2052; here we use the harness-allocated
            // ephemeral port so multiple servers don't collide.
            .env("KISEKI_DS_ADDR", format!("127.0.0.1:{}", ports.ds_tcp))
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

    /// Get an S3 client (kiseki-client RemoteHttpGateway) connected to
    /// the running server. Implements GatewayOps.
    pub fn s3_client(&self) -> kiseki_client::remote_http::RemoteHttpGateway {
        kiseki_client::remote_http::RemoteHttpGateway::new(&self.s3_base)
    }

    /// Get an NFSv4.1 client connected to the running server.
    pub fn nfs4_client(&self) -> kiseki_client::remote_nfs::v4::Nfs4Client {
        let addr = format!("127.0.0.1:{}", self.ports.nfs_tcp).parse().unwrap();
        kiseki_client::remote_nfs::v4::Nfs4Client::v41(addr)
    }

    /// Get an NFSv3 client connected to the running server.
    pub fn nfs3_client(&self) -> kiseki_client::remote_nfs::v3::Nfs3Client {
        let addr = format!("127.0.0.1:{}", self.ports.nfs_tcp).parse().unwrap();
        kiseki_client::remote_nfs::v3::Nfs3Client::new(addr)
    }

    /// Build an `AdvisoryServiceClient` over a fresh tonic Channel to
    /// the running server's advisory port. ADR-021 §3.b — the data-
    /// path workflow_ref validation needs a workflow declared via this
    /// service to find a hit.
    ///
    /// Returns `Err` when the channel can't be established (advisory
    /// port not bound, server crashed, etc.); the caller should treat
    /// any failure as scenario-fatal.
    pub async fn advisory_grpc_client(
        &self,
    ) -> Result<
        kiseki_proto::v1::workflow_advisory_service_client::WorkflowAdvisoryServiceClient<
            tonic::transport::Channel,
        >,
        String,
    > {
        let addr = format!("http://127.0.0.1:{}", self.ports.grpc_advisory);
        let channel = tonic::transport::Channel::from_shared(addr.clone())
            .map_err(|e| format!("invalid advisory addr {addr}: {e}"))?
            .connect()
            .await
            .map_err(|e| format!("advisory connect {addr}: {e}"))?;
        Ok(
            kiseki_proto::v1::workflow_advisory_service_client::WorkflowAdvisoryServiceClient::new(
                channel,
            ),
        )
    }

    /// Scrape the server's `/metrics` endpoint and return the body.
    /// Used by integration steps that assert on metrics surfaces
    /// (e.g. `kiseki_gateway_workflow_ref_writes_total`).
    pub async fn scrape_metrics(&self) -> Result<String, String> {
        let url = format!("http://127.0.0.1:{}/metrics", self.ports.metrics);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("metrics scrape {url}: {e}"))?;
        resp.text().await.map_err(|e| format!("metrics body: {e}"))
    }

    /// Send an ONC RPC call over TCP to the NFS port and return the
    /// reply body (after the record marker and RPC accept header).
    /// This is the building block for NFS @integration steps.
    pub fn nfs_rpc_call(
        &self,
        program: u32,
        version: u32,
        procedure: u32,
        body: &[u8],
    ) -> Result<Vec<u8>, String> {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let addr = format!("127.0.0.1:{}", self.ports.nfs_tcp);
        let mut stream = TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(5))
            .map_err(|e| format!("NFS TCP connect to {addr}: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // Build ONC RPC call message (RFC 5531 §8)
        let xid: u32 = 0x4b495345; // "KISE"
        let mut rpc = Vec::new();
        rpc.extend_from_slice(&xid.to_be_bytes()); // xid
        rpc.extend_from_slice(&0u32.to_be_bytes()); // msg_type = CALL
        rpc.extend_from_slice(&2u32.to_be_bytes()); // rpc_vers = 2
        rpc.extend_from_slice(&program.to_be_bytes()); // prog
        rpc.extend_from_slice(&version.to_be_bytes()); // vers
        rpc.extend_from_slice(&procedure.to_be_bytes()); // proc
                                                         // AUTH_NONE credentials + verifier
        rpc.extend_from_slice(&0u32.to_be_bytes()); // cred flavor = AUTH_NONE
        rpc.extend_from_slice(&0u32.to_be_bytes()); // cred length = 0
        rpc.extend_from_slice(&0u32.to_be_bytes()); // verf flavor = AUTH_NONE
        rpc.extend_from_slice(&0u32.to_be_bytes()); // verf length = 0
        rpc.extend_from_slice(body);

        // Record marker: last fragment flag + length
        let marker = 0x8000_0000 | (rpc.len() as u32);
        stream
            .write_all(&marker.to_be_bytes())
            .map_err(|e| format!("write marker: {e}"))?;
        stream
            .write_all(&rpc)
            .map_err(|e| format!("write rpc: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;

        // Read reply record marker
        let mut hdr = [0u8; 4];
        stream
            .read_exact(&mut hdr)
            .map_err(|e| format!("read reply marker: {e}"))?;
        let reply_marker = u32::from_be_bytes(hdr);
        let reply_len = (reply_marker & 0x7FFF_FFFF) as usize;

        // Read reply body
        let mut reply = vec![0u8; reply_len];
        stream
            .read_exact(&mut reply)
            .map_err(|e| format!("read reply body: {e}"))?;

        // Parse RPC reply header: xid(4) + msg_type(4) + reply_stat(4)
        // + verifier(8) + accept_stat(4) = 24 bytes minimum
        if reply.len() < 24 {
            return Err(format!("reply too short: {} bytes", reply.len()));
        }
        let reply_xid = u32::from_be_bytes(reply[0..4].try_into().unwrap());
        if reply_xid != xid {
            return Err(format!(
                "xid mismatch: expected {xid:#x}, got {reply_xid:#x}"
            ));
        }
        let accept_stat = u32::from_be_bytes(reply[20..24].try_into().unwrap());
        if accept_stat != 0 {
            return Err(format!("RPC rejected: accept_stat={accept_stat}"));
        }

        // Return everything after the 24-byte RPC accept header
        Ok(reply[24..].to_vec())
    }

    /// Establish an NFSv4.1 session: EXCHANGE_ID → CREATE_SESSION.
    /// Returns (client_id, session_id) for use in subsequent COMPOUNDs.
    pub fn nfs4_establish_session(&self) -> Result<(u64, [u8; 16]), String> {
        // --- Step 1: EXCHANGE_ID ---
        let mut exid_body = Vec::new();
        // COMPOUND: tag(0) + minor_version(1) + 1 op
        exid_body.extend_from_slice(&0u32.to_be_bytes()); // tag len
        exid_body.extend_from_slice(&1u32.to_be_bytes()); // minor_version = 1
        exid_body.extend_from_slice(&1u32.to_be_bytes()); // 1 op
        exid_body.extend_from_slice(&42u32.to_be_bytes()); // op EXCHANGE_ID
                                                           // verifier (8 bytes)
        exid_body.extend_from_slice(&[0u8; 8]);
        // owner_id (opaque): length + data
        let owner = b"bdd-test-client";
        exid_body.extend_from_slice(&(owner.len() as u32).to_be_bytes());
        exid_body.extend_from_slice(owner);
        // Pad to 4-byte boundary
        let pad = (4 - owner.len() % 4) % 4;
        exid_body.extend_from_slice(&vec![0u8; pad]);
        // flags = 0
        exid_body.extend_from_slice(&0u32.to_be_bytes());
        // state_protect = SP4_NONE (0)
        exid_body.extend_from_slice(&0u32.to_be_bytes());
        // impl_id array count = 0
        exid_body.extend_from_slice(&0u32.to_be_bytes());

        let reply = self.nfs_rpc_call(100003, 4, 1, &exid_body)?;
        // Parse: status(4) + tag_len(4) + numresults(4) + op(4) + status(4) + client_id(8)
        if reply.len() < 24 {
            return Err(format!("EXCHANGE_ID reply too short: {}", reply.len()));
        }
        let cmp_status = u32::from_be_bytes(reply[0..4].try_into().unwrap());
        if cmp_status != 0 {
            return Err(format!("EXCHANGE_ID COMPOUND failed: status={cmp_status}"));
        }
        let tag_len = u32::from_be_bytes(reply[4..8].try_into().unwrap()) as usize;
        let base = 8 + tag_len + ((4 - tag_len % 4) % 4) + 4; // after numresults
        let op_status = u32::from_be_bytes(reply[base + 4..base + 8].try_into().unwrap());
        if op_status != 0 {
            return Err(format!("EXCHANGE_ID op failed: status={op_status}"));
        }
        let client_id = u64::from_be_bytes(reply[base + 8..base + 16].try_into().unwrap());

        // --- Step 2: CREATE_SESSION ---
        let mut cs_body = Vec::new();
        cs_body.extend_from_slice(&0u32.to_be_bytes()); // tag len
        cs_body.extend_from_slice(&1u32.to_be_bytes()); // minor_version = 1
        cs_body.extend_from_slice(&1u32.to_be_bytes()); // 1 op
        cs_body.extend_from_slice(&43u32.to_be_bytes()); // op CREATE_SESSION
        cs_body.extend_from_slice(&client_id.to_be_bytes());
        cs_body.extend_from_slice(&1u32.to_be_bytes()); // sequence = 1
        cs_body.extend_from_slice(&0u32.to_be_bytes()); // flags = 0

        let reply = self.nfs_rpc_call(100003, 4, 1, &cs_body)?;
        if reply.len() < 24 {
            return Err(format!("CREATE_SESSION reply too short: {}", reply.len()));
        }
        let cmp_status = u32::from_be_bytes(reply[0..4].try_into().unwrap());
        if cmp_status != 0 {
            return Err(format!(
                "CREATE_SESSION COMPOUND failed: status={cmp_status}"
            ));
        }
        let tag_len = u32::from_be_bytes(reply[4..8].try_into().unwrap()) as usize;
        let base = 8 + tag_len + ((4 - tag_len % 4) % 4) + 4;
        let op_status = u32::from_be_bytes(reply[base + 4..base + 8].try_into().unwrap());
        if op_status != 0 {
            return Err(format!("CREATE_SESSION op failed: status={op_status}"));
        }
        let mut session_id = [0u8; 16];
        session_id.copy_from_slice(&reply[base + 8..base + 24]);

        Ok((client_id, session_id))
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
            let candidate = workspace.join("target").join(profile).join("kiseki-server");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        Err("kiseki-server binary not found. Build first: \
             `cargo build -p kiseki-server` or set KISEKI_SERVER_BIN"
            .into())
    }
}
