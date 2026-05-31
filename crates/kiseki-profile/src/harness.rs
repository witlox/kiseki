//! Single-node and multi-node `kiseki-server` lifecycles for the
//! profiling driver.
//!
//! Mirrors `kiseki-acceptance::steps::harness` + `cluster_harness` but
//! trimmed to the pieces the profile driver actually needs: bind
//! ephemeral ports, spawn the binary, wait for `/health`, drop =
//! SIGTERM.
//!
//! Two entry points:
//!
//! - [`ProfileServer`] — single-node spawn, the historic perf-driver
//!   target. Unchanged behavior.
//! - [`Cluster`] — N-node local cluster (multiplexed Raft transport).
//!   Used by `--nodes N` (N>1) to drive the bench against a real
//!   distributed write path, the missing capability called out by
//!   `specs/findings/2026-05-30-decoupled-ack-perf-bottleneck-hunt.md`
//!   step 1.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Ports {
    pub grpc_data: u16,
    pub grpc_advisory: u16,
    pub s3_http: u16,
    pub nfs_tcp: u16,
    pub metrics: u16,
    pub raft: u16,
    pub ds_tcp: u16,
    /// ADR-042 §2.2 TCP-framed-postcard binding listener port.
    /// Allocated alongside the rest so the profiler can drive the
    /// `--binding tcp` mode without colliding with another process
    /// at the default 9103.
    pub tcp_framed: u16,
}

impl Ports {
    pub fn allocate() -> Self {
        PortReservation::allocate().release()
    }
}

/// Multi-node-safe port reservation. Binds 8 ephemeral sockets and
/// holds them until [`PortReservation::release`] is called immediately
/// before child spawn. Without this the kernel can recycle a
/// freshly-released ephemeral port to a later allocation, and the
/// unlucky child dies on bind with EADDRINUSE. Mirrors the
/// `kiseki-acceptance` `PortReservation` shape — same reasoning,
/// same windowed-release pattern.
pub struct PortReservation {
    _listeners: Vec<TcpListener>,
    ports: Ports,
}

impl PortReservation {
    pub fn allocate() -> Self {
        const N: usize = 8;
        let mut listeners = Vec::with_capacity(N);
        let mut ports = Vec::with_capacity(N);
        for _ in 0..N {
            let sock = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            ports.push(
                sock.local_addr()
                    .expect("freshly-bound socket must have a local addr")
                    .port(),
            );
            listeners.push(sock);
        }
        Self {
            _listeners: listeners,
            ports: Ports {
                grpc_data: ports[0],
                grpc_advisory: ports[1],
                s3_http: ports[2],
                nfs_tcp: ports[3],
                metrics: ports[4],
                raft: ports[5],
                ds_tcp: ports[6],
                tcp_framed: ports[7],
            },
        }
    }

    /// Drop the held listeners and return the port numbers. Call this
    /// immediately before spawning the child.
    #[must_use]
    pub fn release(self) -> Ports {
        self.ports
    }

    /// Borrow the port numbers without releasing the listeners.
    /// Used to build env strings (`KISEKI_RAFT_PEERS`, etc.) referencing
    /// every node's ports while reservations are still live.
    #[must_use]
    pub fn ports(&self) -> &Ports {
        &self.ports
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
    /// Start a profile-test server. Long by design: the function
    /// drives the full test setup — binary lookup, ephemeral port
    /// reservation, env-var wiring, child spawn, log capture,
    /// readiness polling, address parse — and splitting into
    /// helpers would just shuffle the same setup across multiple
    /// arguments without making the sequence clearer.
    #[allow(clippy::too_many_lines)]
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
            // ADR-042 §2.2 TCP-framed binding listener — bind to the
            // ephemeral port allocated alongside the gRPC stack so
            // `--binding tcp` mode has a known target.
            .env(
                "KISEKI_NATIVE_TCP_ADDR",
                format!("127.0.0.1:{}", ports.tcp_framed),
            )
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
        // Also forward operator-tuning env vars so the matrix can
        // sweep their effect (KISEKI_OBSERVABILITY, group-commit
        // intervals, etc.) without a recompile.
        for var in [
            "KISEKI_PPROF_OUT",
            "DHAT_OUTPUT_FILE",
            "KISEKI_OBSERVABILITY",
            "KISEKI_COMPOSITION_FLUSH_INTERVAL_MS",
            "KISEKI_CHUNK_FLUSH_INTERVAL_MS",
            "KISEKI_RAFT_FLUSH_INTERVAL_MS",
            // ADR-047 escalation harness knob — simulated outbound
            // RTT (µs) injected into every Raft client RPC. Read once
            // at module init by kiseki-raft::tcp_transport. Forwarded
            // here so the profile harness can sweep its effect on
            // single-host runs to model real-cluster RTTs.
            "KISEKI_RAFT_FAKE_RTT_US",
            // Chunk-device backing size (`PersistentChunkStore`).
            // Default 4 GiB caps the single-node bench at ~65k writes
            // before silent dedup-hit takes over — see
            // `specs/escalations/2026-05-30-decoupled-ack-perf-10x-analysis.md`.
            // Forwarded so a bench run can size the store to its
            // intended write volume.
            "KISEKI_CHUNK_DEVICE_BYTES",
        ] {
            if let Ok(v) = std::env::var(var) {
                cmd.env(var, v);
            }
        }
        let child = cmd
            .stdout(Stdio::null())
            .stderr(std::env::var("KISEKI_PROFILE_STDERR").map_or_else(
                |_| Stdio::null(),
                |path| {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .map_or_else(|_| Stdio::null(), Stdio::from)
                },
            ))
            .spawn()
            .map_err(|e| format!("spawn kiseki-server at {}: {e}", binary.display()))?;

        let s3_base = format!("http://127.0.0.1:{}", ports.s3_http);
        let nfs_addr: std::net::SocketAddr = format!("127.0.0.1:{}", ports.nfs_tcp)
            .parse()
            .expect("static 127.0.0.1:port format always parses as SocketAddr");
        let ds_addr: std::net::SocketAddr = format!("127.0.0.1:{}", ports.ds_tcp)
            .parse()
            .expect("static 127.0.0.1:port format always parses as SocketAddr");
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

// ---------------------------------------------------------------------------
// Multi-node cluster harness
// ---------------------------------------------------------------------------

/// One node in a [`Cluster`] — child process + ports + per-node data
/// dir. Dropped via [`Cluster::drop`] in reverse spawn order.
struct ClusterNode {
    /// Diagnostic field — included so panic messages can name the
    /// failing node (`leader_id=N not in nodes …` etc.).
    #[allow(dead_code)]
    node_id: u64,
    ports: Ports,
    _data_dir: tempfile::TempDir,
    process: Child,
}

/// N-node local `kiseki-server` cluster using the multiplexed Raft
/// transport (one Raft listener per node, ADR-041). Node 1 bootstraps;
/// 2..N join via `KISEKI_RAFT_PEERS`. Waits for `/health` on every
/// node + leader convergence on node 1's `/cluster/info` before
/// returning.
///
/// API shape mirrors [`ProfileServer`] so the protocol drivers can
/// pick endpoints uniformly. The leader's endpoints are what the
/// driver hits — the bench namespace is created on the leader's
/// admin endpoint and per ADR-033 the leader serves writes for every
/// shard it leads (multi-shard distribution becomes a perf follow-up
/// per the GH #99 memory note).
pub struct Cluster {
    nodes: BTreeMap<u64, ClusterNode>,
    /// Cached leader endpoints — picked at construction from node 1's
    /// `/cluster/info`. We don't currently track re-elections;
    /// profile runs are short and the leader rarely flips.
    leader_node_id: u64,
    leader_s3_base: String,
    leader_nfs_addr: std::net::SocketAddr,
    leader_ds_addr: std::net::SocketAddr,
    leader_tcp_framed: u16,
    leader_metrics_url: String,
    leader_grpc_data: u16,
}

impl Cluster {
    /// Spawn `n` kiseki-server processes in a local Raft cluster (node 1
    /// bootstraps). Waits for all `/health` endpoints + leader election
    /// before returning.
    ///
    /// - `n`: number of nodes (>= 1; for n == 1 prefer [`ProfileServer`]
    ///   which has a lighter boot path, but this works too).
    /// - `server_bin`: optional explicit path to `kiseki-server`; falls
    ///   back to the same discovery logic [`ProfileServer`] uses.
    /// - `pprof_out_base`: when `Some(P)`, each node gets
    ///   `KISEKI_PPROF_OUT=<P>.node{i}.svg`. Requires the
    ///   `kiseki-server` binary to be built with `--features pprof`;
    ///   without the feature the env var is silently ignored by the
    ///   server. The driver does NOT verify the server has pprof
    ///   compiled in — the caller is responsible for the build.
    #[allow(clippy::too_many_lines)]
    pub async fn start(
        n: usize,
        server_bin: Option<&Path>,
        pprof_out_base: Option<&Path>,
    ) -> Result<Self, String> {
        if n == 0 {
            return Err("Cluster requires at least 1 node".into());
        }
        let binary = match server_bin {
            Some(p) => p.to_path_buf(),
            None => find_server_binary()?,
        };

        // Reserve all ports up-front and HOLD the listeners for every
        // node, exactly like `cluster_harness.rs`. KISEKI_RAFT_PEERS /
        // KISEKI_FABRIC_PEERS / KISEKI_DS_PEERS / KISEKI_PEER_DATA_ADDRS
        // are the same env vars on every child, so peers must be known
        // before we spawn any of them. Reservations are released
        // one-at-a-time immediately before each child spawn.
        let mut reservations: BTreeMap<u64, PortReservation> = BTreeMap::new();
        for id in 1..=(n as u64) {
            reservations.insert(id, PortReservation::allocate());
        }

        let raft_peers_env = reservations
            .iter()
            .map(|(id, r)| format!("{id}=127.0.0.1:{}", r.ports().raft))
            .collect::<Vec<_>>()
            .join(",");
        let fabric_peers_env = reservations
            .iter()
            .map(|(id, r)| format!("{id}=127.0.0.1:{}", r.ports().grpc_data))
            .collect::<Vec<_>>()
            .join(",");
        let ds_peers_env = reservations
            .iter()
            .map(|(id, r)| format!("{id}=127.0.0.1:{}", r.ports().ds_tcp))
            .collect::<Vec<_>>()
            .join(",");

        // Spawn node 1 first (bootstrap). Wait for its bootstrap shard
        // to come up before starting 2..N — followers that race past
        // the leader's `initialize` call get stuck waiting for a vote.
        let mut nodes: BTreeMap<u64, ClusterNode> = BTreeMap::new();
        for id in 1..=(n as u64) {
            let ports = reservations
                .remove(&id)
                .expect("reservation present")
                .release();
            let pprof_out = pprof_out_base.map(|p| {
                let mut s = p.as_os_str().to_owned();
                s.push(format!(".node{id}.svg"));
                PathBuf::from(s)
            });
            let bootstrap = id == 1;
            let node = spawn_cluster_node(
                &binary,
                id,
                &ports,
                &raft_peers_env,
                &fabric_peers_env,
                &ds_peers_env,
                bootstrap,
                pprof_out.as_deref(),
            )?;
            // Probe /health before moving on to the next node.
            wait_for_health(&ports, Duration::from_secs(60)).await?;
            nodes.insert(id, node);
        }

        // Wait for leader election: poll node 1's /cluster/info until
        // leader_id is set AND the cluster sees all N peers. If this
        // fails we return Err so the driver doesn't run garbage.
        let node1_metrics = nodes.get(&1).expect("node 1 spawned").ports.metrics;
        let (leader_id, _peer_count) =
            wait_for_quorum(node1_metrics, n as u64, Duration::from_secs(60)).await?;

        // Derive leader endpoints. For a 1-node cluster leader_id == 1;
        // for N-node we use whatever node /cluster/info reports.
        let leader_node = nodes
            .get(&leader_id)
            .ok_or_else(|| format!("leader_id={leader_id} not in nodes {:?}", nodes.keys()))?;
        let leader_ports = leader_node.ports.clone();
        let leader_s3_base = format!("http://127.0.0.1:{}", leader_ports.s3_http);
        let leader_nfs_addr: std::net::SocketAddr = format!("127.0.0.1:{}", leader_ports.nfs_tcp)
            .parse()
            .expect("static 127.0.0.1:port format parses");
        let leader_ds_addr: std::net::SocketAddr = format!("127.0.0.1:{}", leader_ports.ds_tcp)
            .parse()
            .expect("static 127.0.0.1:port format parses");
        let leader_metrics_url = format!("http://127.0.0.1:{}/metrics", leader_ports.metrics);

        let cluster = Self {
            nodes,
            leader_node_id: leader_id,
            leader_s3_base,
            leader_nfs_addr,
            leader_ds_addr,
            leader_tcp_framed: leader_ports.tcp_framed,
            leader_metrics_url,
            leader_grpc_data: leader_ports.grpc_data,
        };

        // S3 PUT probe against the leader — confirms the data path is
        // end-to-end. We hit the `default` namespace (auto-created on
        // bootstrap) so this works before topology provisioning.
        let http = reqwest::Client::new();
        let probe_url = format!("{}/default/_profile_probe", cluster.leader_s3_base);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(resp) = http.put(&probe_url).body(b"x".to_vec()).send().await {
                if resp.status().is_success() {
                    break;
                }
            }
            if Instant::now() >= deadline {
                return Err("cluster leader S3 gateway not ready within 30s".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(cluster)
    }

    /// Provision the bench namespace + multi-shard topology by
    /// shelling out to `kiseki-admin topology namespace-create` against
    /// the leader's admin endpoint. Mirrors
    /// `infra/gcp/benchmarks/setup-shards.sh` minus the bench-tenant
    /// env-file plumbing.
    ///
    /// `admin_bin`: optional explicit path to `kiseki-admin`. When
    /// `None`, looks next to `kiseki-server` (caller usually has both
    /// in `target/release/`).
    ///
    /// Returns Ok on 201 (created) AND on 409 / "already exists"
    /// (idempotent). Errors otherwise so the driver halts before
    /// running against an unconfigured namespace.
    pub async fn provision_bench_topology(
        &self,
        admin_bin: Option<&Path>,
        shard_count: usize,
    ) -> Result<(), String> {
        self.provision_bench_topology_with_pool(admin_bin, shard_count, None)
            .await
    }

    /// ADR-048 §"Decision" — same as [`provision_bench_topology`] but
    /// before creating the bench namespace, creates a `Replication`
    /// pool with `requires_migration = true` and wires the bench
    /// namespace's `size_band_pools.replicated` at it. The per-pool
    /// slab-EC compactor task spawned at boot picks up the bench
    /// pool and migrates chunks to cold-tier slabs.
    ///
    /// `slab_ec_pool_name` is the name to give the migration-eligible
    /// pool (e.g. `"slab-ec-bench"`). When `None`, this falls through
    /// to the legacy unflagged path so the same harness drives the
    /// baseline run.
    pub async fn provision_bench_topology_with_pool(
        &self,
        admin_bin: Option<&Path>,
        shard_count: usize,
        slab_ec_pool_name: Option<&str>,
    ) -> Result<(), String> {
        // bench_default_ids — same values as
        // `kiseki-client::bench::bench_default_ids`. Inlined here so
        // kiseki-profile doesn't grow a dep on kiseki-client's `bench`
        // surface.
        const BENCH_TENANT_ID: &str = "179e565c-d506-5c59-8f82-7ae6e13f0aff";
        const BENCH_NAMESPACE_ID: &str = "6658810a-1c4d-564c-a888-7564b5e9e576";
        let admin = match admin_bin {
            Some(p) => p.to_path_buf(),
            None => find_admin_binary()?,
        };
        let endpoint = format!("http://127.0.0.1:{}", self.leader_node().ports.metrics);

        // ADR-048: when the caller asked for slab-EC, create the
        // migration-eligible pool BEFORE the namespace. Idempotent —
        // a fresh cluster always succeeds on first attempt; an
        // existing pool surfaces "already exists" which the
        // namespace-create below treats as success.
        if let Some(pool_name) = slab_ec_pool_name {
            let admin_for_pool = admin.clone();
            let endpoint_for_pool = endpoint.clone();
            let pool_name_owned = pool_name.to_string();
            let pool_res = tokio::task::spawn_blocking(move || -> Result<String, String> {
                let output = Command::new(&admin_for_pool)
                    .arg("--endpoint")
                    .arg(&endpoint_for_pool)
                    .arg("pool")
                    .arg("create")
                    .arg(&pool_name_owned)
                    .arg("--durability")
                    .arg("replication")
                    .arg("--replication-copies")
                    .arg("3")
                    .arg("--device-class")
                    .arg("mixed")
                    .arg("--initial-capacity")
                    .arg("100G")
                    .arg("--slab-ec")
                    .output()
                    .map_err(|e| format!("spawn kiseki-admin pool create: {e}"))?;
                if output.status.success() {
                    return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let combined = format!("{stdout}\n{stderr}");
                if combined.contains("already exists") || combined.contains("409") {
                    return Ok(format!("idempotent (already exists): {combined}"));
                }
                Err(format!("rc={:?}: {combined}", output.status.code()))
            })
            .await
            .map_err(|e| format!("spawn_blocking: {e}"))?;
            pool_res?;
        }

        // Retry the namespace-create until the control-plane Raft
        // group has elected a leader. The bootstrap-shard leader (what
        // `/cluster/info` reports) is a DIFFERENT Raft group from the
        // control plane (ADR-033 §4); the control plane elects
        // separately and on a fresh multi-node cluster the gap is
        // 0.5 - 5 s. openraft's `client_write` does not retry on
        // `ForwardToLeader{leader_id: None}` — surfaced as
        // `control client_write: has to forward request to: None,
        // None` from the admin endpoint as HTTP 421. We retry on the
        // forwarding signature; on `already exists` we treat as
        // idempotent success.
        let admin_path = admin.clone();
        let create_deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let admin_path = admin_path.clone();
            let endpoint = endpoint.clone();
            let slab_pool_for_ns = slab_ec_pool_name.map(str::to_owned);
            let res = tokio::task::spawn_blocking(move || -> Result<String, String> {
                let mut cmd = Command::new(&admin_path);
                cmd.arg("--endpoint")
                    .arg(&endpoint)
                    .arg("topology")
                    .arg("namespace-create")
                    .arg(BENCH_NAMESPACE_ID)
                    .arg("--tenant")
                    .arg(BENCH_TENANT_ID)
                    .arg("--shards")
                    .arg(shard_count.to_string());
                if let Some(ref pool) = slab_pool_for_ns {
                    cmd.arg("--replicated-pool").arg(pool);
                }
                let output = cmd
                    .output()
                    .map_err(|e| format!("spawn kiseki-admin {}: {e}", admin_path.display()))?;
                if output.status.success() {
                    return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let combined = format!("{stdout}\n{stderr}");
                if combined.contains("already exists") || combined.contains("409") {
                    return Ok(format!("idempotent (already exists): {combined}"));
                }
                Err(format!("rc={:?}: {combined}", output.status.code()))
            })
            .await
            .map_err(|e| format!("spawn_blocking: {e}"))?;

            match res {
                Ok(_) => break,
                Err(e) => {
                    // Retry only on the leaderless-forward signature.
                    // Anything else is a real failure and we should
                    // halt so the perf number isn't measured against
                    // garbage.
                    let retryable = e.contains("forward request to")
                        || e.contains("HTTP 421")
                        || e.contains("HTTP 503");
                    if !retryable {
                        return Err(format!("kiseki-admin namespace-create failed: {e}"));
                    }
                    if Instant::now() >= create_deadline {
                        return Err(format!(
                            "kiseki-admin namespace-create never succeeded within 60s; last error: {e}"
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }

        // Post-create settle: the admin endpoint waits for each
        // per-shard Raft group to elect a leader before returning 201,
        // but the per-shard intent fan-out RPC path (ADR-047
        // `put_intent_and_fan`) requires each follower to have fully
        // wired its per-shard Raft handle into the multiplexed
        // listener's dispatch table — which lags the apply-hook
        // `register_shard` call by several seconds on a fresh
        // multi-node cluster. Without this sleep the first writes
        // against the bench namespace see `intent quorum: quorum lost`
        // (Connection reset by peer / early eof on the fan RPCs)
        // because peers' multiplexed listeners drop the intent_put
        // frames as unknown_shard before dispatch resolves. Surfaced
        // during the 2026-05-30 perf-bottleneck-hunt step-1
        // verification on a localhost-multi-node cluster.
        tokio::time::sleep(Duration::from_secs(15)).await;
        Ok(())
    }

    /// Recommended shard count for `n` nodes per ADR-033 §1:
    /// `max(min(3*n, 64), 3)`. Implemented via `clamp(3, 64)` on
    /// `3*n` — same value, fewer ops.
    #[must_use]
    pub fn shard_count_for(n: usize) -> usize {
        (3 * n).clamp(3, 64)
    }

    fn leader_node(&self) -> &ClusterNode {
        self.nodes
            .get(&self.leader_node_id)
            .expect("leader_node_id resolves to a node")
    }

    #[must_use]
    pub fn leader_s3_base(&self) -> &str {
        &self.leader_s3_base
    }

    #[must_use]
    pub fn leader_nfs_addr(&self) -> std::net::SocketAddr {
        self.leader_nfs_addr
    }

    #[must_use]
    pub fn leader_ds_addr(&self) -> std::net::SocketAddr {
        self.leader_ds_addr
    }

    #[must_use]
    pub fn leader_tcp_framed(&self) -> u16 {
        self.leader_tcp_framed
    }

    #[must_use]
    pub fn leader_grpc_data(&self) -> u16 {
        self.leader_grpc_data
    }

    #[must_use]
    pub fn leader_metrics_url(&self) -> String {
        self.leader_metrics_url.clone()
    }

    /// All node metrics URLs as `(node_id, url)` pairs, sorted by
    /// `node_id`. Used by the perf harness to scrape per-step hot-path
    /// histograms from every node (the leader-only scrape misses the
    /// `aux.*` follower side of the intent fan, since the current
    /// shard placement parks every shard leader on node 1).
    #[must_use]
    pub fn all_node_metrics_urls(&self) -> Vec<(u64, String)> {
        self.nodes
            .iter()
            .map(|(id, n)| (*id, format!("http://127.0.0.1:{}/metrics", n.ports.metrics)))
            .collect()
    }

    #[must_use]
    pub fn leader_node_id(&self) -> u64 {
        self.leader_node_id
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        // SIGTERM every node so the kiseki-server pprof guard renders
        // its SVG. Reverse order: node 1 (bootstrap) dies last so the
        // followers don't churn on a missing leader during their own
        // shutdown. Wait up to 30 s per node — same window as
        // `ProfileServer::drop` (pprof symbolication on a long run can
        // take seconds).
        let ids: Vec<u64> = self.nodes.keys().copied().rev().collect();
        for id in ids {
            if let Some(node) = self.nodes.get_mut(&id) {
                send_sigterm(node.process.id());
            }
        }
        // Now drain. We poll all nodes in parallel via tight inner loop;
        // 30 s deadline total per node mirrors ProfileServer.
        for id in self.nodes.keys().copied().rev().collect::<Vec<_>>() {
            if let Some(node) = self.nodes.get_mut(&id) {
                for _ in 0..600 {
                    if matches!(node.process.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = node.process.kill();
                let _ = node.process.wait();
            }
        }
    }
}

/// Single-node `kiseki-server` spawn for the cluster harness. Same
/// env wiring as `cluster_harness::spawn_with_env` from BDD plus the
/// profile-specific bits: `KISEKI_NATIVE_TCP_ADDR` (for
/// `--protocol native --binding tcp`), pprof env passthrough, and the
/// perf-tuning flush-interval knobs from setup-raw-storage.sh's
/// systemd unit.
#[allow(clippy::too_many_arguments)]
fn spawn_cluster_node(
    binary: &Path,
    node_id: u64,
    ports: &Ports,
    raft_peers_env: &str,
    fabric_peers_env: &str,
    ds_peers_env: &str,
    bootstrap: bool,
    pprof_out: Option<&Path>,
) -> Result<ClusterNode, String> {
    let data_dir = tempfile::tempdir().map_err(|e| format!("tempdir for node-{node_id}: {e}"))?;
    let mut cmd = Command::new(binary);
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
        // ADR-042 §2.2 TCP-framed binding — profile driver targets
        // this for `--protocol native --binding tcp`.
        .env(
            "KISEKI_NATIVE_TCP_ADDR",
            format!("127.0.0.1:{}", ports.tcp_framed),
        )
        .env("KISEKI_DATA_DIR", data_dir.path())
        .env("KISEKI_NODE_ID", node_id.to_string())
        .env("KISEKI_RAFT_PEERS", raft_peers_env)
        .env("KISEKI_FABRIC_PEERS", fabric_peers_env)
        // #103: explicit per-node native-data endpoints so the ADR-042
        // §4 proxy fallback can reach peers on their real ephemeral
        // ports (localhost-multi-node). Same id→grpc_data map as the
        // fabric. Required for cross-node forwarding to work.
        .env("KISEKI_PEER_DATA_ADDRS", fabric_peers_env)
        .env("KISEKI_DS_PEERS", ds_peers_env)
        .env("KISEKI_BOOTSTRAP", if bootstrap { "true" } else { "false" })
        .env("KISEKI_ALLOW_PLAINTEXT_NFS", "true")
        .env("KISEKI_INSECURE_NFS", "true")
        // Disable HTTP RBAC so the bench namespace-create call goes
        // through without a token. Production deployments leave both
        // unset.
        .env("KISEKI_ADMIN_AUTH_DISABLED", "true")
        .env("KISEKI_CLUSTER_INFO_PUBLIC", "true")
        // Perf-tuning knobs from setup-raw-storage.sh's systemd unit.
        // Keep the multi-node profile run shaped like the GCP perf
        // cluster so the flame graphs are representative.
        .env("KISEKI_COMPOSITION_FLUSH_INTERVAL_MS", "100")
        .env("KISEKI_CHUNK_FLUSH_INTERVAL_MS", "100")
        .env("KISEKI_RAFT_FLUSH_INTERVAL_MS", "100")
        .env("KISEKI_RAFT_THREADS", "64")
        .env(
            "RUST_LOG",
            std::env::var("KISEKI_PROFILE_RUST_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .env("PATH", std::env::var("PATH").unwrap_or_default());

    // Per-node pprof output. When `pprof_out` is `Some(P)` the server
    // writes its flamegraph SVG to that path on graceful SIGTERM (the
    // server's pprof guard renders in main.rs after the runtime
    // returns). Requires the `kiseki-server` binary to be built with
    // `--features pprof`.
    if let Some(p) = pprof_out {
        cmd.env("KISEKI_PPROF_OUT", p);
    }

    // Forward optional self-profiling env vars from the caller —
    // mirrors ProfileServer's passthrough. dhat lands a per-node
    // suffix the same way pprof does, but the server uses the literal
    // path so we hand it a {base}.node{i} variant when set.
    for var in [
        "DHAT_OUTPUT_FILE",
        "KISEKI_OBSERVABILITY",
        // ADR-047 escalation harness knob — every spawned node sees
        // the same simulated outbound Raft RTT so cross-node traffic
        // is delayed symmetrically. Same pass-through pattern as the
        // single-node ProfileServer above.
        "KISEKI_RAFT_FAKE_RTT_US",
        // Chunk-device backing size; matches the ProfileServer
        // forward list. Lets the bench size a per-node chunk store
        // to its actual write volume instead of the 4 GiB default.
        "KISEKI_CHUNK_DEVICE_BYTES",
    ] {
        if let Ok(v) = std::env::var(var) {
            if var == "DHAT_OUTPUT_FILE" {
                cmd.env(var, format!("{v}.node{node_id}"));
            } else {
                cmd.env(var, v);
            }
        }
    }

    // Optional log capture for post-mortem. When
    // `KISEKI_PROFILE_HARNESS_LOG_DIR` is set, each node's stdout +
    // stderr go to `{dir}/node-{id}.log`; otherwise /dev/null (the
    // historic behavior for ProfileServer).
    if let Ok(dir) = std::env::var("KISEKI_PROFILE_HARNESS_LOG_DIR") {
        let _ = std::fs::create_dir_all(&dir);
        let path = format!("{dir}/node-{node_id}.log");
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            if let Ok(f2) = f.try_clone() {
                cmd.stdout(Stdio::from(f));
                cmd.stderr(Stdio::from(f2));
            } else {
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());
            }
        } else {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
    } else {
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
    }

    let child = cmd.spawn().map_err(|e| {
        format!(
            "spawn kiseki-server (node-{node_id}) at {}: {e}",
            binary.display()
        )
    })?;

    Ok(ClusterNode {
        node_id,
        ports: ports.clone(),
        _data_dir: data_dir,
        process: child,
    })
}

/// Block until `/health` returns 200 on the node's metrics port.
async fn wait_for_health(ports: &Ports, deadline: Duration) -> Result<(), String> {
    let http = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/health", ports.metrics);
    let stop = Instant::now() + deadline;
    loop {
        if let Ok(resp) = http.get(&url).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= stop {
            return Err(format!("node /health never reached ready: {url}"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Poll node 1's `/cluster/info` until `leader_id` is set AND the
/// peer set has `n` members. Returns `(leader_id, peer_count)`.
async fn wait_for_quorum(
    node1_metrics_port: u16,
    expected_n: u64,
    deadline: Duration,
) -> Result<(u64, u64), String> {
    let http = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{node1_metrics_port}/cluster/info");
    let stop = Instant::now() + deadline;
    let mut last_seen: Option<(Option<u64>, Option<u64>)> = None;
    while Instant::now() < stop {
        if let Ok(resp) = http.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    let leader = json.get("leader_id").and_then(serde_json::Value::as_u64);
                    let peer_count = json
                        .get("peers")
                        .and_then(serde_json::Value::as_array)
                        .map(|a| a.len() as u64);
                    if let (Some(lid), Some(pc)) = (leader, peer_count) {
                        if lid != 0 && pc == expected_n {
                            return Ok((lid, pc));
                        }
                    }
                    last_seen = Some((leader, peer_count));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(format!(
        "cluster never converged within {deadline:?}: url={url} last_seen={last_seen:?} expected_n={expected_n}"
    ))
}

fn find_admin_binary() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("KISEKI_ADMIN_BIN") {
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
        let candidate = workspace.join("target").join(profile).join("kiseki-admin");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("kiseki-admin binary not found. Build first: \
         `cargo build -p kiseki-server --bin kiseki-admin` or set KISEKI_ADMIN_BIN"
        .into())
}
