//! 3-node cluster harness — process-level singleton.
//!
//! Spawns three `kiseki-server` children once per test binary lifetime
//! and reuses them across every `@multi-node` scenario. Per-scenario
//! isolation comes from unique S3 bucket names (`bdd-{uuid}`), not from
//! restarting the cluster — startup costs ~5-15s and dwarfs scenario
//! work.
//!
//! Destructive scenarios (kill leader / restart node) take the inner
//! `Mutex` and respawn individual children; the cluster envelope (peer
//! list, ports, data dirs) survives so the next scenario inherits a
//! healed cluster.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, OnceCell};

use super::harness::ServerPorts;

/// Cluster size — fixed at 3 to mirror the docker-compose.3node.yml
/// topology and to give us a real 2-of-3 quorum.
const NODE_COUNT: u64 = 3;

/// One running `kiseki-server` instance plus its S3 client.
pub struct NodeHandle {
    pub node_id: u64,
    pub ports: ServerPorts,
    /// Tempdir for `KISEKI_DATA_DIR` — preserved across kill/restart so
    /// recovery from on-disk Raft log can be exercised.
    pub data_dir: tempfile::TempDir,
    pub process: Child,
    pub s3_base: String,
    pub http: reqwest::Client,
}

impl NodeHandle {
    pub fn s3_client(&self) -> kiseki_client::remote_http::RemoteHttpGateway {
        kiseki_client::remote_http::RemoteHttpGateway::new(&self.s3_base)
    }

    /// Admin endpoint URL — `/cluster/info`, `/health`, etc.
    pub fn admin_url(&self, path: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}",
            self.ports.metrics,
            path.trim_start_matches('/'),
        )
    }
}

impl Drop for NodeHandle {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

pub struct ClusterHarness {
    nodes: BTreeMap<u64, NodeHandle>,
    /// Same value passed to every node — keeps respawns aligned.
    raft_peers_env: String,
    /// Fabric (chunk-fetch) peer override. Required for localhost
    /// multi-node because each child binds a distinct data-path port —
    /// the default fabric derivation in `kiseki-server` assumes a
    /// shared data port (containerized deployments) and can't reach
    /// the right peer otherwise.
    fabric_peers_env: String,
    /// Path to the `kiseki-server` binary — resolved once.
    binary: PathBuf,
}

impl ClusterHarness {
    /// Spawn all 3 nodes. Node 1 bootstraps the Raft group; 2 and 3
    /// join. Returns once every node reports the same non-zero
    /// `leader_id` (30s deadline) — i.e. an election has converged.
    pub async fn start() -> Result<Self, String> {
        let binary = find_server_binary()?;

        // Allocate all ports up-front — `KISEKI_RAFT_PEERS` is the same
        // env var on every child, so peers must be known before we
        // spawn any of them.
        let mut ports: BTreeMap<u64, ServerPorts> = BTreeMap::new();
        for id in 1..=NODE_COUNT {
            ports.insert(id, ServerPorts::allocate());
        }

        let raft_peers_env = ports
            .iter()
            .map(|(id, p)| format!("{id}=127.0.0.1:{}", p.raft))
            .collect::<Vec<_>>()
            .join(",");
        let fabric_peers_env = ports
            .iter()
            .map(|(id, p)| format!("{id}=127.0.0.1:{}", p.grpc_data))
            .collect::<Vec<_>>()
            .join(",");

        // Spawn node 1 first (bootstrap). Wait for its bootstrap shard
        // to come up before starting 2/3 — followers that race past the
        // leader's `initialize` call get stuck waiting for a vote.
        let mut nodes = BTreeMap::new();
        let n1 = spawn_node(
            &binary,
            1,
            &ports[&1],
            &raft_peers_env,
            &fabric_peers_env,
            true,
        )?;
        wait_for_admin(&n1, Duration::from_secs(60)).await?;
        nodes.insert(1, n1);

        for id in 2..=NODE_COUNT {
            let node = spawn_node(
                &binary,
                id,
                &ports[&id],
                &raft_peers_env,
                &fabric_peers_env,
                false,
            )?;
            wait_for_admin(&node, Duration::from_secs(60)).await?;
            nodes.insert(id, node);
        }

        let mut cluster = Self {
            nodes,
            raft_peers_env,
            fabric_peers_env,
            binary,
        };
        cluster
            .wait_for_quorum(Duration::from_secs(30))
            .await
            .map_err(|e| format!("cluster never elected a leader: {e}"))?;
        Ok(cluster)
    }

    /// Borrow a node by id (panics if unknown — scenarios pass ids 1..=3).
    pub fn node(&self, id: u64) -> &NodeHandle {
        self.nodes
            .get(&id)
            .unwrap_or_else(|| panic!("unknown node {id} (have {:?})", self.nodes.keys()))
    }

    pub fn nodes(&self) -> impl Iterator<Item = &NodeHandle> {
        self.nodes.values()
    }

    /// Read `leader_id` from node 1's `/cluster/info`. Returns `None`
    /// while an election is in progress.
    pub async fn leader_id(&self) -> Option<u64> {
        leader_id_for(&self.nodes[&1]).await
    }

    /// Kill node `id` (SIGKILL) and wait for it to exit. The data dir
    /// stays — `restart_node` will reopen it.
    pub async fn kill_node(&mut self, id: u64) -> Result<(), String> {
        let node = self
            .nodes
            .get_mut(&id)
            .ok_or_else(|| format!("unknown node {id}"))?;
        node.process
            .kill()
            .map_err(|e| format!("kill node-{id}: {e}"))?;
        node.process
            .wait()
            .map_err(|e| format!("wait node-{id}: {e}"))?;
        Ok(())
    }

    /// Respawn a previously-killed node with the same node_id, ports,
    /// and data dir. The new child rejoins via the existing peer config.
    pub async fn restart_node(&mut self, id: u64) -> Result<(), String> {
        let node = self
            .nodes
            .get_mut(&id)
            .ok_or_else(|| format!("unknown node {id}"))?;
        // Defensive: if the prior child is still running, kill it.
        let _ = node.process.kill();
        let _ = node.process.wait();
        let new_child = spawn_with_env(
            &self.binary,
            node.node_id,
            &node.ports,
            &self.raft_peers_env,
            &self.fabric_peers_env,
            false,
            node.data_dir.path(),
        )?;
        // Replace in place — Drop on the old Child has already run via
        // `kill`+`wait` above; we just need to swap the field.
        let old_child = std::mem::replace(&mut node.process, new_child);
        // Old child already reaped; drop it explicitly to be tidy.
        drop(old_child);
        let id_for_log = node.node_id;
        wait_for_admin(self.node(id_for_log), Duration::from_secs(60)).await?;
        self.wait_for_quorum(Duration::from_secs(30)).await
    }

    /// Wait until every live node reports the same non-zero `leader_id`.
    pub async fn wait_for_quorum(&self, deadline: Duration) -> Result<(), String> {
        let stop = Instant::now() + deadline;
        let mut last_seen: Option<u64> = None;
        while Instant::now() < stop {
            let mut leaders = Vec::new();
            for n in self.nodes.values() {
                leaders.push(leader_id_for(n).await);
            }
            if let Some(first) = leaders.first().copied().flatten() {
                if leaders.iter().all(|l| *l == Some(first)) {
                    return Ok(());
                }
                last_seen = Some(first);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Err(format!(
            "no consistent leader after {:?}; last partial sighting: {last_seen:?}",
            deadline
        ))
    }

    /// Bucket name guaranteed unique across scenarios — use this in
    /// every `@multi-node` step that writes data.
    pub fn unique_bucket(&self) -> String {
        format!("bdd-{}", uuid::Uuid::new_v4().simple())
    }
}

// ---------------------------------------------------------------------------
// Process-level singleton
// ---------------------------------------------------------------------------

static CLUSTER: OnceCell<Arc<Mutex<ClusterHarness>>> = OnceCell::const_new();

/// Acquire the shared cluster handle. First caller pays the ~5-15s
/// startup; subsequent callers get the cached `Arc` immediately.
pub async fn acquire_cluster() -> Result<Arc<Mutex<ClusterHarness>>, String> {
    CLUSTER
        .get_or_try_init(|| async {
            ClusterHarness::start()
                .await
                .map(|c| Arc::new(Mutex::new(c)))
        })
        .await
        .cloned()
}

// ---------------------------------------------------------------------------
// Spawn helpers
// ---------------------------------------------------------------------------

fn spawn_node(
    binary: &Path,
    node_id: u64,
    ports: &ServerPorts,
    raft_peers_env: &str,
    fabric_peers_env: &str,
    bootstrap: bool,
) -> Result<NodeHandle, String> {
    let data_dir = tempfile::tempdir().map_err(|e| format!("tempdir for node-{node_id}: {e}"))?;
    let child = spawn_with_env(
        binary,
        node_id,
        ports,
        raft_peers_env,
        fabric_peers_env,
        bootstrap,
        data_dir.path(),
    )?;
    Ok(NodeHandle {
        node_id,
        ports: ports.clone(),
        data_dir,
        process: child,
        s3_base: format!("http://127.0.0.1:{}", ports.s3_http),
        http: reqwest::Client::new(),
    })
}

fn spawn_with_env(
    binary: &Path,
    node_id: u64,
    ports: &ServerPorts,
    raft_peers_env: &str,
    fabric_peers_env: &str,
    bootstrap: bool,
    data_dir: &Path,
) -> Result<Child, String> {
    let mut cmd = Command::new(binary);
    cmd.env_clear()
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
        .env("KISEKI_RAFT_ADDR", format!("127.0.0.1:{}", ports.raft))
        .env("KISEKI_DATA_DIR", data_dir)
        .env("KISEKI_NODE_ID", node_id.to_string())
        .env("KISEKI_RAFT_PEERS", raft_peers_env)
        .env("KISEKI_FABRIC_PEERS", fabric_peers_env)
        .env("KISEKI_BOOTSTRAP", if bootstrap { "true" } else { "false" })
        .env("KISEKI_ALLOW_PLAINTEXT_NFS", "true")
        .env("KISEKI_INSECURE_NFS", "true")
        .env("RUST_LOG", "warn")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        // Both streams to /dev/null. We previously piped stderr but
        // never drained it, so the kernel's ~64 KiB pipe buffer would
        // fill mid-test under RUST_LOG=warn and block the child's
        // next write — causing seemingly-random "connection refused"
        // failures on its admin port. If you need child logs, set
        // KISEKI_HARNESS_LOG_DIR and tee stderr there.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    install_pdeathsig(&mut cmd);
    cmd.spawn().map_err(|e| {
        format!(
            "spawn kiseki-server (node-{node_id}) at {}: {e}",
            binary.display()
        )
    })
}

#[cfg(target_os = "linux")]
fn install_pdeathsig(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Put each spawned server in its own session+process-group via
    // setsid(2). On clean shutdown the harness's Drop kills each child
    // explicitly. We previously tried prctl(PR_SET_PDEATHSIG, SIGTERM)
    // here but that fires on the *spawning thread's* exit, not the
    // parent process — and tokio scenarios end their workers between
    // batched scenarios, so the children received SIGTERM mid-test.
    // setsid alone leaks children on `kill -9` of cargo test (they
    // reparent to init), but at least passes batched runs reliably.
    // SAFETY: pre_exec runs in the forked child between fork() and
    // execve(); setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn install_pdeathsig(_cmd: &mut Command) {
    // No-op on non-Linux. Children may outlive the test binary on
    // crash, but tempdir cleanup at least removes their state.
}

// ---------------------------------------------------------------------------
// Readiness probes
// ---------------------------------------------------------------------------

/// Block until `/health` returns 200 on the metrics port. The metrics
/// HTTP server is the last thing the runtime starts, so once it's up
/// the gRPC + S3 listeners are too.
async fn wait_for_admin(node: &NodeHandle, deadline: Duration) -> Result<(), String> {
    let url = node.admin_url("health");
    let stop = Instant::now() + deadline;
    while Instant::now() < stop {
        if let Ok(resp) = node.http.get(&url).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(format!(
        "node-{} admin endpoint never reached ready: {url}",
        node.node_id
    ))
}

async fn leader_id_for(node: &NodeHandle) -> Option<u64> {
    let url = node.admin_url("cluster/info");
    let resp = node.http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("leader_id")?.as_u64()
}

// ---------------------------------------------------------------------------
// Binary lookup (mirrors ServerHarness::find_binary so we don't depend
// on it being public).
// ---------------------------------------------------------------------------

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
