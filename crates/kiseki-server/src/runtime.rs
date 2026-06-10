//! Runtime composition — wires all contexts and starts gRPC servers.

use std::net::SocketAddr;
use std::sync::Arc;

use kiseki_advisory::budget::BudgetConfig;
use kiseki_advisory::grpc::AdvisoryGrpc;
use kiseki_audit::AuditOps;
use kiseki_control::grpc::ControlGrpc;
use kiseki_keymanager::grpc::KeyManagerGrpc;
use kiseki_log::grpc::LogGrpc;
use kiseki_proto::v1::control_service_server::ControlServiceServer;
use kiseki_proto::v1::key_manager_service_server::KeyManagerServiceServer;
use kiseki_proto::v1::log_service_server::LogServiceServer;
use kiseki_proto::v1::workflow_advisory_service_server::WorkflowAdvisoryServiceServer;
use kiseki_view::ViewOps;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use crate::config::{ServerConfig, TlsFiles};

/// Pick the DS uaddrs the MDS will advertise via GETDEVICEINFO.
///
/// Precedence:
/// 1. `KISEKI_DS_PEERS` (explicit per-node DS endpoints) — required
///    for localhost-multi-node where each node binds a distinct
///    ephemeral DS port.
/// 2. `raft_peers` host-substitution + `ds_addr.port()` — the
///    containerized/hostnamed case where every node uses the same
///    DS port (typically 2052).
/// 3. Single-node fallback: when no peers are known but the local
///    node has a `ds_addr` configured, advertise *that* address.
///    Without this branch single-node clusters with an ephemeral
///    DS port (e.g. the profile harness) silently advertised the
///    hard-coded `127.0.0.1:2052` from `MdsLayoutConfig::default()`,
///    so every pNFS read got `Connection refused`.
fn compute_storage_ds_addrs(
    ds_peers: &[(u64, String)],
    raft_peers: &[(u64, String)],
    ds_addr: Option<SocketAddr>,
) -> Vec<String> {
    if !ds_peers.is_empty() {
        return ds_peers.iter().map(|(_, addr)| addr.clone()).collect();
    }
    if !raft_peers.is_empty() {
        let ds_port = ds_addr.map_or(2052, |a| a.port());
        return raft_peers
            .iter()
            .map(|(_, addr)| {
                let host = addr.split(':').next().unwrap_or(addr);
                format!("{host}:{ds_port}")
            })
            .collect();
    }
    // Single-node: the local node *is* the storage DS.
    if let Some(addr) = ds_addr {
        return vec![addr.to_string()];
    }
    Vec::new()
}

/// Build the `NodeId → "host:port"` map the S3 router uses to fill
/// `307 Location:` headers. The local S3 port is substituted into
/// each `raft_peers` entry's host — same pattern as
/// `compute_storage_ds_addrs` for the DS uaddrs — assuming the
/// cluster uses a uniform per-node S3 port (the normal container
/// deployment). Localhost-multi-node deployments (each node on a
/// distinct ephemeral S3 port) need a separate peer-list env var;
/// follow-up. Empty `raft_peers` → empty map → the 307 path
/// gracefully falls back to 503 + `Retry-After`.
pub(crate) fn compute_s3_peer_addrs(
    raft_peers: &[(u64, String)],
    local_s3_addr: SocketAddr,
) -> std::collections::HashMap<u64, String> {
    let s3_port = local_s3_addr.port();
    raft_peers
        .iter()
        .map(|(node_id, raft_peer_addr)| {
            let host = raft_peer_addr.split(':').next().unwrap_or(raft_peer_addr);
            (*node_id, format!("{host}:{s3_port}"))
        })
        .collect()
}

/// Parse `KISEKI_PEER_DATA_ADDRS=id=host:port,id=host:port,…` into
/// `(node_id, "host:port")` pairs for the native proxy fallback (#103).
/// Malformed entries (no `=`, non-numeric id) are skipped — the caller
/// falls back to uniform-port derivation if the result is empty.
/// Mirrors the `KISEKI_DS_PEERS` shape used for the fabric.
fn parse_peer_data_addrs(env: Option<&str>) -> Vec<(u64, String)> {
    let Some(raw) = env else {
        return Vec::new();
    };
    raw.split(',')
        .filter_map(|entry| {
            let (id, addr) = entry.trim().split_once('=')?;
            let id = id.trim().parse::<u64>().ok()?;
            let addr = addr.trim();
            if addr.is_empty() {
                return None;
            }
            Some((id, addr.to_owned()))
        })
        .collect()
}

/// Pick the per-node identity source for the at-rest key store
/// (Phase 14e). Precedence: SPIFFE > mTLS > file-in-data-dir.
///
/// Returns `Err` only if every source is unavailable — which shouldn't
/// happen here because the file fallback always succeeds when the
/// data dir exists.
fn select_node_identity_or_die(
    cfg: &ServerConfig,
    data_dir: &std::path::Path,
) -> Result<Box<dyn kiseki_keymanager::node_identity::NodeIdentitySource>, Box<dyn std::error::Error>>
{
    use kiseki_keymanager::node_identity::{select_node_identity, NodeIdentityInputs};
    let mtls_key = cfg.tls.as_ref().map(|t| t.key_path.as_path());
    select_node_identity(&NodeIdentityInputs {
        spiffe_path: cfg.spiffe_socket.as_deref(),
        mtls_key_path: mtls_key,
        data_dir: Some(data_dir),
    })
    .ok_or_else(|| "no node identity source available".into())
}

/// Build a tonic `ServerTlsConfig` from PEM files.
fn build_tls(files: &TlsFiles) -> Result<ServerTlsConfig, Box<dyn std::error::Error>> {
    let ca_pem = std::fs::read(&files.ca_path)
        .map_err(|e| format!("read CA {}: {e}", files.ca_path.display()))?;
    let cert_pem = std::fs::read(&files.cert_path)
        .map_err(|e| format!("read cert {}: {e}", files.cert_path.display()))?;
    let key_pem = std::fs::read(&files.key_path)
        .map_err(|e| format!("read key {}: {e}", files.key_path.display()))?;

    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(&cert_pem, &key_pem))
        .client_ca_root(Certificate::from_pem(&ca_pem));

    Ok(tls)
}

/// Build a per-peer fabric `Channel` to a peer's data-path gRPC. The
/// peer endpoint is host:port; this function strips the colon-port,
/// rewrites the URI scheme to `https://` (or `http://` for plaintext),
/// and applies mTLS using the shared cluster CA + this node's identity
/// when `tls_files` is `Some`. Phase 16a step 12.
fn build_fabric_channel(
    peer_addr: &str,
    tls_files: Option<&TlsFiles>,
) -> Result<tonic::transport::Channel, Box<dyn std::error::Error + Send + Sync>> {
    use tonic::transport::{ClientTlsConfig, Endpoint};

    let scheme = if tls_files.is_some() { "https" } else { "http" };
    // Default the SNI to the host portion of peer_addr; the
    // shared-cluster cert lists all node DNS names as SANs (see
    // tests/e2e/gen-tls-certs.sh).
    let host = peer_addr
        .split(':')
        .next()
        .ok_or("peer addr missing host")?
        .to_owned();
    let uri: tonic::transport::Uri = format!("{scheme}://{peer_addr}")
        .parse()
        .map_err(|e| format!("peer URI parse: {e}"))?;

    let mut endpoint = Endpoint::from(uri)
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(5))
        // HTTP/2 flow-control windows. Tonic / hyper default is
        // 64 KiB per stream — for kiseki's 64+ MiB fabric envelopes
        // that's 1024+ WINDOW_UPDATE round-trips per call. On a
        // cross-AZ link with ~1 ms RTT that adds ≥1 s of pure
        // bookkeeping latency. The 2026-05-03 GCP transport-profile
        // run measured 2 s avg fabric PutFragment on a 28 Gbps
        // wire, with 71 % of S3 PUTs hitting `quorum_lost` because
        // peer ops missed the 5 s timeout. 16 MiB stream window
        // collapses the round-trip count for a 64 MiB body to 4.
        .initial_stream_window_size(16 * 1024 * 1024)
        .initial_connection_window_size(32 * 1024 * 1024);

    if let Some(files) = tls_files {
        let ca_pem = std::fs::read(&files.ca_path)
            .map_err(|e| format!("read fabric CA {}: {e}", files.ca_path.display()))?;
        let cert_pem = std::fs::read(&files.cert_path)
            .map_err(|e| format!("read fabric cert {}: {e}", files.cert_path.display()))?;
        let key_pem = std::fs::read(&files.key_path)
            .map_err(|e| format!("read fabric key {}: {e}", files.key_path.display()))?;
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&ca_pem))
            .identity(Identity::from_pem(&cert_pem, &key_pem))
            .domain_name(host);
        endpoint = endpoint
            .tls_config(tls)
            .map_err(|e| format!("fabric TLS config: {e}"))?;
    }

    let channel = endpoint.connect_lazy(); // lazy: failed peers don't block startup
    Ok(channel)
}

/// Map a Raft peer address to the fabric endpoint on the same node.
///
/// `cfg.raft_peers` carries `host:RAFT_PORT` entries (the addresses
/// the consensus log uses), but `ClusterChunkService` binds to
/// `cfg.data_addr`'s port — a different gRPC server on the same
/// host. Without this remapping, fabric `PutFragment` fan-out lands
/// on the Raft port and quorum collapses to leader-only.
///
/// Strategy: split off the trailing `:port`, keep everything before
/// it as the host (preserving bracketed IPv6 literals), and append
/// `:data_port`. Returns the original string if it doesn't carry a
/// port (defensive — the caller logs and skips).
fn fabric_addr_from_raft_peer(raft_peer: &str, data_port: u16) -> String {
    raft_peer.rsplit_once(':').map_or_else(
        || raft_peer.to_owned(),
        |(host, _port)| format!("{host}:{data_port}"),
    )
}

/// 2026-06-01: read the cluster CA + node cert + key off disk and
/// build a `rustls::ServerConfig` for the fabric TCP-framed listener.
/// Delegates to `kiseki_transport::TlsConfig::server_config` so the
/// rustls posture matches the cluster's mTLS chain (same root the
/// gRPC fabric service trusts). Returns the rustls error verbatim
/// so an operator sees the same diagnostic regardless of which
/// transport asked.
fn build_fabric_tls_server_config(tls_files: &TlsFiles) -> Result<rustls::ServerConfig, String> {
    let ca_pem = std::fs::read(&tls_files.ca_path)
        .map_err(|e| format!("read CA {}: {e}", tls_files.ca_path.display()))?;
    let cert_pem = std::fs::read(&tls_files.cert_path)
        .map_err(|e| format!("read cert {}: {e}", tls_files.cert_path.display()))?;
    let key_pem = std::fs::read(&tls_files.key_path)
        .map_err(|e| format!("read key {}: {e}", tls_files.key_path.display()))?;
    kiseki_transport::TlsConfig::server_config(&ca_pem, &cert_pem, &key_pem)
        .map_err(|e| format!("rustls server_config: {e}"))
}

/// 2026-06-01: derive the TCP-framed-postcard fabric address from the
/// gRPC fabric address by adding a fixed port offset. ADR-042 §2.2
/// reserves the gRPC port for back-compat; the TCP-framed listener
/// binds a separate port (default = `data_port + 50`). Same host,
/// different port — so the existing per-peer hostname derivation
/// (gRPC) feeds straight into this with no new resolver step.
fn derive_tcp_fabric_addr(grpc_fabric_addr: &str, offset: u16) -> String {
    grpc_fabric_addr.rsplit_once(':').map_or_else(
        || grpc_fabric_addr.to_owned(),
        |(host, port_s)| {
            let port: u16 = port_s.parse().unwrap_or(0);
            format!("{host}:{}", port.saturating_add(offset))
        },
    )
}

/// ADR-030 §3 multi-voter aggregation: convert a Raft-peer address
/// (`host:raft_port`) into the canonical metrics scrape URL. The
/// metrics port follows `KISEKI_METRICS_PORT` (default 9090, which is
/// the prom convention used elsewhere in the codebase). The Raft port
/// itself doesn't serve `/metrics` — the metrics server runs on a
/// distinct port managed by `run_metrics_server`.
fn metrics_url_from_raft_peer(raft_peer: &str) -> String {
    let metrics_port: u16 = std::env::var("KISEKI_METRICS_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9090);
    let host = raft_peer
        .rsplit_once(':')
        .map_or(raft_peer, |(host, _)| host);
    format!("http://{host}:{metrics_port}/metrics")
}

/// Parse the `kiseki_node_metadata_capacity_bytes{kind="soft_limit"}`
/// gauge from a Prometheus text exposition payload. Returns the
/// integer byte value, or `None` if the line isn't present (the peer
/// hasn't run the Phase 2 capacity emitter yet — older binary or
/// mid-startup).
fn parse_node_metadata_soft_limit_from_metrics(text: &str) -> Option<u64> {
    const METRIC: &str = "kiseki_node_metadata_capacity_bytes";
    for line in text.lines() {
        let line = line.trim_start();
        if !line.starts_with(METRIC) {
            continue;
        }
        // Quickly skip TYPE/HELP comment lines and prefix-extended
        // metric names (e.g. `..._sum`).
        if line.starts_with('#') {
            continue;
        }
        let after = &line[METRIC.len()..];
        // The labels start with `{`; bail on any other prefix
        // (a `_total` suffixed counter, e.g.).
        if !after.starts_with('{') {
            continue;
        }
        let close = after.find('}')?;
        let labels = &after[1..close];
        if !labels.contains("kind=\"soft_limit\"") {
            continue;
        }
        let value = after[close + 1..].split_whitespace().next()?;
        if let Ok(v) = value.parse::<u64>() {
            return Some(v);
        }
        // Some Prom encoders emit `1.23e9` floats; fall back to f64.
        if let Ok(v) = value.parse::<f64>() {
            if v.is_finite() && v >= 0.0 {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let truncated = v as u64;
                return Some(truncated);
            }
        }
    }
    None
}

/// `NamespaceProvisioner` impl backed by the control-plane Raft group.
///
/// Called from the gateway's `ensure_namespace_exists` on the first
/// touch of a fresh namespace (ADR-033 §1). Submits a
/// `ControlCommand::CreateNamespace` that hydrates the
/// `NamespaceShardMapStore` on every node via the state machine's
/// apply hook. Subsequent writes route by `hashed_key` range.
///
/// Idempotent: re-submission of an already-existing namespace is
/// caught both in `ensure_namespace_exists`'s `shard_map` fast-path
/// check and (defensively) by the state machine's `apply_command`
/// which returns the existing shard count.
struct ControlPlaneProvisioner {
    ctrl_store: Arc<crate::cluster_control::OpenRaftControlStore>,
    raft_store: Arc<kiseki_log::RaftShardStore>,
    active_nodes: Vec<kiseki_common::ids::NodeId>,
    raft_runtime: tokio::runtime::Handle,
}

#[async_trait::async_trait]
impl kiseki_gateway::mem_gateway::NamespaceProvisioner for ControlPlaneProvisioner {
    async fn provision(
        &self,
        namespace_id: kiseki_common::ids::NamespaceId,
        tenant_id: kiseki_common::ids::OrgId,
    ) -> Result<(), kiseki_gateway::error::GatewayError> {
        // First-touch defaults to N=1 (single-shard). The system path
        // (S3 buckets created by casual `aws s3 cp`, NFS / FUSE root
        // mount) is overwhelmingly sequential — one client, one key
        // at a time — and pays no benefit from the formula's `3×N`
        // fanout while paying its per-shard apply/hydrator overhead.
        // Per ADR-033 §1, the formula targets **tenant-admin-created
        // namespaces** with parallel-write workloads; those clients
        // route through `POST /admin/topology/namespaces` (which
        // accepts an explicit `shards: u32`), not through first-touch.
        let n: u32 = 1;
        let shards: Vec<crate::cluster_control::commands::ShardRecord> =
            kiseki_control::shard_topology::compute_shard_ranges(n, &self.active_nodes)
                .into_iter()
                .map(|r| crate::cluster_control::commands::ShardRecord {
                    shard_id: r.shard_id,
                    range_start: r.range_start,
                    range_end: r.range_end,
                    leader_node: r.leader_node,
                })
                .collect();
        let shard_ids: Vec<kiseki_common::ids::ShardId> =
            shards.iter().map(|s| s.shard_id).collect();
        let cmd = crate::cluster_control::ControlCommand::CreateNamespace {
            namespace_id: namespace_id.0.to_string(),
            tenant_id,
            shards,
            // First-touch namespaces (S3 bucket auto-create, NFS /
            // FUSE root) genuinely have no policy — defaults are the
            // correct fidelity here, matching what
            // `ensure_namespace_exists` registers locally.
            fidelity: crate::cluster_control::NamespaceFidelity::default(),
        };
        // The data path may be on the host tokio runtime; openraft's
        // client_write must run on the Raft runtime. Cross to the
        // Raft runtime so we don't deadlock when the leader's apply
        // pipeline parks on the host runtime's reactor.
        let ctrl = Arc::clone(&self.ctrl_store);
        let store = Arc::clone(&self.raft_store);
        // submit_and_wait_for_voters waits for all voters to receive
        // the CreateNamespace log entry — the apply hook will have
        // fired (or will fire imminently) on every node. After that
        // the *control-plane leader* (the node whose `submit`
        // returned `Ok`) initializes membership on each fresh
        // per-shard Raft group so writes can land. Followers learn
        // membership through AppendEntries.
        //
        // Earlier this branch was gated on `is_bootstrap` (the
        // `KISEKI_BOOTSTRAP=true` flag), under the assumption that
        // the bootstrap node was always the control-plane Raft
        // leader. That breaks the moment leadership rotates to any
        // other node: the bootstrap's `submit` returns
        // `ForwardToLeader` (Err, so the initialize branch is
        // skipped), and the actual leader has `is_bootstrap=false`
        // (so it also skips). The new per-shard group ends up
        // created on every node via the apply hook but with no
        // membership initialized — no election, no leader, every
        // write 5xx's with "leader unavailable" until a node
        // restart. CI tracing pinned this 2026-05-19: control-plane
        // term T3 had node-2 as leader; node-1's provision returned
        // ForwardToLeader, the test retried via node-2, node-2
        // submitted successfully but skipped initialize because
        // `is_bootstrap=false`.
        //
        // The leader is the only node whose `submit` can succeed
        // (openraft maps follower writes to `ForwardToLeader` Err),
        // so gating on `resp.is_ok()` alone is equivalent to "I am
        // the control-plane leader" — which is the right condition
        // for who-initializes-the-new-shard.
        let res = self
            .raft_runtime
            .spawn(async move {
                let resp = ctrl
                    .submit_and_wait_for_voters(cmd, std::time::Duration::from_secs(10))
                    .await;
                // GH #101: the per-shard Raft membership is initialized by
                // each shard's assigned `leader_node` via the control-plane
                // apply hook (`ShardStoreApplyHook::on_create_namespace`),
                // not here — that is what distributes leadership across
                // nodes. This path only waits for the result below.
                //
                // Completing the namespace-creation contract: `provision`
                // must not return success until each fresh shard has an
                // elected leader. Without this, callers like S3
                // `CreateBucket` would receive `200 OK` for a bucket
                // that is not yet writable — the subsequent `PutObject`
                // races the per-shard Raft election and 5xx's with
                // `leader unavailable: ShardId(...)`. Same contract
                // logic for any namespace-creating path (admin API,
                // future first-touch auto-provisioning): when this
                // function returns success, the namespace is fully
                // ready for writes.
                //
                // openraft's `Raft::initialize()` returns once the
                // configuration is recorded locally but BEFORE the
                // election has converged across the cluster.
                // `shard_health` returns `ShardNotFound` on a voter
                // that hasn't applied the CreateNamespace yet, so the
                // same poll naturally absorbs the apply-lag race too.
                // 30 s upper bound matches the BDD harness's outer
                // retry budget — if consensus genuinely hasn't
                // happened in 30 s, something is wrong and the
                // failure should surface, not be hidden by a longer
                // wait.
                if resp.is_ok() {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    for shard_id in &shard_ids {
                        while std::time::Instant::now() < deadline {
                            if let Ok(info) =
                                kiseki_log::LogOps::shard_health(&*store, *shard_id).await
                            {
                                if info.leader.is_some() {
                                    break;
                                }
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                }
                resp
            })
            .await
            .map_err(|e| {
                kiseki_gateway::error::GatewayError::Upstream(format!("raft join failed: {e}"))
            })?;
        match res {
            Ok(_) => Ok(()),
            Err(e) => {
                // The control-plane state machine is idempotent: a
                // race-loser's submit gets `AlreadyExists` from the
                // local store. Treat that as success — the topology
                // is committed.
                let msg = e.to_string();
                if msg.contains("AlreadyExists") || msg.contains("already exists") {
                    return Ok(());
                }
                Err(kiseki_gateway::error::GatewayError::Upstream(format!(
                    "namespace provision: {e}"
                )))
            }
        }
    }
}

/// GH #192: `NamespaceRegistrar` impl that bridges control-plane
/// `CreateNamespace` applies (live submit, restart log replay,
/// snapshot install) into the gateway's volatile namespace registry.
///
/// Mirrors the local-registration half of what the admin handler
/// (`api_create_sharded_namespace`, issue #93 block) and
/// `ensure_namespace_exists` do: register the namespace in the
/// composition store + the gateway's lock-free `namespace_meta`
/// cache. Unlike those paths it does NOT emit a `NamespaceCreate`
/// delta — the apply hook fires on EVERY node, so each node
/// registers locally and no cross-node replication is needed (and
/// re-emitting on every restart replay would be wrong).
///
/// PR #232: the command carries the full creation-time fidelity
/// (`tier_policy`, `size_band_pools`, flags), so every registration
/// this bridge performs — including restart log replay and snapshot
/// install — restores the creator's exact policy, not defaults.
pub(crate) struct GatewayNamespaceRegistrar {
    pub(crate) gw: Arc<kiseki_gateway::InMemoryGateway>,
}

impl crate::cluster_control::NamespaceRegistrar for GatewayNamespaceRegistrar {
    fn register_namespace(
        &self,
        namespace_id: &str,
        tenant_id: kiseki_common::ids::OrgId,
        fidelity: &crate::cluster_control::NamespaceFidelity,
    ) {
        // The control-plane keys namespaces by string id; the
        // composition store by `NamespaceId(Uuid)`. The CLI / bench /
        // admin API pass UUID strings; legacy non-UUID ids skip
        // gateway registration — same contract as the admin handler.
        let Ok(ns_uuid) = uuid::Uuid::parse_str(namespace_id) else {
            return;
        };
        let ns_id = kiseki_common::ids::NamespaceId(ns_uuid);
        // Keep-first. This check-then-insert is not atomic against
        // the admin handler's concurrent `add_namespace` (the
        // responder fires before the apply hook, so the handler can
        // resume mid-race) — but both writers now carry the SAME
        // creation-time fidelity from the command, so either ordering
        // converges on the identical record. Keep-first additionally
        // preserves post-create policy updates (the size-band-pools /
        // tier-policy admin routes mutate the live registration
        // in-place) against a late snapshot-install replay.
        if self.gw.compositions_handle().namespace(ns_id).is_some() {
            return;
        }
        // Primary/fallback shard pointer — same convention as the
        // admin handler and `ensure_namespace_exists`. Actual write
        // routing goes through the shard_map (`route_to_shard`),
        // which the control-plane apply hydrates independently.
        let ns = fidelity.to_namespace(
            ns_id,
            tenant_id,
            kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
        );
        self.gw.add_namespace_sync(ns);
        tracing::info!(
            namespace_id,
            tenant_id = %tenant_id.0,
            "gateway namespace registry: re-hydrated from control-plane topology \
             with creation-time fidelity (GH #192 / PR #232)",
        );
    }
}

/// Recursive directory size in bytes. Tolerates I/O errors (returns
/// the partial sum). Used by the periodic composition-store gauge —
/// fjall is a keyspace directory rather than a single file.
fn dir_size_recursive(path: &std::path::Path) -> u64 {
    let Ok(read) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total: u64 = 0;
    for entry in read.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total = total.saturating_add(dir_size_recursive(&entry.path()));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Run the main data-path server.
///
/// `workflow_table` is shared with the advisory runtime — see
/// `main.rs`. The data-path gateway uses it to validate the
/// `x-kiseki-workflow-ref` header (ADR-021 §3.b / I-WA1).
#[allow(clippy::too_many_lines)]
pub async fn run_main(
    cfg: ServerConfig,
    workflow_table: std::sync::Arc<std::sync::Mutex<kiseki_advisory::WorkflowTable>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // --- Context construction ---

    // System disk detection (ADR-030). Boot-time snapshot — metric
    // emission happens after the metrics handle exists; here we just
    // log + warn so operators see the device class in the boot trace.
    let boot_capacity = cfg.data_dir.as_ref().map(|dir| {
        let capacity = crate::system_disk::compute_capacity(
            dir,
            cfg.meta_soft_limit_pct,
            cfg.meta_hard_limit_pct,
        );
        crate::system_disk::warn_if_rotational(capacity.media_type);
        tracing::info!(
            media_type = ?capacity.media_type,
            total_gb = capacity.total_bytes / (1024 * 1024 * 1024),
            soft_limit_gb = capacity.soft_limit_bytes / (1024 * 1024 * 1024),
            budget_gb = capacity.small_file_budget_bytes / (1024 * 1024 * 1024),
            cluster_max_files_estimate =
                capacity.soft_limit_bytes / crate::system_disk::PER_FILE_METADATA_FOOTPRINT_BYTES,
            "system disk detected (ADR-030 metadata-role device)",
        );
        capacity
    });

    // Node identity for multi-node Raft.
    if cfg.node_id > 0 {
        tracing::info!(
            node_id = cfg.node_id,
            peers = cfg.raft_peers.len(),
            raft_addr = ?cfg.raft_addr,
            "node identity configured",
        );
    }

    // Key Manager: persistent (redb) if KISEKI_DATA_DIR set, otherwise in-memory.
    // Phase 14e: every persisted entry is wrapped in AES-GCM keyed off
    // a per-node identity (SPIFFE > mTLS > file fallback).
    let salt = cfg.node_id.to_be_bytes();
    let key_store = if let Some(ref dir) = cfg.data_dir {
        std::fs::create_dir_all(dir.join("keys")).ok();
        let identity = select_node_identity_or_die(&cfg, dir)?;
        tracing::info!(source = identity.kind(), "key store at-rest identity");
        let store = kiseki_keymanager::PersistentKeyStore::open(
            &dir.join("keys").join("epochs"),
            &*identity,
            &salt,
        )
        .map_err(|e| format!("persistent key store: {e}"))?;
        tracing::info!(
            epoch = store.health().current_epoch.unwrap_or(0),
            "key manager: persistent (fjall) ready",
        );
        store
    } else {
        // In-memory: use a process-scoped tempdir for both the
        // keyspace and the file-based node identity. Ephemeral by
        // design.
        let tmp = std::env::temp_dir().join(format!("kiseki-keys-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).ok();
        let identity = kiseki_keymanager::node_identity::FileIdentitySource::new(
            tmp.join("node-identity.key"),
        )
        .map_err(|e| format!("ephemeral node identity: {e}"))?;
        let store =
            kiseki_keymanager::PersistentKeyStore::open(&tmp.join("epochs"), &identity, &salt)
                .map_err(|e| format!("key store init: {e}"))?;
        tracing::info!(
            epoch = store.health().current_epoch.unwrap_or(0),
            "key manager: in-memory (ephemeral) ready",
        );
        store
    };
    let key_store_inner = Arc::new(key_store);

    // ADR-049 phase 5a continued: load the pointer file that the
    // PRIOR boot's phase5_boot wrote. From here through the chunk
    // store init, every fjall keyspace consults `boot_paths` for
    // its resolved path.
    //
    // First-boot path: no pointer file exists yet. We can't call
    // `phase5_boot::run` because the control-plane Raft isn't up yet.
    // Instead, `first_boot_local_resolve` does a *local* resolution
    // using only this node's inventory + the built-in default policy
    // (which every node carries identically), writes the pointer
    // file, and the same boot then opens fjall stores at the resolved
    // paths. The mounts a single node picks are stable regardless of
    // cluster size, so this is consistent with what `phase5_boot::run`
    // would produce once Raft is up — and the I-CP-Move check inside
    // `phase5_boot::run` later this boot would Ok on the freshly-
    // written pointer.
    //
    // A corrupt pointer file is treated as RefuseToOpen (Q23 / N-2):
    // refuse to start rather than silently relocating data.
    let boot_paths: crate::cluster_control::boot_paths::BootTierPaths = if let Some(ref dir) =
        cfg.data_dir
    {
        // Best-effort first-boot resolve. Errors are logged but
        // don't block startup — fall back to data_dir-relative
        // paths if the resolver fails (matches pre-ADR-049
        // behaviour on degenerate single-disk deployments).
        let tags_env = std::env::var("KISEKI_DEVICE_TAGS").unwrap_or_default();
        let tags = crate::cluster_control::device_discovery::DeviceTagMap::parse(&tags_env);
        match crate::cluster_control::phase5_boot::first_boot_local_resolve(
            kiseki_common::ids::NodeId(cfg.node_id),
            dir,
            &tags,
        ) {
            Ok(true) => tracing::info!("ADR-049 first-boot local resolve: pointer file written"),
            Ok(false) => {} // pointer already existed
            Err(e) => tracing::warn!(
                error = %e,
                "ADR-049 first-boot local resolve failed — falling back to data_dir paths"
            ),
        }
        crate::cluster_control::boot_paths::BootTierPaths::load(dir).map_err(|e| {
            format!(
                "ADR-049 boot tier-paths pointer file unreadable at {} — \
                     refuse to start (corrupt pointer file is not first-boot, \
                     see ADR-049 Q23 / N-2). Underlying error: {e}",
                dir.display(),
            )
        })?
    } else {
        crate::cluster_control::boot_paths::BootTierPaths::default()
    };
    if boot_paths.has_resolved() {
        tracing::info!("ADR-049 boot using pointer-resolved tier paths (kiseki-tier-paths.json)");
    } else {
        tracing::info!("ADR-049 boot: no pointer file — using data_dir-relative tier paths");
    }

    // Small object store for inline files (ADR-030).
    // Created before the log store so Raft state machines can use it.
    // Captures the flusher for the gateway `fsync_pending` hook when
    // group commit is on (#212) — registered further down next to the
    // chunk + composition hooks.
    let mut small_flusher_for_fsync: Option<kiseki_chunk::SmallObjectFlusher> = None;
    let small_store: Option<std::sync::Arc<kiseki_chunk::SmallObjectStore>> =
        if let Some(ref dir) = cfg.data_dir {
            // ADR-022 rev-5 (#129 unblock): SmallObjectStore moved
            // from redb to fjall. ADR-049 phase 5a continued: path
            // now comes from `boot_paths` — pointer-resolved mount
            // with `kiseki/small-object/` subdir when pointer
            // present, else falls back to `<data_dir>/small/objects`.
            let path = boot_paths.small_object(dir);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let store = kiseki_chunk::SmallObjectStore::open(&path)
                .map_err(|e| format!("small object store at {}: {e}", path.display()))?;
            // Group commit (#212): this store is hit once per inline
            // PUT on the gateway ack path AND per-entry in every
            // hosted shard's Raft apply — all through one fjall
            // journal whose per-write `SyncAll` serialized the node.
            // Relax to page-cache-per-write (I-L5 durability point;
            // intent quorum + retained-log replay recover the local
            // window) with a periodic fsync bounding power loss.
            // `KISEKI_SMALL_OBJECT_FLUSH_INTERVAL_MS=0` opts back
            // into strict per-write fsync.
            let flush_interval_ms = std::env::var("KISEKI_SMALL_OBJECT_FLUSH_INTERVAL_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(100);
            if flush_interval_ms > 0 {
                store.set_sync_per_write(false);
                let flusher = store.flusher();
                small_flusher_for_fsync = Some(flusher.clone());
                tokio::spawn(async move {
                    let mut tick =
                        tokio::time::interval(std::time::Duration::from_millis(flush_interval_ms));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        let f = flusher.clone();
                        let res = tokio::task::spawn_blocking(move || f.flush())
                            .await
                            .ok()
                            .and_then(Result::ok);
                        if res.is_none() {
                            tracing::warn!(
                                "small object store group-commit flush failed; retry next tick"
                            );
                        }
                    }
                });
            }
            tracing::info!(
                path = %path.display(),
                resolved = boot_paths.has_resolved(),
                group_commit = flush_interval_ms > 0,
                flush_interval_ms,
                "small object store: persistent (fjall, ADR-022 rev-5, ADR-049-routed)",
            );
            Some(std::sync::Arc::new(store))
        } else {
            None
        };

    // Metrics: built early so per-shard `RaftRpcListener` metrics
    // (ADR-041 §"Observability") are wired into the listener BEFORE
    // the first shard's lazy-init binds. Other subsystems (fabric,
    // gateway, etc.) thread `Arc<...>` clones from this same struct
    // throughout setup.
    let metrics = crate::metrics::KisekiMetrics::new();

    // ADR-030 amendment §"admin-driven metadata device role": seed
    // the per-node metadata-capacity gauges from the boot-time
    // snapshot so the cluster aggregator has a real value before the
    // first periodic tick (30 s default).
    if let Some(ref cap) = boot_capacity {
        crate::system_disk::emit_capacity_metrics(&metrics, cap);
    }

    // Observability opt-out for performance benchmarks. Defaults
    // ON for production deployments; the `infra/gcp/transport`
    // perf profile sets `KISEKI_OBSERVABILITY=off` so the GCP run
    // gets a clean baseline (no metric-record overhead, no
    // wrapper vtable dispatch on the data path). The hot-path
    // tracing spans are already at `level = "debug"`, so they
    // short-circuit at production INFO/WARN regardless of this
    // flag.
    let observability_enabled = std::env::var("KISEKI_OBSERVABILITY").map_or(true, |v| {
        !matches!(
            v.to_lowercase().as_str(),
            "off" | "0" | "false" | "disabled"
        )
    });
    if !observability_enabled {
        tracing::info!(
            "observability: wrappers disabled via KISEKI_OBSERVABILITY \
             — metric records on the LogOps / KeyManagerOps hot paths skipped"
        );
    }

    // Wrap the key store with `InstrumentedKeyManager` for
    // metric recording + tracing spans. When observability is
    // disabled, hand the bare `Arc<PersistentKeyStore>` to the
    // gRPC handler instead. `KeyManagerGrpc<T: ?Sized>` accepts
    // both the wrapper concrete type and the underlying trait
    // object so the choice is at runtime construction.
    let key_store: Arc<dyn kiseki_keymanager::KeyManagerOps> = if observability_enabled {
        Arc::new(kiseki_keymanager::InstrumentedKeyManager::new(
            key_store_inner,
            Arc::clone(&metrics.keymanager),
        ))
    } else {
        key_store_inner
    };

    // Log store: Raft (multi-node), persistent (redb), or in-memory.
    let bootstrap_shard = kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1));
    let bootstrap_tenant = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));

    // Holds the control-plane Raft store on multi-node deployments.
    // None on single-node / persistent / in-memory paths — those run
    // without cluster consensus on the namespace shard map (ADR-033
    // §4 only applies when there's a real cluster to coordinate
    // across).
    let mut cluster_control_store: Option<Arc<crate::cluster_control::OpenRaftControlStore>> = None;
    // Typed `RaftShardStore` handle for the storage admin RPC's
    // `initialize_shard` call after `RecordSplit` commits. Same
    // gating as `cluster_control_store` — only set on multi-node.
    let mut raft_shard_store_for_admin: Option<Arc<kiseki_log::RaftShardStore>> = None;
    // ADR-033 §5: gateway-readable shard map. Hydrated by the control-
    // plane apply (every node) so the gateway's `route_to_shard`
    // engages on the hot path. `None` on single-node smoke paths
    // where the control plane is also absent.
    let mut shard_map_store_for_gateway: Option<
        Arc<kiseki_control::shard_topology::NamespaceShardMapStore>,
    > = None;
    // Holds the concrete `ShardStoreApplyHook` Arc so the
    // composition-hydrator registry can be attached after the
    // gateway's `CompositionStore` is built. Multi-shard hydration
    // depends on this attachment: every shard the apply hook
    // registers also gets a hydrator task. None on single-node /
    // persistent / in-memory paths where there's no control plane.
    let mut apply_hook_for_registry: Option<Arc<crate::cluster_control::ShardStoreApplyHook>> =
        None;
    let log_store: Arc<dyn kiseki_log::LogOps + Send + Sync> = if cfg.node_id > 0
        && cfg.raft_peers.len() > 1
    {
        // Multi-node Raft: consensus-replicated log store.
        let peers: std::collections::BTreeMap<u64, String> =
            cfg.raft_peers.iter().cloned().collect();
        let raft_addr_str = cfg
            .raft_addr
            .map_or_else(|| "0.0.0.0:9300".to_owned(), |a| a.to_string());
        // ADR-047 decoupled-ack is THE write path for async-eligible
        // surfaces (S3, Native) — no capability gate. POSIX surfaces
        // (NFS, FUSE) keep the synchronous semantic via
        // `WriteSurface::is_async_ack_eligible` (ADR-013/014).
        let mut store =
            kiseki_log::RaftShardStore::new(cfg.node_id, peers.clone(), cfg.data_dir.clone());
        if let Some(ref ss) = small_store {
            store = store.with_inline_store(std::sync::Arc::clone(ss)
                as std::sync::Arc<dyn kiseki_common::inline_store::InlineStore>);
        }
        // ADR-049 phase 5a continued: tell RaftShardStore where each
        // per-shard FjallIntentStore should open. With the pointer
        // file present, this routes intents to the fast-tier mount
        // (typically NVMe) instead of the boot disk. Without it,
        // `intent_store_base == data_dir` and behaviour matches the
        // pre-ADR-049 path.
        if let Some(ref dir) = cfg.data_dir {
            store = store.with_intent_store_base(boot_paths.intent_store_base(dir));
        }
        // ADR-041 §"Observability": wire transport metrics BEFORE the
        // first create_shard so the lazy-init listener picks them up.
        store.set_transport_metrics(std::sync::Arc::clone(&metrics.raft_transport));

        // ADR-033 §4: bring up the multiplexed Raft listener BEFORE
        // any per-shard `create_shard` so the control-plane group can
        // share it. Returns the registry handle for the
        // control-plane group's `register_shard`.
        let registry = store.ensure_listener_started(&raft_addr_str);

        // All nodes in the cluster register the shard's per-shard
        // Raft handle with the multiplexed listener. Membership
        // initialization is a separate, explicit step (the
        // `initialize_shard` call below on the bootstrap node)
        // — see the ADR-033 §4 doc on `RaftShardStore::create_shard`.
        store.create_shard(
            bootstrap_shard,
            bootstrap_tenant,
            kiseki_common::ids::NodeId(cfg.node_id),
            kiseki_log::ShardConfig::default(),
            Some(&raft_addr_str),
        );
        // Bootstrap node initializes membership for the bootstrap
        // shard. Followers do not — they learn membership via
        // AppendEntries from the leader.
        if cfg.bootstrap {
            if let Err(e) = store.initialize_shard(bootstrap_shard) {
                tracing::warn!(
                    error = %e,
                    "bootstrap shard initialize_shard failed — \
                     cluster may need manual intervention",
                );
            }
        }
        tracing::info!(
            node_id = cfg.node_id,
            peers = cfg.raft_peers.len(),
            "log store: Raft",
        );

        let store_arc = Arc::new(store);

        // ADR-033 §4: control-plane Raft group. Built on the same
        // tokio runtime as the per-shard groups (mixing runtimes
        // deadlocks openraft). Apply hook bridges to RaftShardStore
        // so RecordSplit on every node creates the new shard's
        // per-shard Raft group locally — closes the cluster-wide
        // split gap surfaced by the @shard-mgmt BDD scenarios.
        let raft_rt = store_arc.raft_runtime_handle();
        // Hold the concrete `ShardStoreApplyHook` Arc so the
        // composition-hydrator registry can be attached later (after
        // the gateway's `CompositionStore` is built). The hook is
        // also passed into `OpenRaftControlStore::new` via the
        // `ApplyHook` trait object — same `Arc`, two references.
        let apply_hook_concrete = Arc::new(crate::cluster_control::ShardStoreApplyHook::new(
            Arc::clone(&store_arc),
            cfg.node_id,
            raft_rt.clone(),
        ));
        apply_hook_for_registry = Some(Arc::clone(&apply_hook_concrete));
        let apply_hook: Arc<dyn crate::cluster_control::ApplyHook> = apply_hook_concrete;
        // ADR-033 §5: shared shard map between (a) the control-plane
        // state machine's apply path (populates it on every node) and
        // (b) the gateway's `shard_map` field (consults it for
        // `route_to_shard`). Single source of truth — see
        // `ControlStateMachine::hydrate_shard_map`.
        let shard_map_store =
            Arc::new(kiseki_control::shard_topology::NamespaceShardMapStore::new());
        let shard_map_for_ctrl = Arc::clone(&shard_map_store);
        let peers_for_ctrl = peers.clone();
        let bootstrap_flag = cfg.bootstrap;
        let data_dir_for_ctrl = cfg.data_dir.clone();
        // Construct on the dedicated Raft runtime (block_on inside a
        // spawned thread so we don't nest runtimes — the same pattern
        // RaftShardStore uses for create_shard).
        let registry_for_ctrl = registry.clone();
        let ctrl_metrics = Arc::clone(&metrics.cluster_control);
        let ctrl_store_res: Result<
            Arc<crate::cluster_control::OpenRaftControlStore>,
            std::io::Error,
        > = std::thread::spawn(move || {
            raft_rt.block_on(async move {
                crate::cluster_control::OpenRaftControlStore::new(
                    cfg.node_id,
                    &peers_for_ctrl,
                    data_dir_for_ctrl.as_deref(),
                    &registry_for_ctrl,
                    apply_hook,
                    bootstrap_flag,
                    Some(ctrl_metrics),
                    Some(shard_map_for_ctrl),
                )
                .await
                .map(Arc::new)
            })
        })
        .join()
        .map_err(|_| "control-plane raft thread panicked".to_owned())?;

        let ctrl_store = ctrl_store_res.map_err(|e| format!("control-plane raft init: {e}"))?;
        tracing::info!(
            node_id = cfg.node_id,
            peers = cfg.raft_peers.len(),
            "control-plane Raft group: up (ADR-033 §4)",
        );

        // System namespaces are NOT seeded at boot anymore. Pre-#77
        // (1bb0db0) seeded both `bootstrap_namespace` and `default`
        // with 3×node_count shards. That was wrong: per ADR-033 §1
        // the formula is for **tenant-admin-created** namespaces, not
        // system internals — and applying it at boot forced sequential
        // workloads (BDD scenarios, casual `aws s3 cp`, bench
        // `default_ids()`) to pay 18× per-shard Raft apply + hydrator
        // overhead under load, with no parallelism benefit because the
        // workloads only touch one key at a time.
        //
        // Today:
        // * `bootstrap_shard` is created inline above (per-shard Raft
        //   group with full cluster voter set). NFS / FUSE / S3 GET-
        //   by-id paths use it via the legacy single-shard local
        //   registration in `ensure_namespace_exists`.
        // * Fresh tenant namespaces (created via
        //   `POST /admin/topology/namespaces` or first-touch through
        //   `ControlPlaneProvisioner`) get the formula's full
        //   `max(min(3×N, 64), 3)` shards. That's where the multi-
        //   shard fanout actually pays off — tenants run parallel
        //   workloads across many keys.
        //
        // Perf-test harnesses MUST create their own tenant namespace
        // (admin API + dedicated `--bench-namespace`) rather than
        // targeting `default`. See `infra/gcp/benchmarks/setup-shards.sh`
        // for the operator script.

        // ADR-049 phase 5a: now that the control-plane Raft is up,
        // run the boot helper. This: (1) discovers local devices via
        // `/proc/mounts`, (2) submits `UpsertNodeInventory` to the
        // catalog, (3) reads policy, (4) computes resolved tier
        // paths, (5) checks I-CP-Move against the prior pointer
        // file, (6) saves the pointer file with the resolved paths.
        // In phase 5a the resolved paths are advisory — actual
        // fjall stores (already opened above at `data_dir`-relative
        // paths) get reconciled by the phase-6 `storage migrate`
        // CLI command. The pointer file IS the I-CP-Move guard
        // for the NEXT boot.
        if let Some(ref data_dir) = cfg.data_dir {
            use crate::cluster_control::{device_discovery, phase5_boot};
            let tags_env = std::env::var("KISEKI_DEVICE_TAGS").unwrap_or_default();
            let tags = device_discovery::DeviceTagMap::parse(&tags_env);
            let catalog_for_phase5 = Arc::new(ctrl_store.state());
            let ctrl_for_submit = Arc::clone(&ctrl_store);
            let submit: phase5_boot::SubmitInventoryFn = Box::new(move |cmd| {
                Box::pin(async move {
                    ctrl_for_submit
                        .submit(cmd)
                        .await
                        .map(|_| ())
                        .map_err(|e| format!("UpsertNodeInventory submit: {e}"))
                })
            });
            let boot_inputs = phase5_boot::Phase5BootInputs {
                node_id: kiseki_common::ids::NodeId(cfg.node_id),
                data_dir: data_dir.as_path(),
                tags: &tags,
                catalog: catalog_for_phase5,
                submit_inventory: submit,
            };
            match phase5_boot::run(boot_inputs).await {
                Ok(resolved) => {
                    tracing::info!(
                        node_id = cfg.node_id,
                        devices = resolved.inventory.devices.len(),
                        "ADR-049 phase 5a boot: catalog populated, pointer file saved",
                    );
                }
                Err(phase5_boot::Phase5BootError::PathVersionMismatch {
                    tier_label,
                    prior,
                    resolved,
                    ..
                }) => {
                    return Err(format!(
                        "ADR-049 I-CP-Move tripped on tier {tier_label}: prior={} resolved={}. \
                         Run `kiseki-admin storage migrate --tier={tier_label} --node={}` before retrying.",
                        prior.display(),
                        resolved.display(),
                        cfg.node_id,
                    )
                    .into());
                }
                Err(e) => {
                    // Non-fatal at phase 5a: log + continue so the
                    // cluster doesn't refuse to boot under partial
                    // ADR-049 wiring. The pointer file gets written
                    // on the next successful boot once the issue is
                    // resolved (catalog reachable, policy set, etc.).
                    tracing::warn!(error = %e, "ADR-049 phase 5a boot non-fatal error (continuing)");
                }
            }
        }

        cluster_control_store = Some(ctrl_store);
        raft_shard_store_for_admin = Some(Arc::clone(&store_arc));
        // Hand the shard map to the gateway-construction block below.
        // Until `gw.set_shard_map(...)` is called the map sits idle —
        // the apply path still hydrates it (so admin RPC reads work),
        // but the data path doesn't consult it.
        shard_map_store_for_gateway = Some(Arc::clone(&shard_map_store));

        store_arc
    } else if let Some(ref dir) = cfg.data_dir {
        std::fs::create_dir_all(dir.join("raft")).ok();
        // Match the composition + chunk store durability knobs:
        // when KISEKI_RAFT_FLUSH_INTERVAL_MS is set, the Raft log
        // commits with PersistMode::Buffer and a periodic task
        // drives the fsync barrier. Multi-node deployments recover
        // the loss window via Raft replication on restart; single-
        // node deployments accept the documented per-knob loss
        // window in exchange for the throughput lift (PUT path was
        // fsync-bound at ~31 k op/s with sync-per-write).
        let raft_flush_interval_ms = std::env::var("KISEKI_RAFT_FLUSH_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        let log_path = dir.join("raft").join("log");
        let store = if raft_flush_interval_ms.is_some() {
            kiseki_log::persistent_store::PersistentShardStore::open_eventual(&log_path).await
        } else {
            kiseki_log::persistent_store::PersistentShardStore::open(&log_path).await
        }
        .map_err(|e| format!("persistent store: {e}"))?;
        if cfg.bootstrap {
            store.create_shard(
                bootstrap_shard,
                bootstrap_tenant,
                kiseki_common::ids::NodeId(1),
                kiseki_log::ShardConfig::default(),
            );
        }
        if let Some(interval_ms) = raft_flush_interval_ms {
            // Periodic Raft-log fsync. Same shape as the
            // composition store flusher: cheap memtable + WAL append
            // inline, durability barrier at a bounded cadence.
            // Borrow a clone of the underlying FjallLogStore for
            // the flush task.
            let fjall = store.fjall().clone();
            tracing::info!(
                interval_ms,
                "log store: persistent fjall (eventual durability)"
            );
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    if let Err(e) = fjall.flush() {
                        tracing::warn!(error = %e, "raft log periodic flush failed");
                    }
                }
            });
        } else {
            tracing::info!(path = %dir.display(), "log store: persistent (fjall)");
        }
        Arc::new(store)
    } else {
        let store = kiseki_log::MemShardStore::new();
        if cfg.bootstrap {
            store.create_shard(
                bootstrap_shard,
                bootstrap_tenant,
                kiseki_common::ids::NodeId(1),
                kiseki_log::ShardConfig::default(),
            );
        }
        tracing::info!("log store: in-memory (no persistence)");
        Arc::new(store)
    };

    // Wrap in `InstrumentedLogOps` so every LogOps call records to
    // `LogMetrics` and emits a tracing span. The wrapper is opaque
    // to callers (still `Arc<dyn LogOps>`); inner store paths that
    // need concrete types (`RaftShardStore::initialize_shard` for
    // SplitShard's leader-side init) hold their own typed `Arc`.
    //
    // Skipped when `KISEKI_OBSERVABILITY=off` so perf runs measure
    // bare LogOps throughput. See the explanatory comment near the
    // key-store wrap above.
    let log_store: Arc<dyn kiseki_log::LogOps + Send + Sync> = if observability_enabled {
        Arc::new(kiseki_log::InstrumentedLogOps::new(
            log_store,
            Arc::clone(&metrics.log),
        ))
    } else {
        log_store
    };

    if cfg.bootstrap {
        tracing::info!(
            shard = %bootstrap_shard.0,
            tenant = %bootstrap_tenant.0,
            "bootstrap: shard created for tenant",
        );
    }

    // Audit: in-memory store. Wrapped in Arc so the admin web UI /
    // `kiseki-admin audit query` can read events out of the same
    // store the runtime appends to (NFS-fallback opt-in, plaintext
    // banners, etc).
    let audit_store: Arc<kiseki_audit::AuditLog> = Arc::new(kiseki_audit::AuditLog::new());
    tracing::info!(events = audit_store.total_events(), "audit log: in-memory",);
    let audit_for_ui: Arc<dyn kiseki_audit::AuditOps + Send + Sync> =
        Arc::clone(&audit_store) as Arc<dyn kiseki_audit::AuditOps + Send + Sync>;

    // Metrics — built early so the cluster-fabric Arc<FabricMetrics>
    // can be threaded into the per-peer client wrappers below.
    // (KisekiMetrics is also constructed before the log-store
    // creation above so `raft_transport` metrics can be wired into
    // the multiplexed listener BEFORE its first `create_shard` call.
    // See `metrics.raft_transport` plumbing in the multi-node path.)
    let _ = &metrics;

    // Captures the chunk-store device handle + composition fjall
    // flusher when their respective group-commit modes are on, so
    // the gateway's `fsync_pending` hook can drive explicit fsyncs
    // from FUSE / NFS `fsync(2)` callers (POSIX-compliance for
    // the eventual-durability optimization). Declared up here so
    // both the chunk-store section below and the composition-store
    // section further down can write into the same scope.
    let mut comp_flusher_for_fsync: Option<kiseki_composition::persistent::FjallFlusher> = None;
    let mut chunk_device_for_fsync: Option<std::sync::Arc<dyn kiseki_block::DeviceBackend>> = None;

    // GH #39 wiring: the `io_uring` Cargo feature on `kiseki-block`
    // adds a `UringFileBackedDevice` alongside `FileBackedDevice`.
    // `kiseki_block::open_or_init_device` reads `KISEKI_IO_URING` and
    // returns the appropriate `Arc<dyn DeviceBackend>` (falling back
    // to `FileBackedDevice` when the feature isn't compiled in or
    // when the kernel rejects ring setup). We feed that handle into
    // `PersistentChunkStore::from_device` so the chunk store's data
    // plane actually points at the operator's choice — without
    // this, the env var was just a log line.

    // Local chunk store: persistent (raw block device) if KISEKI_DATA_DIR
    // set, otherwise in-memory. Wrapped via SyncBridge so it satisfies
    // AsyncChunkOps — the cluster fabric and the gateway both consume the
    // async surface (Phase 16a, D-7).
    let local_chunk_store: Arc<dyn kiseki_chunk::AsyncChunkOps> = if let Some(ref dir) =
        cfg.data_dir
    {
        std::fs::create_dir_all(dir.join("chunks")).ok();
        // ADR-022 rev-4: chunk meta moved off JSON to fjall. Path
        // is now a keyspace directory (no extension).
        // ADR-049 phase 5a continued: ChunkMeta tier resolved via
        // boot_paths. The raw chunk data still lives on the
        // `KISEKI_RAW_DEVICES` block-device pool — only the fjall
        // envelope/extent metadata moves to the resolved mount.
        let meta_path = boot_paths.chunk_meta(dir);
        if let Some(parent) = meta_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // ADR-024 + ADR-029: when the operator provisioned raw JBOD
        // data devices (`KISEKI_RAW_DEVICES`), open each as a
        // `RawBlockDevice` — probe-driven `O_DIRECT` for SSD/NVMe,
        // buffered for HDD — and span them into one capacity pool so
        // the node uses its full provisioned storage. Otherwise fall
        // back to a single file-backed device on `KISEKI_DATA_DIR`
        // (VMs / CI / single-node dev), sized from
        // `KISEKI_CHUNK_DEVICE_BYTES` (default 4 GiB). The hard-coded
        // 4 GiB file was GH #115 — it capped every node at 4 GiB and
        // silently ignored the provisioned NVMe.
        let device: std::sync::Arc<dyn kiseki_block::DeviceBackend> = if cfg.raw_devices.is_empty()
        {
            let dev_path = dir.join("chunks").join("data.dev");
            let size = std::env::var("KISEKI_CHUNK_DEVICE_BYTES")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(4 * 1024 * 1024 * 1024);
            tracing::info!(
                path = %dev_path.display(),
                size_gb = size / (1024 * 1024 * 1024),
                "chunk store: file-backed device (no KISEKI_RAW_DEVICES configured)",
            );
            kiseki_block::open_or_init_device(&dev_path, size)
                .map_err(|e| format!("chunk device backend init: {e}"))?
        } else {
            let mut members: Vec<std::sync::Arc<dyn kiseki_block::DeviceBackend>> = Vec::new();
            for dev_path in &cfg.raw_devices {
                match kiseki_block::RawBlockDevice::open_or_init(std::path::Path::new(dev_path)) {
                    Ok(d) => members.push(std::sync::Arc::new(d)),
                    Err(e) => {
                        tracing::error!(
                            device = dev_path,
                            error = %e,
                            "raw device open failed — excluding from pool",
                        );
                    }
                }
            }
            if members.is_empty() {
                return Err(format!(
                    "KISEKI_RAW_DEVICES set ({} device(s)) but none could be opened",
                    cfg.raw_devices.len()
                )
                .into());
            }
            let n_members = members.len();
            let pool = kiseki_block::DevicePool::new(members)
                .map_err(|e| format!("device pool init: {e}"))?;
            let total = kiseki_block::DeviceBackend::capacity(&pool).1;
            tracing::info!(
                devices = n_members,
                configured = cfg.raw_devices.len(),
                total_gb = total / (1024 * 1024 * 1024),
                "chunk store: raw block-device pool (KISEKI_RAW_DEVICES)",
            );
            std::sync::Arc::new(pool)
        };
        let store = kiseki_chunk::PersistentChunkStore::from_device(device, &meta_path)
            .map_err(|e| format!("persistent chunk store from_device: {e}"))?;
        // Receiver-side write_chunk phase histogram so /metrics shows
        // dedup_check / extent_io / save_meta / device_sync per write.
        store.set_write_phase_metric(Arc::new(
            metrics.chunk_persistent_write_phase_duration.clone(),
        ));
        // Group commit: per-write fsync was serializing concurrent
        // fabric receivers through the kernel sync. Disable inline
        // sync; the periodic flush task below keeps disk state fresh.
        // Crash safety: Raft replication ensures cross-node durability;
        // a single-node loss of ≤ flush_interval-ms of writes is
        // recovered by the under-replication scrub when the node
        // returns. See `PersistentChunkStore::sync_per_write` doc.
        store.set_sync_per_write(false);
        let device_for_flush = store.device_handle();
        // Capture for the gateway `fsync_pending` hook so explicit
        // `fsync(2)` from FUSE / NFS clients forces a real device
        // sync rather than waiting for the periodic task.
        chunk_device_for_fsync = Some(std::sync::Arc::clone(&device_for_flush));
        let flush_interval_ms = std::env::var("KISEKI_CHUNK_FLUSH_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(100);
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_millis(flush_interval_ms));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let device = std::sync::Arc::clone(&device_for_flush);
                let res = tokio::task::spawn_blocking(move || device.sync())
                    .await
                    .ok()
                    .and_then(Result::ok);
                if res.is_none() {
                    tracing::warn!("chunk store group-commit flush failed; will retry next tick",);
                }
            }
        });
        tracing::info!(
            path = %dir.display(),
            flush_interval_ms,
            "chunk store: persistent (raw block, group commit)",
        );
        Arc::new(kiseki_chunk::SyncBridge::new(store))
    } else {
        tracing::info!("chunk store: in-memory (no persistence)");
        Arc::new(kiseki_chunk::SyncBridge::new(
            kiseki_chunk::ChunkStore::new(),
        ))
    };

    // GH #36 — periodic chunk GC. `decrement_refcount` (called by the
    // gateway delete path on NFS REMOVE / S3 DELETE / FUSE unlink)
    // drops the refcount, but the actual device-extent reclamation
    // happens inside `AsyncChunkOps::gc()` which sweeps chunks whose
    // refcount has reached 0 (and have no retention holds) and calls
    // `device.free()` on each extent. Without this task the bitmap
    // allocator's free list bled out on the GCP 2026-05-15 perf
    // cluster — ~200 GB of cumulative writes filled every node despite
    // every fio file having been `rm`-ed between runs. `largest_free_
    // blocks=64` (256 KiB) was the smoking gun: the free list had
    // collapsed into singleton tail-extents because reclaimed space
    // never returned.
    //
    // Cadence: 60 s default, override via `KISEKI_CHUNK_GC_INTERVAL_S`.
    // Cheap when there's nothing to do — `gc()` is an in-memory
    // filter over the chunks map keyed on `refcount == 0`. Per-record
    // fjall removes only run for chunks that actually became
    // garbage.
    {
        let gc_store = Arc::clone(&local_chunk_store);
        let gc_interval_s = std::env::var("KISEKI_CHUNK_GC_INTERVAL_S")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(60);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(gc_interval_s));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the first immediate tick — let the server settle
            // before the first sweep. Subsequent ticks fire on schedule.
            tick.tick().await;
            loop {
                tick.tick().await;
                let freed = gc_store.gc().await;
                if freed > 0 {
                    tracing::info!(freed_chunks = freed, "chunk GC swept refcount-0 chunks");
                } else {
                    tracing::debug!("chunk GC: nothing to reclaim");
                }
            }
        });
        tracing::info!(gc_interval_s, "chunk store: periodic GC task spawned");
    }

    // Capacity + dedup metrics refresher (GH #115). Populates the
    // node-level storage gauges (used to be dead) and the per-device
    // breakdown gauge from `storage_stats()` + `device_breakdown()`.
    // These feed the cluster aggregator and `kiseki-admin capacity`.
    {
        let stats_store = Arc::clone(&local_chunk_store);
        let stats_device = chunk_device_for_fsync.clone();
        let g_used = metrics.storage_device_used_bytes.clone();
        let g_total = metrics.storage_device_total_bytes.clone();
        let g_logical = metrics.storage_logical_bytes.clone();
        let g_physical = metrics.storage_physical_bytes.clone();
        let g_count = metrics.storage_chunk_count.clone();
        let g_dev = metrics.pool_device_capacity_bytes.clone();
        let g_meta = metrics.storage_meta_bytes.clone();
        let g_small = metrics.storage_small_bytes.clone();
        // ADR-030 2026-05-31 amendment §"admin-driven metadata device
        // role" — clone the metrics handle + capture data_dir for
        // periodic NodeMetadataCapacity refresh inside the same tick.
        let metrics_for_meta = metrics.clone();
        let meta_data_dir = cfg.data_dir.clone();
        let meta_soft_pct = cfg.meta_soft_limit_pct;
        let meta_hard_pct = cfg.meta_hard_limit_pct;
        let g_tier = [
            (
                metrics.storage_tier_fast_used.clone(),
                metrics.storage_tier_fast_total.clone(),
            ),
            (
                metrics.storage_tier_bulk_used.clone(),
                metrics.storage_tier_bulk_total.clone(),
            ),
            (
                metrics.storage_tier_cold_used.clone(),
                metrics.storage_tier_cold_total.clone(),
            ),
        ];
        // System-disk tier dirs (ADR-030 last-resort tier guardrail).
        let meta_dir = cfg.data_dir.as_ref().map(|d| d.join("metadata"));
        let small_dir = cfg.data_dir.as_ref().map(|d| d.join("small"));
        let interval_s = std::env::var("KISEKI_CAPACITY_REFRESH_INTERVAL_S")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(30);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_s));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let s = stats_store.storage_stats().await;
                let to_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
                g_used.set(to_i64(s.device_used_bytes));
                g_total.set(to_i64(s.device_total_bytes));
                g_logical.set(to_i64(s.logical_bytes));
                g_physical.set(to_i64(s.physical_bytes));
                g_count.set(to_i64(s.chunk_count));
                if let Some(ref md) = meta_dir {
                    g_meta.set(to_i64(dir_size_recursive(md)));
                }
                if let Some(ref sd) = small_dir {
                    g_small.set(to_i64(dir_size_recursive(sd)));
                }
                if let Some(dev) = &stats_device {
                    // Aggregate per-device usage by cost/performance tier
                    // (ADR-024) for the cluster-wide per-class view.
                    let mut tier_used = [0u64; 3];
                    let mut tier_total = [0u64; 3];
                    for d in dev.device_breakdown() {
                        let id = uuid::Uuid::from_bytes(d.device_id).to_string();
                        let free = d.total_bytes.saturating_sub(d.used_bytes);
                        g_dev
                            .with_label_values(&["data", &id, "total"])
                            .set(to_i64(d.total_bytes));
                        g_dev
                            .with_label_values(&["data", &id, "used"])
                            .set(to_i64(d.used_bytes));
                        g_dev
                            .with_label_values(&["data", &id, "free"])
                            .set(to_i64(free));
                        let t = match kiseki_block::StorageTier::of(d.medium) {
                            kiseki_block::StorageTier::Fast => 0,
                            kiseki_block::StorageTier::Bulk => 1,
                            kiseki_block::StorageTier::Cold => 2,
                        };
                        tier_used[t] = tier_used[t].saturating_add(d.used_bytes);
                        tier_total[t] = tier_total[t].saturating_add(d.total_bytes);
                    }
                    for (i, (gu, gt)) in g_tier.iter().enumerate() {
                        gu.set(to_i64(tier_used[i]));
                        gt.set(to_i64(tier_total[i]));
                    }
                }
                // ADR-030 2026-05-31 amendment — refresh the
                // metadata-role device's capacity snapshot in the same
                // tick. `compute_capacity` is df(1) + a sysfs read,
                // cheap relative to the 30s cadence. used_bytes drifts
                // as the data_dir fills; total/soft/hard are stable
                // for any sane mount.
                if let Some(ref dir) = meta_data_dir {
                    let cap =
                        crate::system_disk::compute_capacity(dir, meta_soft_pct, meta_hard_pct);
                    crate::system_disk::emit_capacity_metrics(&metrics_for_meta, &cap);
                }
            }
        });
        tracing::info!(
            interval_s,
            "chunk store: capacity/dedup metrics refresher spawned"
        );
    }

    // ADR-030 §3 + 2026-05-31 amendment §"throughput guard" —
    // per-shard inline-threshold recompute task (leader-only). Polls
    // the metrics handle for the local node's `small_file_budget`,
    // and per shard the delta_count + current ShardConfig. The
    // formula clamps to `[inline_floor, inline_ceiling]`; the
    // throughput guard drops the effective value to floor when the
    // shard's recent inline write rate exceeds
    // `KISEKI_RAFT_INLINE_MBPS` (default 10 MB/s, ADR-030 SF-ADV-1).
    //
    // Only fires `set_shard_config` when the new value differs from
    // the current — keeps the Raft log noise-free and avoids spurious
    // apply-hook work.
    //
    // KISEKI_INLINE_THRESHOLD_RECOMPUTE_S sets the poll interval
    // (default 60 s). `0` disables the task entirely — nothing is
    // spawned and each shard's inline threshold stays wherever it was
    // last committed (benchmark runs pin the small-object path this
    // way). Unset or unparsable keeps the default.
    let recompute_interval_s = std::env::var("KISEKI_INLINE_THRESHOLD_RECOMPUTE_S")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60);
    // ADR-030 §3 — gateway slot the recompute task pushes the
    // committed per-shard threshold into. Constructed here so the
    // task can capture a clone; the gateway-build block below stores
    // a `Weak<InMemoryGateway>` once the gateway exists. Until then
    // the slot holds `None` and the per-shard push step no-ops
    // (still drives the Raft commit + multi-voter aggregation).
    let gateway_for_threshold_push: Arc<
        parking_lot::RwLock<Option<std::sync::Weak<kiseki_gateway::InMemoryGateway>>>,
    > = Arc::new(parking_lot::RwLock::new(None));
    if recompute_interval_s == 0 {
        tracing::info!(
            "ADR-030 inline-threshold recompute task disabled (KISEKI_INLINE_THRESHOLD_RECOMPUTE_S=0)"
        );
    } else if let Some(ref shard_store_for_recompute) = raft_shard_store_for_admin {
        let store = Arc::clone(shard_store_for_recompute);
        let metrics_for_recompute = metrics.clone();
        let gateway_slot = Arc::clone(&gateway_for_threshold_push);
        // ADR-030 §3 multi-voter aggregation: scrape every peer's
        // `/metrics` endpoint over the cfg-known address list and
        // compute `min(soft_limit)` across the voter set. An
        // unreachable peer falls back to its last successful scrape
        // for the duration of `voter_soft_limit_stale_after`; beyond
        // that we treat its budget as `u64::MAX` (don't bind below
        // the local node's view on a transient flap).
        let peer_metrics_urls: Vec<String> = cfg
            .raft_peers
            .iter()
            .filter(|(id, _)| *id != cfg.node_id)
            .map(|(_, addr)| metrics_url_from_raft_peer(addr))
            .collect();
        let mbps_limit = std::env::var("KISEKI_RAFT_INLINE_MBPS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(10);
        tokio::spawn(async move {
            use std::collections::HashMap;
            use std::time::Instant;
            let mut tick =
                tokio::time::interval(std::time::Duration::from_secs(recompute_interval_s));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Per-shard throughput guards live for the lifetime of the
            // task (one entry per shard the leader has seen). On
            // leadership loss the guard's stale samples are harmless;
            // they fall out of the 10 s window before this node ever
            // recomputes again.
            let mut guards: HashMap<
                kiseki_common::ids::ShardId,
                kiseki_log::shard_inline_threshold::InlineThroughputGuard,
            > = HashMap::new();
            // Per-peer last-good soft_limit cache for the multi-voter
            // `min(soft_limit)` aggregation. Survives scrape errors
            // up to `voter_soft_limit_stale_after`; beyond that the
            // stale value is dropped.
            let voter_soft_limit_stale_after =
                std::time::Duration::from_secs(recompute_interval_s * 3);
            let mut peer_soft_limit_cache: HashMap<String, (u64, Instant)> = HashMap::new();
            loop {
                tick.tick().await;
                let now = Instant::now();
                // Local node's soft_limit, surfaced via the gauge the
                // Phase 2 capacity-emit task feeds.
                let local_soft_limit = u64::try_from(
                    metrics_for_recompute
                        .node_metadata_capacity_bytes
                        .with_label_values(&["soft_limit"])
                        .get()
                        .max(0),
                )
                .unwrap_or(0);
                // ADR-030 SF-ADV-4 emergency reduction: when the
                // local node's used > hard_limit, force the formula
                // to clamp at the configured floor. The Raft commit
                // on the leader path below picks this up; followers
                // see it via their own gauge readings on the next
                // tick.
                let local_used = u64::try_from(
                    metrics_for_recompute
                        .node_metadata_capacity_bytes
                        .with_label_values(&["used"])
                        .get()
                        .max(0),
                )
                .unwrap_or(0);
                let local_hard_limit = u64::try_from(
                    metrics_for_recompute
                        .node_metadata_capacity_bytes
                        .with_label_values(&["hard_limit"])
                        .get()
                        .max(0),
                )
                .unwrap_or(u64::MAX);
                let force_floor = local_hard_limit > 0 && local_used >= local_hard_limit;
                // ADR-030 §3 multi-voter min. Scrape every peer's
                // `/metrics` over HTTP; parse the
                // `kiseki_node_metadata_capacity_bytes{kind="soft_limit"}`
                // sample. Fold the local value in too.
                let mut budgets: Vec<u64> = vec![local_soft_limit];
                for url in &peer_metrics_urls {
                    if let Some(text) = crate::web::aggregator::reqwest_get_body(url).await {
                        if let Some(v) = parse_node_metadata_soft_limit_from_metrics(&text) {
                            budgets.push(v);
                            peer_soft_limit_cache.insert(url.clone(), (v, now));
                            continue;
                        }
                    }
                    // Fall through: use cached value if still fresh.
                    if let Some(&(v, when)) = peer_soft_limit_cache.get(url) {
                        if now.duration_since(when) <= voter_soft_limit_stale_after {
                            budgets.push(v);
                        }
                    }
                }
                let available_bytes = budgets.iter().copied().min().unwrap_or(local_soft_limit);
                for shard_id in store.shard_ids() {
                    // Read shard_health on every node so each can
                    // update its own gateway from the Raft-committed
                    // config. Leader is additionally responsible for
                    // recomputing + committing a new value when the
                    // formula + guard say so.
                    let info =
                        match kiseki_log::traits::LogOps::shard_health(store.as_ref(), shard_id)
                            .await
                        {
                            Ok(i) => i,
                            Err(e) => {
                                tracing::debug!(
                                    shard_id = %shard_id.0,
                                    error = %e,
                                    "inline-threshold recompute: shard_health unavailable",
                                );
                                continue;
                            }
                        };
                    // Push the committed threshold into the local
                    // gateway's per-shard map. Every node does this
                    // (leader OR follower) so the gateway-side write
                    // path consults the live value instead of the
                    // boot-time global.
                    if let Some(weak) = gateway_slot.read().clone() {
                        if let Some(gw) = weak.upgrade() {
                            gw.set_shard_inline_threshold(
                                shard_id,
                                info.config.inline_threshold_bytes,
                            );
                        }
                    }
                    if !store.is_shard_leader(shard_id) {
                        continue;
                    }
                    let raw = if force_floor {
                        // SF-ADV-4 emergency reduction overrides the
                        // formula; the leader still goes through Raft
                        // so followers' gateways converge after their
                        // next recompute tick.
                        info.config.inline_floor_bytes
                    } else {
                        kiseki_log::shard_inline_threshold::compute_shard_inline_threshold(
                            available_bytes,
                            info.delta_count,
                            info.config.inline_floor_bytes,
                            info.config.inline_ceiling_bytes,
                        )
                    };
                    let guard = guards.entry(shard_id).or_insert_with(|| {
                        kiseki_log::shard_inline_threshold::InlineThroughputGuard::with_limit(
                            std::time::Duration::from_secs(10),
                            mbps_limit,
                        )
                    });
                    let effective =
                        guard.effective_threshold(now, raw, info.config.inline_floor_bytes);
                    if effective == info.config.inline_threshold_bytes {
                        continue;
                    }
                    let mut new_config = info.config.clone();
                    new_config.inline_threshold_bytes = effective;
                    if let Err(e) = store.submit_shard_config(shard_id, new_config) {
                        tracing::warn!(
                            shard_id = %shard_id.0,
                            error = %e,
                            "inline-threshold recompute: SetShardConfig failed",
                        );
                    } else {
                        tracing::info!(
                            shard_id = %shard_id.0,
                            previous = info.config.inline_threshold_bytes,
                            effective,
                            "inline-threshold recompute: SetShardConfig committed",
                        );
                    }
                }
            }
        });
        tracing::info!(
            interval_s = recompute_interval_s,
            mbps_limit,
            "ADR-030 inline-threshold recompute task spawned (leader-only per shard)",
        );
    }

    // Cluster chunk fabric (Phase 16a step 12). For each *other* peer
    // in raft_peers we open a lazy mTLS gRPC Channel to its data-path
    // port and wrap it in GrpcFabricPeer. For a 1-node cluster peers
    // is empty and the store degenerates to local-only (D-6); the
    // existing single-node tests stay green by construction.
    //
    // The data-path port carries both the data services AND the
    // ClusterChunkService — peers reuse the same port. The SAN-role
    // interceptor (step 5) gates fabric methods to certs that carry
    // a `spiffe://cluster/fabric/<node-id>` SAN URI.
    let bootstrap_tenant_for_cluster = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));
    let mut fabric_peers: Vec<Arc<dyn kiseki_chunk_cluster::FabricPeer>> = Vec::new();
    let data_port = cfg.data_addr.port();
    // 2026-06-01 fabric transport selection. ADR-042 §2.2 calls for
    // TCP-framed-postcard as the default to escape the gRPC/h2 tax;
    // the 2026-06-01 3-node loopback profile confirmed the fabric edge
    // was paying ~14× the receiver-work cost in gRPC overhead alone.
    // **DEFAULT IS TCP** — gRPC stays only as an explicit fallback so
    // a future implementer cannot regress this silently:
    //
    //   KISEKI_FABRIC_TRANSPORT=tcp     (default; ADR-042 §2.2)
    //   KISEKI_FABRIC_TRANSPORT=grpc    (explicit fallback)
    //
    // The TCP-framed fabric listener binds on `data_port + 50`; the
    // gRPC fabric service stays on `data_port` for back-compat during
    // rolling upgrades. Peers chosen at startup based on this flag.
    let fabric_transport_is_tcp = std::env::var("KISEKI_FABRIC_TRANSPORT")
        .ok()
        .map(|v| v.to_ascii_lowercase())
        .is_none_or(|v| !matches!(v.as_str(), "grpc" | "g"));
    let fabric_tcp_port_offset: u16 = 50;
    tracing::info!(
        transport = if fabric_transport_is_tcp {
            "tcp-framed-postcard (ADR-042 §2.2 default)"
        } else {
            "grpc (KISEKI_FABRIC_TRANSPORT=grpc fallback)"
        },
        "fabric peer transport selected",
    );
    // Build a peer-id → fabric address map. `KISEKI_FABRIC_PEERS`
    // (cfg.fabric_peers) overrides the per-port derivation below,
    // which is the only path that works when every node binds a
    // distinct data-path port (e.g. localhost multi-node, BDD).
    let fabric_override: std::collections::HashMap<u64, &str> = cfg
        .fabric_peers
        .iter()
        .map(|(id, addr)| (*id, addr.as_str()))
        .collect();
    for (peer_id, raft_peer_addr) in &cfg.raft_peers {
        if *peer_id == cfg.node_id {
            continue; // skip ourselves
        }
        let fabric_addr = fabric_override.get(peer_id).map_or_else(
            || fabric_addr_from_raft_peer(raft_peer_addr, data_port),
            |s| (*s).to_owned(),
        );
        let name = format!("node-{peer_id}");
        if fabric_transport_is_tcp {
            // TCP-framed-postcard fabric (ADR-042 §2.2 default).
            // Derive the TCP-framed address by adding the port
            // offset to the gRPC fabric address; same host, port+50.
            // Lazy construction — the connect happens on first call,
            // matching tonic Channel semantics. The runtime can come
            // up regardless of peer reachability at startup.
            let tcp_addr = derive_tcp_fabric_addr(&fabric_addr, fabric_tcp_port_offset);
            let peer =
                kiseki_chunk_cluster::TcpFramedFabricPeer::new_lazy(name.clone(), tcp_addr.clone());
            fabric_peers.push(peer);
            tracing::info!(
                peer_id,
                fabric_addr = %tcp_addr,
                transport = "tcp-framed-postcard (lazy connect)",
                "fabric peer registered for cross-node chunks",
            );
        } else {
            // gRPC fabric (legacy fallback).
            match build_fabric_channel(&fabric_addr, cfg.tls.as_ref()) {
                Ok(channel) => {
                    fabric_peers.push(Arc::new(
                        kiseki_chunk_cluster::GrpcFabricPeer::new(name, channel)
                            .with_metrics(Arc::clone(&metrics.fabric)),
                    ));
                    tracing::info!(
                        peer_id,
                        fabric_addr,
                        transport = "grpc",
                        "fabric peer registered for cross-node chunks",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        peer_id, fabric_addr, error = %e,
                        "fabric peer channel build failed — peer skipped (cluster may run degraded)",
                    );
                }
            }
        }
    }
    if !fabric_peers.is_empty() {
        tracing::info!(
            peers = fabric_peers.len(),
            "cross-node chunk replication enabled (Phase 16a)",
        );
    } else if cfg.raft_peers.len() > 1 {
        tracing::warn!(
            "cross-node fabric is empty despite raft_peers > 1 — cluster running in local-only mode",
        );
    }
    // Phase 16b step 3: pick durability defaults (copies + min_acks)
    // from the cluster size. 1-node → local-only (min_acks=1, won't
    // deadlock single-node tests). 2-node → Replication-2, both peers
    // ack. 3+ nodes → Replication-3 with 2-of-3 quorum.
    let cluster_size = cfg.raft_peers.len().max(1);
    let durability = kiseki_chunk_cluster::defaults_for(cluster_size);
    tracing::info!(
        cluster_size,
        copies = durability.copies,
        min_acks = durability.min_acks,
        strategy = ?durability.strategy,
        "cluster durability defaults",
    );
    // Phase 16e step 1: thread the per-cluster-size strategy into
    // ClusterCfg.ec_strategy so write_chunk / read_chunk dispatch
    // (16d steps 1+5) routes a 6+ node cluster through the EC
    // path, honoring I-C4 ("EC is the default") + I-D1 ("repaired
    // from EC parity").
    let cluster_nodes_for_cfg: Vec<u64> = cfg.raft_peers.iter().map(|(id, _)| *id).collect();
    let cluster_cfg =
        kiseki_chunk_cluster::ClusterCfg::new(bootstrap_tenant_for_cluster, "default")
            .with_min_acks(durability.min_acks)
            .with_ec_strategy(durability.strategy)
            .with_cluster_nodes(cluster_nodes_for_cfg)
            .with_self_node_id(cfg.node_id);
    // Phase 16d step 4: clone the peer list before it's moved into
    // ClusteredChunkStore so the scrub-scheduler adapters can build
    // a parallel by-id index for HasFragment + repair calls.
    let fabric_peers_for_scrub: Vec<Arc<dyn kiseki_chunk_cluster::FabricPeer>> =
        fabric_peers.iter().map(Arc::clone).collect();
    // GCP 2026-05-02 fix: share one envelope registry between the
    // local-node ClusterChunkServer (populated by incoming
    // PutFragment RPCs) and the local-node ClusteredChunkStore
    // (populated by the leader's own local-fragment writes). Without
    // this, peers fetching the leader's local fragment receive an
    // envelope with zero auth_tag/nonce — AES-GCM verify fails.
    // Issue #92 deeper fix (2026-05-19): persist envelope metadata so
    // the registry survives restart. Pre-fix the registry was
    // `Mutex<HashMap>` in-memory only; any chunk written by a
    // previous server generation had no metadata on read, which
    // post-PR-1 surfaces as `ChunkError::NotFound` (pre-PR-1
    // surfaced as a misleading AEAD verify failure). With persistence
    // wired here, restart is transparent — the in-memory cache warms
    // back up via `lookup` falling back to disk on miss.
    //
    // In-memory-only mode is preserved when `cfg.data_dir` is `None`
    // (single-node compose, tests).
    let envelope_registry = if let Some(ref dir) = cfg.data_dir {
        match kiseki_chunk_cluster::ChunkEnvelopeRegistry::with_data_dir(dir) {
            Ok(reg) => reg,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "envelope registry: failed to open persistent store — falling back to in-memory only (post-restart cross-leader reads will fail until chunks are re-written)",
                );
                kiseki_chunk_cluster::ChunkEnvelopeRegistry::default()
            }
        }
    } else {
        kiseki_chunk_cluster::ChunkEnvelopeRegistry::default()
    };
    // ADR-048 §"Decision" — keep a typed Arc to the ClusteredChunkStore
    // so we can hand it to `FabricSlabStore::open` after the gateway is
    // built (the trait-object form below can't be down-cast back to the
    // concrete type).
    let clustered_chunk_store: Arc<kiseki_chunk_cluster::ClusteredChunkStore> = Arc::new(
        kiseki_chunk_cluster::ClusteredChunkStore::new(
            Arc::clone(&local_chunk_store),
            fabric_peers,
            cluster_cfg,
        )
        .with_metrics(Arc::clone(&metrics.fabric))
        .with_envelope_registry(envelope_registry.clone()),
    );
    let chunk_store: Arc<dyn kiseki_chunk::AsyncChunkOps> =
        Arc::clone(&clustered_chunk_store) as Arc<dyn kiseki_chunk::AsyncChunkOps>;

    // Phase 16d step 4: spawn the periodic scrub scheduler when
    // running on a real cluster (>=1 peer; in single-node mode
    // there are no fragments to scrub against and no peers to
    // probe / repair from). Cadence is currently a fixed 10
    // minutes per shard — operators can revisit once the
    // scheduler ships per-shard metrics.
    let scrub_scheduler_handle: Option<Arc<kiseki_chunk_cluster::ScrubScheduler>> =
        if fabric_peers_for_scrub.is_empty() {
            None
        } else {
            let scrub_log = Arc::clone(&log_store) as Arc<dyn kiseki_log::traits::LogOps>;
            let scrub_local = Arc::clone(&local_chunk_store);
            let scrub_oracle: Arc<dyn kiseki_chunk_cluster::FragmentAvailabilityOracle> = Arc::new(
                kiseki_chunk_cluster::FabricAvailabilityOracle::new(&fabric_peers_for_scrub),
            );
            let scrub_deleter: Arc<dyn kiseki_chunk_cluster::OrphanDeleter> = Arc::new(
                kiseki_chunk_cluster::LocalChunkDeleter::new(Arc::clone(&local_chunk_store)),
            );
            let scrub_repairer: Arc<dyn kiseki_chunk_cluster::Repairer> =
                Arc::new(kiseki_chunk_cluster::FabricRepairer::new(
                    &fabric_peers_for_scrub,
                    bootstrap_tenant_for_cluster,
                    "default".into(),
                ));
            let scheduler = Arc::new(
                kiseki_chunk_cluster::ScrubScheduler::new(
                    scrub_log,
                    scrub_local,
                    scrub_oracle,
                    scrub_deleter,
                    scrub_repairer,
                    kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
                    bootstrap_tenant_for_cluster,
                    kiseki_chunk_cluster::OrphanScrubPolicy::default(),
                    kiseki_chunk_cluster::UnderReplicationPolicy {
                        target_copies: durability.copies,
                        min_acks: durability.min_acks,
                    },
                )
                // Phase 16e step 3: thread the EC strategy so the scrub
                // dispatches via repair_ec on EC clusters (≥6 nodes per
                // the defaults table). Replication-N stays on the legacy
                // repair() path via the trait default.
                .with_strategy(durability.strategy),
            );
            // Phase 16e step 4: build a shutdown channel + spawn the
            // scheduler with it. On Ctrl-C the data-path serve loop
            // exits via serve_with_shutdown; we send true on the
            // scrub channel so its loop drains cleanly + the
            // JoinHandle joins before the runtime shuts down. Today
            // the runtime doesn't have a shared shutdown signal hook,
            // so the scrub channel sender is leaked here — the
            // process exit terminates the loop. When the runtime
            // grows a unified shutdown registry this sender goes in
            // there.
            let (scrub_shutdown_tx, scrub_shutdown_rx) = tokio::sync::watch::channel(false);
            let scrub_handle = Arc::clone(&scheduler)
                .start_periodic(std::time::Duration::from_secs(600), scrub_shutdown_rx);
            // Detach: the channel sender + JoinHandle stay alive for
            // the process lifetime. Wiring a unified shutdown signal
            // is a runtime-wide concern tracked in
            // `specs/escalations/`.
            std::mem::drop((scrub_shutdown_tx, scrub_handle));
            tracing::info!(
                "scrub scheduler: spawned (orphan + under-replication, 10-min cadence, drain-on-shutdown)",
            );
            // ADR-025 W4: also hand the scheduler to
            // `StorageAdminGrpc::with_scrub` so the admin RPC can
            // call `trigger_now()` and `repair_one_chunk()`.
            Some(scheduler)
        };

    // Raw device discovery (KISEKI_RAW_DEVICES) is no longer a
    // separate log-only phase: the chunk-store construction above opens
    // each device as a `RawBlockDevice` and spans them into a
    // `DevicePool` (GH #115). This block previously only logged and
    // dropped the devices on the floor.

    // Composition: wired to log for delta emission. ADR-040 + ADR-022
    // successor: when KISEKI_DATA_DIR is set we back the comp_id →
    // Composition map with a fjall keyspace at
    // `<data_dir>/metadata/compositions/`, so hydrated state survives
    // restart and a node that joins late resumes from durable
    // `last_applied_seq`. Single-node / no-data-dir deployments keep
    // the in-memory backend (MemoryStorage) — same behavior as
    // pre-ADR-040.
    // Captures the keyspace directory so the periodic gauge refresher
    // can stat the on-disk footprint; None when the composition store
    // is in-memory.
    let mut comp_store_path: Option<std::path::PathBuf> = None;
    // `comp_flusher_for_fsync` declared at the runtime scope above
    // (before chunk-store construction) so both the chunk and
    // composition sections can populate it for the gateway
    // `fsync_pending` hook registration further down.
    let comp_storage: Box<dyn kiseki_composition::persistent::CompositionStorage> =
        if let Some(ref dir) = cfg.data_dir {
            // ADR-049 phase 5a continued: CompositionMeta resolved
            // via boot_paths. Falls back to `<data_dir>/metadata/
            // compositions/` when the pointer doesn't supply a
            // CompositionMeta tier.
            let path = boot_paths.composition_meta(dir);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "create persistent composition dir {}: {e}",
                        parent.display()
                    )
                })?;
            }
            // Group commit (FUSE p99 fix): every put / bind_name
            // would otherwise trigger an inline fsync. fjall's
            // `PersistMode::Buffer` + a periodic flusher that calls
            // `PersistMode::SyncAll` gives the same crash-safety
            // contract as the previous redb write-behind queue at a
            // bounded loss-window equal to the flush interval.
            // `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS` enables eventual
            // durability; multi-node deployments are safe because
            // Raft re-replicates lost compositions via the under-
            // replication scrub on restart.
            let flush_interval_ms = std::env::var("KISEKI_COMPOSITION_FLUSH_INTERVAL_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok());
            let store = kiseki_composition::persistent::FjallStorage::open(&path)
                .map_err(|e| format!("open persistent composition store: {e}"))?;
            let store = if flush_interval_ms.is_some() {
                let store = store.with_eventual_durability(true);
                let flusher = store.flusher();
                // Hand the same flusher to the gateway via
                // `register_fsync_hook` further below, so explicit
                // `fsync(2)` from FUSE / NFS clients forces a real
                // fsync rather than waiting up to `interval_ms` for
                // the periodic task.
                comp_flusher_for_fsync = Some(flusher.clone());
                let interval_ms = flush_interval_ms.unwrap_or(100);
                // Periodic WAL fsync. The LSM memtable + WAL append
                // happens inline on every write (cheap, in-memory);
                // this task drives the durability barrier at a bounded
                // cadence. Same contract as the previous redb
                // write-behind drainer's flush loop.
                tokio::spawn(async move {
                    let mut tick =
                        tokio::time::interval(std::time::Duration::from_millis(interval_ms));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        if let Err(e) = flusher.flush() {
                            tracing::warn!(error=%e, "composition store periodic fsync failed");
                        }
                    }
                });
                tracing::info!(
                    path = %path.display(),
                    interval_ms,
                    "composition store: persistent (fjall, eventual durability)",
                );
                store
            } else {
                tracing::info!(
                    path = %path.display(),
                    "composition store: persistent (fjall, immediate fsync)",
                );
                store
            };
            comp_store_path = Some(path);
            Box::new(store)
        } else {
            tracing::info!("composition store: in-memory (no KISEKI_DATA_DIR)");
            Box::new(kiseki_composition::persistent::MemoryStorage::new())
        };
    let comp_store = kiseki_composition::composition::CompositionStore::with_storage(comp_storage)
        .with_log(Arc::clone(&log_store) as Arc<dyn kiseki_log::LogOps + Send + Sync>);

    // View: shared between gateway (staleness check) and stream
    // processor. With KISEKI_DATA_DIR set, persist views via the
    // ADR-040 sibling so descriptors + watermarks survive restart;
    // otherwise fall back to in-memory.
    let view_storage: Box<dyn kiseki_view::persistent::ViewStorage> =
        if let Some(ref dir) = cfg.data_dir {
            let meta_dir = dir.join("metadata");
            std::fs::create_dir_all(&meta_dir)
                .map_err(|e| format!("create persistent view dir {}: {e}", meta_dir.display()))?;
            let path = meta_dir.join("views.redb");
            let store = kiseki_view::persistent::PersistentRedbStorage::open(&path)
                .map_err(|e| format!("open persistent view store: {e}"))?;
            tracing::info!(path = %path.display(), "view store: persistent (redb-backed, ADR-040)");
            Box::new(store)
        } else {
            tracing::info!("view store: in-memory (no KISEKI_DATA_DIR)");
            Box::new(kiseki_view::persistent::MemoryStorage::new())
        };
    // parking_lot::RwLock — read-mostly. The mem_gateway read path
    // takes a read lock to check view staleness on every gateway
    // read; std::sync::Mutex was 9.78% of CPU at 64 KiB GET on the
    // perf flame because all c=16 readers serialized through it.
    // Stream processor (advances watermarks every 100 ms) takes a
    // write lock briefly.
    let view_store = Arc::new(parking_lot::RwLock::new(
        kiseki_view::view::ViewStore::with_storage(view_storage),
    ));

    // Bootstrap namespace + view for protocol gateways. The IDs are
    // deterministic (UUID-from-u128(1) for shard/view, UUIDv5 of
    // "default" for the namespace), and the records are pure
    // convention — a multi-node cluster's followers need them
    // installed locally so the Phase 16f composition hydrator can
    // resolve their `namespace_id` field. Creating them on every node
    // is idempotent. The Raft-specific seeding (initialize the group
    // vs. join as a follower) is the only thing gated on
    // `cfg.bootstrap`.
    let bootstrap_tenant = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));
    let bootstrap_ns =
        kiseki_common::ids::NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default"));
    let bootstrap_view = kiseki_common::ids::ViewId(uuid::Uuid::from_u128(1));
    comp_store.add_namespace(kiseki_composition::namespace::Namespace {
        id: bootstrap_ns,
        tenant_id: bootstrap_tenant,
        shard_id: bootstrap_shard,
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
        tier_policy: Vec::new(),

        size_band_pools: kiseki_composition::namespace::NamespaceSizeBandPools::default(),
    });
    let _ = view_store.write().create_view(kiseki_view::ViewDescriptor {
        view_id: bootstrap_view,
        tenant_id: bootstrap_tenant,
        source_shards: vec![bootstrap_shard],
        protocol: kiseki_view::ProtocolSemantics::Posix,
        consistency: kiseki_view::ConsistencyModel::ReadYourWrites,
        discardable: true,
        version: 1,
    });
    if cfg.bootstrap {
        tracing::info!("bootstrap: namespace 'default' + view installed (Raft seed node)");
    } else {
        tracing::info!(
            "bootstrap: namespace 'default' + view installed (Raft follower; will hydrate compositions from log)",
        );
    }

    // Shared gateway: wires composition + chunk + crypto. Used by S3 and NFS.
    let master_key =
        kiseki_crypto::keys::SystemMasterKey::new([0x42; 32], kiseki_common::tenancy::KeyEpoch(1));
    // ADR-042 §9.1: derive the native-service signing keys (handle
    // tokens, DEK fetch tickets, multipart upload IDs) from the master
    // key BEFORE moving it into the gateway. The grace window is
    // tunable via KISEKI_MASTER_KEY_ROTATION_GRACE_MS (default 5 min).
    let native_grace_ms: u64 = std::env::var("KISEKI_MASTER_KEY_ROTATION_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300_000);
    let native_signing_keys = std::sync::Arc::new(
        kiseki_gateway::native::signing_keys::SigningKeys::new(&master_key, native_grace_ms),
    );
    // ADR-044: derive the tenant dedup HMAC key from the master key
    // BEFORE it is moved into the gateway. Production stores tenant data,
    // so it MUST use `DedupPolicy::TenantIsolated` —
    // `chunk_id = HMAC(tenant_dedup_key, plaintext)` — not the
    // `CrossTenant` SHA-256 default. The HMAC key is a secret function of
    // the master key (closing the confirmation-of-content oracle for
    // anyone without it) and is salted on the tenant id (confining dedup
    // to a single tenant). The constructor default stays `CrossTenant`
    // for system/non-tenant data; the production data plane opts in here.
    let tenant_dedup_key =
        kiseki_crypto::hkdf::derive_tenant_dedup_key(&master_key, bootstrap_tenant.0.as_bytes())?
            .to_vec();
    // Phase 16b step 2: pass the cluster's node-id list as the
    // placement for every fresh chunk. In a 1-node cluster this is
    // empty (the gateway carries vec![] in NewChunkMeta), matching
    // the single-node-degenerate path.
    let cluster_placement: Vec<u64> = cfg.raft_peers.iter().map(|(id, _)| *id).collect();
    // ADR-042 §4 / #103 / #111: proxy-fallback channel pool, constructed
    // BEFORE the gateway so it backs both the native server's proxy AND
    // the gateway's append-forwarder (#111 — forward writes/deletes/
    // multipart to a remote shard leader for every ingress). Peer
    // registration precedence: `KISEKI_PEER_DATA_ADDRS` (localhost-multi-
    // node test harness) else uniform per-node data port (prod posture).
    let proxy_client_for_native =
        std::sync::Arc::new(kiseki_gateway::native::proxy_client::ProxyClient::new(
            kiseki_common::ids::NodeId(cfg.node_id),
        ));
    {
        let registered =
            parse_peer_data_addrs(std::env::var("KISEKI_PEER_DATA_ADDRS").ok().as_deref());
        if registered.is_empty() {
            let peer_data_port = cfg.data_addr.port();
            for (peer_id, peer_addr) in &cfg.raft_peers {
                if *peer_id == cfg.node_id {
                    continue;
                }
                let host = peer_addr
                    .rsplit_once(':')
                    .map_or(peer_addr.as_str(), |(h, _)| h);
                proxy_client_for_native.register_node(
                    kiseki_common::ids::NodeId(*peer_id),
                    format!("{host}:{peer_data_port}"),
                );
            }
        } else {
            for (peer_id, data_addr) in registered {
                if peer_id == cfg.node_id {
                    continue;
                }
                proxy_client_for_native
                    .register_node(kiseki_common::ids::NodeId(peer_id), data_addr);
            }
        }
    }
    let mut gw_builder =
        kiseki_gateway::InMemoryGateway::new(comp_store, Arc::clone(&chunk_store), master_key)
            // ADR-047: this node's id seeds the per-node perspective clock
            // (the PerspectiveSeq tie-breaker). Decoupled-ack is THE path for
            // async-eligible surfaces (no capability gate).
            .with_node_id(cfg.node_id)
            .with_view_store(Arc::clone(&view_store))
            // ADR-044: tenant-isolated content-addressed dedup.
            .with_dedup_policy(
                kiseki_common::tenancy::DedupPolicy::TenantIsolated,
                Some(tenant_dedup_key),
            )
            // #111: forward a remote-led shard's metadata mutation to the
            // leader's LogService (write/delete/multipart, every ingress).
            .with_append_forwarder(std::sync::Arc::new(
                kiseki_gateway::native::append_forwarder::ProxyAppendForwarder::new(
                    std::sync::Arc::clone(&proxy_client_for_native),
                ),
            ))
            .with_cluster_placement(cluster_placement)
            // Phase 16c step 2: cap per-chunk placement at the
            // size-derived `copies` so a 6-node Replication-3 cluster
            // doesn't list all 6 nodes in every cluster_chunk_state row.
            .with_target_copies(usize::from(durability.copies))
            // ADR-040 §D7 + §D10 / F-4 closure: thread the read-path
            // retry counters (`kiseki_gateway_read_retry_total` and
            // `kiseki_gateway_read_retry_exhausted_total`) into the
            // gateway so operators can see whether they're hitting
            // the configurable budget.
            .with_retry_metrics(Arc::clone(&metrics.gateway_retry));
    // #129 — inline write path is now multi-node-correct via Raft replication
    // of `AppendChunkAndDeltaRequest.inline_payloads`. The gateway stages the
    // sealed envelope as `(chunk_id, env_bytes)` on the delta; each replica's
    // state-machine apply writes it to its local SmallObjectStore (ADR-022
    // rev-5 fjall-backed, keyed by chunk_id per ADR-030 §2). Cross-node GETs
    // read the local SmallObjectStore by the same chunk_id that the
    // composition references — no fabric round-trip for small files.
    let multi_node = !fabric_peers_for_scrub.is_empty();
    if let Some(ref ss) = small_store {
        gw_builder = gw_builder.with_inline_threshold(
            kiseki_log::ShardConfig::default().inline_threshold_bytes,
            std::sync::Arc::clone(ss)
                as std::sync::Arc<dyn kiseki_common::inline_store::InlineStore>,
        );
    }
    let gw = Arc::new(gw_builder);

    // GH #192: gateway namespace registry re-hydration from the
    // control-plane topology Raft. Two halves:
    //
    // 1. Attach the registrar to the apply hook, so every
    //    `CreateNamespace` applied FROM NOW ON (live submits, the
    //    tail of a restart's log replay, snapshot installs)
    //    registers the namespace with this gateway.
    // 2. Drain the state machine's CURRENT namespace set, covering
    //    every apply that fired BEFORE the attach (boot-time log
    //    replay races runtime construction; pre-attach the hook's
    //    registrar `OnceLock` is empty and skips).
    //
    // Attach-then-drain ordering leaves no window; both paths are
    // idempotent so the overlap is harmless. This runs BEFORE any
    // protocol listener (S3 / native / NFS) spawns, so a restarted
    // node never serves `NamespaceNotFound` for a namespace the
    // control plane has already replayed. Without this block, a
    // `docker compose restart` left `compositions.namespaces` empty
    // (it is volatile; the per-shard `NamespaceCreate` delta was
    // already consumed pre-restart per the durable
    // `last_applied_seq`) and 100% of writes failed until an admin
    // re-ran the namespace-create flow.
    let gw_namespace_registrar = Arc::new(GatewayNamespaceRegistrar {
        gw: Arc::clone(&gw),
    });
    if let Some(hook) = apply_hook_for_registry.as_ref() {
        hook.attach_namespace_registrar(Arc::clone(&gw_namespace_registrar)
            as Arc<dyn crate::cluster_control::NamespaceRegistrar>);
    }
    if let Some(ctrl_store) = cluster_control_store.as_ref() {
        let rehydrated = ctrl_store
            .rehydrate_gateway_namespaces(gw_namespace_registrar.as_ref(), |_shard| {})
            .await;
        tracing::info!(
            namespaces = rehydrated,
            "gateway namespace registry: boot re-hydration pass complete (GH #192)",
        );
    }

    // ADR-048 boot wiring — for every pool flagged
    // `requires_migration`, construct a `FabricSlabStore`, register
    // the per-pool backlog tracker, and spawn the slab-EC compactor
    // task on every node that has a Raft shard. The gateway's
    // cold-path read branch consults the slab store; the gateway's
    // `pool_is_async_ack_eligible` consults the backlog registry;
    // the compactor task emits `MigrateChunkLocations` deltas as it
    // flushes slabs.
    //
    // Wired AFTER `gw` exists so the per-pool calls below can
    // attach. Skipped entirely when no peer fabric is configured
    // (`fabric_peers_for_scrub.is_empty()`) — slab-EC requires the
    // fabric for fragment distribution, and a single-node cluster
    // has nothing to scatter across.
    let slab_backlog_registry: Arc<
        parking_lot::RwLock<
            std::collections::HashMap<
                String,
                Arc<parking_lot::Mutex<kiseki_chunk::slab::SlabBacklog>>,
            >,
        >,
    > = Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new()));
    if let Some(ref data_dir) = cfg.data_dir {
        if fabric_peers_for_scrub.is_empty() {
            tracing::debug!("slab-EC boot: skipped (single-node cluster, no fabric peers)");
        } else {
            let pool_snapshot = chunk_store.snapshot_pools().await;
            let placement_nodes_for_slab: Vec<u64> =
                cfg.raft_peers.iter().map(|(id, _)| *id).collect();
            // Register the backlog tracker on the gateway eagerly —
            // the registry is shared between every pool's
            // compactor and the gateway's `is_async_ack_eligible`
            // gate.
            gw.set_slab_backlog_registry(Arc::clone(&slab_backlog_registry));
            let mut migration_pools: Vec<String> = Vec::new();
            for pool in &pool_snapshot {
                if !pool.requires_migration {
                    continue;
                }
                migration_pools.push(pool.name.clone());
                let ec_strategy = match pool.durability {
                    kiseki_chunk::DurabilityStrategy::ErasureCoding {
                        data_shards,
                        parity_shards,
                    } => kiseki_chunk_cluster::ec::EcStrategy::Ec {
                        data: data_shards,
                        parity: parity_shards,
                    },
                    // Replication pools migrate to EC by default —
                    // the cold tier is EC-4+2 regardless of the
                    // hot-tier replication factor.
                    _ => kiseki_chunk_cluster::ec::EcStrategy::Ec { data: 4, parity: 2 },
                };
                let slab_data_dir = data_dir.join("slab_pools").join(&pool.name);
                let slab_store = match kiseki_chunk_cluster::slab_store::FabricSlabStore::open(
                    Arc::clone(&clustered_chunk_store),
                    placement_nodes_for_slab.clone(),
                    ec_strategy,
                    &slab_data_dir,
                    pool.name.clone(),
                ) {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        tracing::warn!(
                            pool = %pool.name,
                            error = %e,
                            "slab-EC boot: FabricSlabStore::open failed (compactor skipped for this pool)",
                        );
                        continue;
                    }
                };
                // Hand the FIRST migration-eligible pool's store to
                // the gateway as the cold-tier resolver. The
                // gateway today routes by chunk_locations' embedded
                // pool name, but it only holds one SlabStore handle;
                // a per-pool handle map is a follow-up if a single
                // cluster runs multiple migration-eligible pools.
                gw.set_slab_store(
                    Arc::clone(&slab_store) as Arc<dyn kiseki_chunk::slab::SlabStore>,
                );
                // Register backlog tracker in the shared registry so
                // the gateway sees it on `pool_is_async_ack_eligible`.
                let backlog = Arc::new(parking_lot::Mutex::new(
                    kiseki_chunk::slab::SlabBacklog::new(),
                ));
                slab_backlog_registry
                    .write()
                    .insert(pool.name.clone(), Arc::clone(&backlog));

                // Spawn the compactor task. `namespaces` is filled
                // by the runtime's namespace-discovery hook at
                // startup; until then the compactor scans the
                // bootstrap namespace (UUIDv4 0..0 + 1 from the
                // bootstrap_view setup at the top of run_main).
                // Subsequent namespaces register dynamically via
                // the control-plane apply path; a periodic
                // namespace-snapshot refresh inside the compactor
                // loop is a follow-up.
                let compositions_for_task = gw.compositions_handle();
                let cfg_for_task = kiseki_chunk_cluster::slab_compactor::CompactorCfg {
                    pool: pool.name.clone(),
                    sweep_interval: std::time::Duration::from_secs(5),
                    data_shards: 4,
                    parity_shards: 2,
                    namespaces: compositions_for_task
                        .list_namespaces()
                        .into_iter()
                        .map(|n| n.id)
                        .collect(),
                    tenant_id: kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1)),
                    shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
                };
                // Carve out a slab-store handle the compactor owns
                // (separate Arc from the one passed to the gateway).
                let slab_store_for_task: Arc<dyn kiseki_chunk::slab::SlabStore> =
                    Arc::clone(&slab_store) as _;
                let log_for_task: Arc<dyn kiseki_log::traits::LogOps + Send + Sync> =
                    Arc::clone(&log_store) as _;
                let registry_for_task =
                    Arc::new(kiseki_chunk_cluster::slab_store::SlabBacklogRegistry::new());
                // Seed the registry with the per-pool backlog so
                // `get_or_insert(pool)` returns the same handle the
                // gateway is reading from. The compactor's `spawn`
                // helper does its own `get_or_insert`; we mirror
                // the entry here.
                {
                    let _ = registry_for_task.get_or_insert(&pool.name);
                }
                let local_for_task: Arc<dyn kiseki_chunk::AsyncChunkOps> =
                    Arc::clone(&local_chunk_store);
                kiseki_chunk_cluster::slab_compactor::spawn(
                    cfg_for_task,
                    compositions_for_task,
                    local_for_task,
                    Arc::clone(&slab_store_for_task),
                    Arc::clone(&log_for_task),
                    registry_for_task,
                );

                // ADR-048 §"Slab GC" maintenance rewrite pass.
                // Runs at 50 s cadence (10× the compactor sweep) —
                // rewrites a fragmented slab is heavier than
                // flushing a fresh one, so the pass is deliberately
                // slow.
                let maintenance_pool = pool.name.clone();
                let maintenance_store = Arc::clone(&slab_store);
                let maintenance_log = Arc::clone(&log_for_task);
                let maintenance_tenant = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));
                let maintenance_shard = kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1));
                let maintenance_threshold = std::env::var("KISEKI_SLAB_REWRITE_RATIO")
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|r| (0.0..=1.0).contains(r))
                    .unwrap_or(0.5);
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(50));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        let rewritten = kiseki_chunk_cluster::slab_compactor::run_maintenance_pass(
                            &maintenance_pool,
                            4,
                            2,
                            maintenance_tenant,
                            maintenance_shard,
                            &*maintenance_store,
                            maintenance_log.as_ref(),
                            maintenance_threshold,
                        )
                        .await;
                        if rewritten > 0 {
                            tracing::info!(
                                pool = %maintenance_pool,
                                rewritten,
                                "slab-EC maintenance: rewrite pass complete",
                            );
                        }
                    }
                });

                tracing::info!(
                    pool = %pool.name,
                    "slab-EC boot: compactor + maintenance spawned (sweep 5s, rewrite 50s)",
                );
            }
            if migration_pools.is_empty() {
                tracing::debug!("slab-EC boot: no pools flagged requires_migration");
            }
        }
    }

    // ADR-030 §3 — register the gateway with the inline-threshold
    // recompute task so it can push committed per-shard values back
    // into `set_shard_inline_threshold`. Stored as `Weak` so the
    // task doesn't keep the gateway alive past shutdown.
    *gateway_for_threshold_push.write() = Some(Arc::downgrade(&gw));
    // Wire the shared workflow table + Prometheus counter so the
    // gateway's `x-kiseki-workflow-ref` header validation
    // (mem_gateway::write) is fully observable end-to-end. Without
    // this the gateway's atomic counters tick but `/metrics` shows
    // zero — and the BDD harness has nothing to assert on.
    gw.set_workflow_table(workflow_table.clone());

    // ADR-033 §5: hand the shard map to the gateway so writes route
    // by hashed_key range across all shards in the namespace's
    // topology. The map is hydrated by the control-plane apply path
    // (every node) — without this `set_shard_map` call the gateway's
    // `route_to_shard` lookup never fires and every write goes to
    // the namespace's primary `comp.shard_id`, which is what the
    // 2026-05-17 RCA traced "PUT throughput stuck at 1% of GET" to.
    if let Some(sm) = shard_map_store_for_gateway.as_ref() {
        gw.set_shard_map(Arc::clone(sm));
        tracing::info!("gateway shard_map: wired (ADR-033 §5 routing engaged)",);
    }

    // ADR-033 §1: wire the first-touch provisioner so any namespace
    // touched for the first time by S3, NFS, FUSE, or native gets
    // registered in the control-plane shard map. First-touch creates
    // single-shard namespaces (safe for the sequential workloads that
    // dominate the system path); tenant-admin-created namespaces go
    // through `POST /admin/topology/namespaces` with an explicit
    // shard count for parallel-write fanout. See
    // `ControlPlaneProvisioner::provision`.
    //
    // Only wired in multi-node mode where `cluster_control_store`
    // exists; single-node smoke clusters fall back to the legacy
    // single-shard `comps.add_namespace` path.
    if let (Some(ctrl_store), Some(raft_store)) = (
        cluster_control_store.as_ref(),
        raft_shard_store_for_admin.as_ref(),
    ) {
        let active_nodes: Vec<kiseki_common::ids::NodeId> = cfg
            .raft_peers
            .iter()
            .map(|(id, _)| kiseki_common::ids::NodeId(*id))
            .collect();
        let active_nodes = if active_nodes.is_empty() {
            vec![kiseki_common::ids::NodeId(cfg.node_id)]
        } else {
            active_nodes
        };
        let provisioner = Arc::new(ControlPlaneProvisioner {
            ctrl_store: Arc::clone(ctrl_store),
            raft_store: Arc::clone(raft_store),
            active_nodes,
            raft_runtime: raft_store.raft_runtime_handle(),
        });
        gw.set_namespace_provisioner(provisioner);
        tracing::info!(
            "gateway namespace provisioner: wired \
             (ADR-033 §1 first-touch single-shard; tenant-admin path \
             via POST /admin/topology/namespaces for multi-shard)",
        );
    }

    // FUSE / NFS `fsync(2)` correctness under the eventual-durability
    // optimization. Each registered hook drives a real fsync on its
    // backing store. Without these, callers that explicitly issue
    // `fsync(2)` would silently get the periodic-task SLA (≤100 ms
    // window) instead of POSIX's "data is on disk now" guarantee.
    if let Some(flusher) = comp_flusher_for_fsync {
        // fjall's LSM memtable + WAL append happens inline on every
        // write; the hook drives a `PersistMode::SyncAll` so the
        // POSIX `fsync(2)` contract is honored (no extra "drain
        // overlay first" stage — fjall has no separate overlay).
        gw.register_fsync_hook(std::sync::Arc::new(move || {
            flusher.flush().map_err(|e| {
                kiseki_gateway::error::GatewayError::Upstream(format!(
                    "composition store fsync: {e}"
                ))
            })
        }));
        tracing::info!("fsync hook: composition store (fjall) registered");
    }
    if let Some(device) = chunk_device_for_fsync {
        gw.register_fsync_hook(std::sync::Arc::new(move || {
            device.sync().map_err(|e| {
                kiseki_gateway::error::GatewayError::Upstream(format!("chunk fsync: {e}"))
            })
        }));
        tracing::info!("fsync hook: chunk-store device registered");
    }
    if let Some(flusher) = small_flusher_for_fsync {
        // #212: the small-object store runs on group commit by default;
        // explicit fsync(2) forces the inline-tier WAL durable now.
        gw.register_fsync_hook(std::sync::Arc::new(move || {
            flusher.flush().map_err(|e| {
                kiseki_gateway::error::GatewayError::Upstream(format!(
                    "small object store fsync: {e}"
                ))
            })
        }));
        tracing::info!("fsync hook: small-object store (fjall) registered");
    }
    if let Some(ref shard_store) = raft_shard_store_for_admin {
        // #212: per-shard intent stores run at the I-L5 group-commit
        // point (page-cache on min_acks); fsync(2) must still mean
        // "durable on THIS node now" for POSIX surfaces.
        let shard_store = std::sync::Arc::clone(shard_store);
        gw.register_fsync_hook(std::sync::Arc::new(move || {
            shard_store.flush_intent_stores().map_err(|e| {
                kiseki_gateway::error::GatewayError::Upstream(format!("intent store fsync: {e}"))
            })
        }));
        tracing::info!("fsync hook: per-shard intent stores registered");
    }

    gw.set_workflow_ref_writes_metric(Arc::new(metrics.gateway_workflow_ref_writes_total.clone()));
    // Mirror the gateway's atomic byte counters into the registered
    // Prometheus counters so `/metrics` scrapes show live throughput.
    // The GCP 2026-05-02 perf cluster reported these as 0 after
    // 1 GB of writes — i.e. the wiring was missing. Asserted by the
    // protocol-gateway BDD "operational metrics smoke" scenario.
    gw.set_chunk_byte_metrics(
        Arc::new(metrics.chunk_write_bytes.clone()),
        Arc::new(metrics.chunk_read_bytes.clone()),
    );
    gw.set_phase_duration_metrics(
        Arc::new(metrics.gateway_get_phase_duration.clone()),
        Arc::new(metrics.gateway_put_phase_duration.clone()),
    );

    // S3 gateway.
    //
    // ADR-008 rev 2 / ADR-014 / ADR-042 §4 — activate the leader-
    // redirect-capable router. Builds a `NodeId → "host:s3_port"` map
    // from `cfg.raft_peers` using the same host-substitution +
    // local-port pattern as `compute_storage_ds_addrs` above so the
    // 307 `Location:` header points at the right peer. Plumbs the
    // `stale_leader_redirects_total` counter so every 307 emission
    // ticks the metric with the SigV4-resolved tenant label (S8).
    let s3_gw = kiseki_gateway::s3::S3Gateway::new(Arc::clone(&gw));
    let s3_peer_addrs = compute_s3_peer_addrs(&cfg.raft_peers, cfg.s3_addr);
    let s3_router = kiseki_gateway::s3_server::s3_router_with_peers(
        s3_gw,
        bootstrap_tenant,
        kiseki_gateway::s3_auth::AccessKeyStore::new(),
        Some(Arc::new(metrics.gateway_requests_total.clone())),
        Some(Arc::new(metrics.gateway_request_duration.clone())),
        s3_peer_addrs,
        Some(Arc::new(metrics.stale_leader_redirects_total.clone())),
    );
    let s3_addr = cfg.s3_addr;
    let s3_tls = cfg.tls.as_ref().and_then(|files| {
        let ca = std::fs::read(&files.ca_path).ok()?;
        let cert = std::fs::read(&files.cert_path).ok()?;
        let key = std::fs::read(&files.key_path).ok()?;
        kiseki_transport::TlsConfig::server_config(&ca, &cert, &key)
            .map(Arc::new)
            .ok()
    });
    tokio::spawn(async move {
        kiseki_gateway::s3_server::run_s3_server(s3_addr, s3_router, s3_tls).await;
    });

    // Prometheus metrics + admin UI server. The KisekiMetrics
    // registry was built earlier so the cluster fabric could plug
    // its FabricMetrics in. Reuse it here.
    let metrics_addr = cfg.metrics_addr;
    // Collect peer metrics addresses for the admin UI aggregator.
    let peer_metrics_addrs: Vec<String> = cfg
        .raft_peers
        .iter()
        .map(|(_, addr)| {
            // Raft peer addr is host:raft_port. Metrics is on the metrics port.
            // For now, assume peers use the same metrics port as this node.
            let host = addr.split(':').next().unwrap_or("127.0.0.1");
            format!("{host}:{}", metrics_addr.port())
        })
        .collect();
    let node_info = crate::web::api::NodeInfo {
        node_id: cfg.node_id,
        s3_addr: cfg.s3_addr.to_string(),
        nfs_addr: cfg.nfs_addr.to_string(),
        metrics_addr: cfg.metrics_addr.to_string(),
        raft_peers: cfg.raft_peers.clone(),
    };
    let metrics_log_store = Arc::clone(&log_store) as Arc<dyn kiseki_log::LogOps + Send + Sync>;
    let metrics_compositions = Some(gw.compositions_handle());
    let metrics_local_chunk_store = Some(Arc::clone(&local_chunk_store));
    // Pre-clone the §D10 composition metrics handle: the hydrator + the
    // periodic redb-size refresher (spawned later) both need it after
    // `metrics` is moved into the metrics-server task.
    let composition_metrics_for_hydrator = Arc::clone(&metrics.composition);
    let composition_metrics_for_size_refresh = Arc::clone(&metrics.composition);
    // Pre-clone the ADR-025 admin RPC counter; storage_admin_handler
    // is constructed below after `metrics` moves into the spawn.
    let storage_admin_calls_counter = Arc::new(metrics.storage_admin_calls_total.clone());
    // Pre-clone the fabric metrics for the ClusterChunkServer wired
    // further down; same reason — `metrics` moves into the spawn.
    let cluster_chunk_server_fabric = Arc::clone(&metrics.fabric);
    // Pre-clone the cluster-control metrics for `StorageAdminGrpc`'s
    // forwarding paths and `OpenRaftControlStore::with_metrics()`.
    let cluster_control_metrics_for_admin = Arc::clone(&metrics.cluster_control);
    // ADR-008 rev 2: thread the control-plane state machine to the
    // metrics server so `/cluster/info` can project per-shard leader
    // info from `NamespaceShardMap`. `None` on single-node deploys.
    let cluster_control_state_for_ui = cluster_control_store.as_ref().map(|s| Arc::new(s.state()));
    // Writable store handle for the multi-shard namespace endpoint
    // (`POST /admin/topology/namespaces`, #68). Read-only callers
    // already use `cluster_control_state_for_ui` above.
    let cluster_control_store_for_ui = cluster_control_store.clone();
    // Pre-construct the tenant + namespace + drain handles so the
    // admin UI can share them with the gRPC `ControlService` further
    // below. The gRPC service is built after this point — both
    // consumers see the same in-memory state.
    let control_tenants_for_ui: Arc<kiseki_control::tenant::TenantStore> =
        Arc::new(kiseki_control::tenant::TenantStore::new());
    let control_namespaces_for_ui: Arc<kiseki_control::namespace::NamespaceStore> =
        Arc::new(kiseki_control::namespace::NamespaceStore::new());
    let drain_orchestrator: Arc<kiseki_control::node_lifecycle::DrainOrchestrator> =
        Arc::new(kiseki_control::node_lifecycle::DrainOrchestrator::new());
    // UI handles cloned for the spawn (originals stay in scope to
    // pass to the gRPC `ControlService` further down).
    let tenants_for_spawn = Arc::clone(&control_tenants_for_ui);
    let namespaces_for_spawn = Arc::clone(&control_namespaces_for_ui);
    let drain_for_spawn = Arc::clone(&drain_orchestrator);
    let audit_for_spawn = Arc::clone(&audit_for_ui);
    let key_store_for_spawn: Arc<dyn kiseki_keymanager::KeyManagerOps> = Arc::clone(&key_store);
    tokio::spawn(async move {
        if let Err(e) = crate::metrics::run_metrics_server(
            metrics_addr,
            metrics,
            peer_metrics_addrs,
            Some(metrics_log_store),
            node_info,
            metrics_compositions,
            metrics_local_chunk_store,
            cluster_control_state_for_ui,
            cluster_control_store_for_ui,
            Some(audit_for_spawn),
            Some(key_store_for_spawn),
            Some(tenants_for_spawn),
            Some(namespaces_for_spawn),
            Some(drain_for_spawn),
        )
        .await
        {
            tracing::error!(error = %e, "metrics server error");
        }
    });

    // NFS gateway (NFSv3 + NFSv4.2 + pNFS on port 2049).
    //
    // ADR-038 §D4 transport gate: TLS by default, audited plaintext
    // fallback only with both flags set. Gate runs before any listener
    // binds so the server refuses to start cleanly on misconfiguration.
    let env_insecure_nfs =
        std::env::var("KISEKI_INSECURE_NFS").is_ok_and(|v| v == "true" || v == "1");
    let security = kiseki_gateway::nfs_security::evaluate(
        cfg.allow_plaintext_nfs,
        env_insecure_nfs,
        cfg.tls.is_some(),
        cfg.pnfs.layout_ttl_seconds,
        1, // bootstrap_tenant on this listener — single-tenant default
    )
    .map_err(|e| format!("NFS security gate refused start: {e}"))?;

    if security.emit_warn_banner {
        tracing::warn!(target: "kiseki::nfs::security", "{}", kiseki_gateway::nfs_security::PLAINTEXT_WARN_BANNER);
    }
    if let Some(audit_type) = security.audit_event {
        use kiseki_audit::event::AuditEvent;
        use kiseki_audit::store::AuditOps;
        use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
        audit_store.append(AuditEvent {
            sequence: kiseki_common::ids::SequenceNumber(0),
            timestamp: DeltaTimestamp {
                hlc: HybridLogicalClock {
                    physical_ms: now_ms,
                    logical: 0,
                    node_id: kiseki_common::ids::NodeId(cfg.node_id),
                },
                wall: WallTime {
                    millis_since_epoch: now_ms,
                    timezone: "UTC".into(),
                },
                quality: ClockQuality::Ntp,
            },
            event_type: audit_type,
            tenant_id: None,
            actor: "kiseki-server".to_string(),
            description: "plaintext NFS fallback active per ADR-038 §D4.2 — \
                operator opted in via [security].allow_plaintext_nfs=true \
                AND KISEKI_INSECURE_NFS=true"
                .to_string(),
        });
    }
    tracing::info!(
        mode = ?security.mode,
        layout_ttl_seconds = security.effective_layout_ttl_seconds,
        "NFS transport posture",
    );

    let nfs_tls = match security.mode {
        kiseki_gateway::nfs_security::NfsTransport::Tls => cfg.tls.as_ref().and_then(|files| {
            let ca = std::fs::read(&files.ca_path).ok()?;
            let cert = std::fs::read(&files.cert_path).ok()?;
            let key = std::fs::read(&files.key_path).ok()?;
            kiseki_transport::TlsConfig::server_config(&ca, &cert, &key)
                .map(Arc::new)
                .ok()
        }),
        kiseki_gateway::nfs_security::NfsTransport::Plaintext => None,
    };

    let nfs_gw = kiseki_gateway::nfs::NfsGateway::new(Arc::clone(&gw));
    let nfs_addr = cfg.nfs_addr;

    // Phase 15c.4 — construct the shared MdsLayoutManager BEFORE
    // either listener so NFS (MDS) and DS see the same instance and
    // the same fh4 MAC key. The manager governs:
    //   * Layout TTL + LRU eviction (§D9)
    //   * fh4 MAC validation between MDS-issued and DS-presented
    //     layouts (ADR-038 §D4.1)
    //   * The recall log that DS subprotocol consults
    //
    // DS endpoints are derived from raft peers (host portion) +
    // ds_addr's port — e.g. raft peer "kiseki-node1:9300" + ds_addr
    // ":2052" → "kiseki-node1:2052". This is what the kernel pNFS
    // client connects to after GETDEVICEINFO.
    let pnfs_layout_mgr: Option<Arc<kiseki_gateway::pnfs::MdsLayoutManager>> = if cfg.pnfs_enabled {
        let cluster_id_bytes: [u8; 16] = bootstrap_tenant.0.into_bytes();
        let mac_key = kiseki_gateway::pnfs::derive_pnfs_fh_mac_key(
            &[0x42; 32], // TODO Phase 15b: pull from kiseki_keymanager
            &cluster_id_bytes,
        );
        let storage_ds_addrs =
            compute_storage_ds_addrs(&cfg.ds_peers, &cfg.raft_peers, cfg.ds_addr);
        let mgr_cfg = kiseki_gateway::pnfs::MdsLayoutConfig {
            stripe_size_bytes: cfg.pnfs.stripe_size_bytes,
            layout_ttl_ms: cfg.pnfs.layout_ttl_seconds.saturating_mul(1000),
            max_entries: cfg.pnfs.layout_cache_max_entries,
            storage_ds_addrs,
            max_stripes_per_layout: cfg.pnfs.max_stripes_per_layout,
        };
        Some(Arc::new(kiseki_gateway::pnfs::MdsLayoutManager::new(
            mac_key, mgr_cfg,
        )))
    } else {
        None
    };

    // Storage nodes for the legacy LayoutManager fallback path. With
    // pnfs_layout_mgr wired (Phase 15c.4), op_layoutget routes via the
    // production manager; this list is unused but kept for back-compat
    // with the test harness that doesn't set the manager.
    let nfs_storage_nodes: Vec<String> = cfg
        .raft_peers
        .iter()
        .map(|(_, addr)| {
            let host = addr.split(':').next().unwrap_or(addr);
            format!("{host}:2052")
        })
        .collect();
    let nfs_listener =
        std::net::TcpListener::bind(nfs_addr).map_err(|e| format!("NFS bind {nfs_addr}: {e}"))?;
    let nfs_tls_for_thread = nfs_tls.clone();
    let pnfs_layout_mgr_for_nfs = pnfs_layout_mgr.clone();
    // Spawn the NFS listener as a tokio task on the main runtime —
    // post-async-native conversion the listener uses tokio::net and
    // each connection becomes a tokio task. No more std::thread::spawn,
    // no more block_on into a dedicated NFS runtime.
    tokio::spawn(async move {
        kiseki_gateway::nfs_server::serve_nfs_listener_with_mgr(
            nfs_listener,
            nfs_gw,
            bootstrap_tenant,
            bootstrap_ns,
            nfs_storage_nodes,
            pnfs_layout_mgr_for_nfs,
            None,
            nfs_tls_for_thread,
        )
        .await;
    });

    // Bug 10 fix: minimal portmapper / RPCBIND listener (RFC 1057).
    // Without this, unmodified Linux `mount -t nfs -o vers=3` clients
    // fail with "Connection refused" before any NFS RPC because the
    // kernel client first hits portmapper on TCP/111 to discover the
    // NFS / MOUNT port. We always advertise `nfs_addr.port()` for both
    // NFS3 and MOUNT3 since both are dispatched off the same NFS
    // listener by program number.
    if let Some(portmap_addr) = cfg.portmap_addr {
        match std::net::TcpListener::bind(portmap_addr) {
            Ok(listener) => {
                let listener = std::sync::Arc::new(listener);
                let advertised_port = nfs_addr.port();
                tracing::info!(
                    addr = %portmap_addr,
                    advertised_nfs_port = advertised_port,
                    "portmapper listening (NFS3 + MOUNT3 → NFS port)",
                );
                std::thread::spawn(move || {
                    kiseki_gateway::portmap::serve_portmap_listener(&listener, advertised_port);
                });
            }
            Err(e) => {
                // Don't fail server startup if 111 is taken by a host
                // rpcbind or unavailable; log and continue. NFSv4 still
                // works without portmapper.
                tracing::warn!(
                    addr = %portmap_addr,
                    error = %e,
                    "portmapper bind failed — NFSv3 mounts will need explicit mountport=",
                );
            }
        }
    }

    // pNFS Data Server listener (ADR-038 §D2). Only spawned when pNFS
    // is enabled AND `ds_addr` is configured. Shares the same
    // MdsLayoutManager instance as the NFS dispatcher above so DS
    // reads can validate fh4 stamps + honor recalls.
    if cfg.pnfs_enabled {
        if let Some(ds_addr) = cfg.ds_addr {
            let mac_key = pnfs_layout_mgr.as_ref().map_or_else(
                || {
                    let cluster_id_bytes: [u8; 16] = bootstrap_tenant.0.into_bytes();
                    kiseki_gateway::pnfs::derive_pnfs_fh_mac_key(&[0x42; 32], &cluster_id_bytes)
                },
                |m| m.current_mac_key(),
            );
            let ds_ctx = Arc::new(kiseki_gateway::pnfs_ds_server::DsContext {
                gateway: Arc::clone(&gw),
                mac_key,
                stripe_size_bytes: cfg.pnfs.stripe_size_bytes,
                rt: tokio::runtime::Handle::current(),
                now_ms: Arc::new(kiseki_gateway::pnfs_ds_server::default_now_ms),
                mds_layout_manager: pnfs_layout_mgr.clone(),
                write_buffers: Arc::new(kiseki_gateway::pnfs_write_buffer::DsWriteBuffers::new()),
            });
            let ds_tls_for_thread = nfs_tls.clone();
            tokio::spawn(async move {
                kiseki_gateway::pnfs_ds_server::run_ds_server(
                    ds_addr,
                    ds_ctx,
                    None,
                    ds_tls_for_thread,
                )
                .await;
            });
            tracing::info!(addr = %ds_addr, "pNFS DS listener spawned");
        }
    }

    // Stream processor: polls deltas from log → advances view watermarks.
    // Uses block_in_place to hold the std::sync::MutexGuard (not Send)
    // while awaiting the async poll(). This is safe because the spawned
    // task runs on a multi-thread runtime with block_in_place support.
    let sp_log = Arc::clone(&log_store);
    let sp_views = Arc::clone(&view_store);
    let sp_view_id = kiseki_common::ids::ViewId(uuid::Uuid::from_u128(1));
    let sp_rt = tokio::runtime::Handle::current();
    tokio::spawn(async move {
        loop {
            tokio::task::block_in_place(|| {
                let mut vs = sp_views.write();
                let mut sp = kiseki_view::stream_processor::TrackedStreamProcessor::new(
                    sp_log.as_ref(),
                    &mut *vs,
                );
                sp.track(sp_view_id);
                sp_rt.block_on(
                    sp.poll(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                    ),
                );
            });
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

    // Phase 16f + multi-shard hydrator (2026-05-18): followers
    // reconstruct their CompositionStore from each per-shard Raft
    // log. A single bootstrap-shard hydrator was wrong for the ADR-033
    // §4 / §1 multi-shard topology — every tenant namespace lives on
    // its own shard, so Create deltas on those shards never made it
    // into followers' local CompositionStore. The `HydratorRegistry`
    // owns one hydrator task per known shard; the control-plane
    // `ShardStoreApplyHook` calls `register(shard_id)` on every
    // `CreateNamespace` / `RecordSplit` commit, so a fresh tenant
    // shard is hydrated immediately after the apply hook installs the
    // local Raft group. Sibling of the view stream processor above;
    // both consume the same delta stream with non-overlapping
    // responsibilities (views: watermarks, compositions: id→metadata).
    if multi_node {
        let hyd_log = Arc::clone(&log_store);
        let hyd_compositions = gw.compositions_handle();
        let hyd_metrics = composition_metrics_for_hydrator;
        // P3 / I-L4: each hydrator poll reports this node's position
        // via the local `report_consumer_position` seam; the shard
        // leader's supervisor gathers all voters' reports and proposes
        // `min` as the replicated `hydrator` watermark, so delta-log
        // pruning can never outrun the slowest node.
        let registry = Arc::new(kiseki_composition::HydratorRegistry::new(
            hyd_compositions,
            hyd_log,
            Some(hyd_metrics),
        ));
        // Pre-register the bootstrap shard so its hydrator starts
        // immediately, before any control-plane CreateNamespace
        // commits land.
        registry.register(bootstrap_shard);
        // Hand the registry to the cluster-control apply hook so
        // every subsequent shard creation also registers a hydrator.
        if let Some(hook) = apply_hook_for_registry.as_ref() {
            hook.attach_hydrator_registry(Arc::clone(&registry));
        }
        // GH #192 (hydrator half): a restart's control-plane log
        // replay can fire `on_create_namespace` BEFORE the attach
        // above — the hook's registry `OnceLock` was empty, so those
        // shards' Raft groups exist but have no hydrator poll loop
        // (followers would silently stop installing Create deltas
        // for tenant namespaces). Drain the state machine's current
        // namespace set and register every known shard. Idempotent
        // and concurrency-safe against applies racing this pass.
        // The namespace registrar half already drained right after
        // gateway construction; re-running it here is a no-op.
        if let Some(ctrl_store) = cluster_control_store.as_ref() {
            ctrl_store
                .rehydrate_gateway_namespaces(gw_namespace_registrar.as_ref(), |shard_id| {
                    registry.register(shard_id);
                })
                .await;
        }
        tracing::info!(
            "composition hydrator registry spawned (bootstrap shard pre-registered; \
             apply hook will register per-namespace shards)",
        );
    }

    // §D10 — periodic stat of the composition store directory so the
    // `kiseki_composition_store_size_bytes` gauge tracks on-disk
    // growth. fjall is a keyspace = directory of LSM segments + WAL,
    // so we recurse one level (cheap — a few dozen files).
    // Only spawned when the persistent store is active. Also refreshes
    // `kiseki_composition_count` from the live store (cheap — backend
    // `count()` does one len call against the comps partition, no
    // full scan).
    if let Some(path) = comp_store_path {
        let size_metrics = composition_metrics_for_size_refresh;
        let count_compositions = gw.compositions_handle();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let total = dir_size_recursive(&path);
                size_metrics
                    .store_size_bytes
                    .set(i64::try_from(total).unwrap_or(i64::MAX));
                if let Ok(c) = count_compositions.with_storage_locked(|s| s.count()) {
                    size_metrics.count.set(i64::try_from(c).unwrap_or(i64::MAX));
                }
            }
        });
    }

    // TODO: Wire rotation_monitor::run_rotation_monitor() here.
    // The components exist (kiseki_keymanager::rotation_monitor + rewrap_worker)
    // but need a real KeyManagerOps reference from the Raft key store.
    // Current key manager is bootstrapped with a fixed key; production
    // requires the Raft-backed OpenRaftKeyStore for distributed rotation.

    // Periodic device scrub (P4c): bitmap vs redb consistency check.
    // Runs every 60 seconds when persistent chunk store is active.
    if cfg.data_dir.is_some() {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                // Scrub runs on the block device layer; report logged if issues found.
                // The actual scrub is performed by DeviceBackend::scrub() which
                // checks bitmap integrity and detects orphan extents.
                tracing::info!("scrub: periodic check completed");
            }
        });
    }

    // Backup manager (ADR-016). Stays None when KISEKI_BACKUP_BACKEND is
    // unset — the admin gRPC service will surface that as "disabled".
    if let Some(ref bcfg) = cfg.backup {
        match crate::backup::init_runtime_backup_manager(bcfg) {
            Ok(_) => tracing::info!(
                retention_days = bcfg.retention_days,
                include_data = bcfg.include_data,
                cleanup_interval_secs = bcfg.cleanup_interval_secs,
                "backup: enabled",
            ),
            Err(e) => tracing::warn!(error = %e, "backup: init failed — backups disabled"),
        }
    } else {
        tracing::info!("backup: disabled (set KISEKI_BACKUP_BACKEND=fs|s3 to enable)");
    }

    // --- gRPC services ---

    // Control plane (ADR-027: Rust-only).
    // The tenant + namespace stores are pre-built above so the admin
    // UI shares the same in-memory state as this gRPC service.
    let control_svc = ControlServiceServer::new(ControlGrpc::with_namespaces(
        Arc::clone(&control_tenants_for_ui),
        Arc::clone(&control_namespaces_for_ui),
    ));
    tracing::info!("control plane: in-process (ControlService on data-path gRPC)");

    let key_svc = KeyManagerServiceServer::new(KeyManagerGrpc::new(key_store));
    // ADR-025 W5: pre-clone the log store handle for the admin
    // service (`SplitShard` / `MergeShards`) before LogGrpc takes
    // ownership.
    let log_for_admin: Arc<dyn kiseki_log::traits::LogOps + Send + Sync> = Arc::clone(&log_store);
    let log_svc = LogServiceServer::new(LogGrpc::new(log_store));
    let admin_svc = kiseki_proto::v1::admin_service_server::AdminServiceServer::new(
        crate::admin_grpc::AdminGrpc::from_runtime(),
    );
    // ADR-025 W2: read-only RPCs (ListPools / GetPool / ListDevices /
    // GetDevice / ClusterStatus / PoolStatus / ListShards / GetShard /
    // ListRepairs) are wired through to the live runtime state.
    // Mutating RPCs still return UNIMPLEMENTED until W3-W7.
    let cluster_member_ids: Vec<u64> = cfg.raft_peers.iter().map(|(id, _)| *id).collect();
    // RepairTracker is constructed empty; the scrub-scheduler /
    // RepairChunk paths (W4) will start writing to it. Keeping the
    // shared handle here means the admin endpoint always returns a
    // stable empty list rather than UNIMPLEMENTED while we wire those.
    let repair_tracker = Arc::new(kiseki_chunk_cluster::repair_tracker::RepairTracker::new());
    // ADR-025 W3: cluster-wide tuning store. Persistent (redb)
    // when KISEKI_DATA_DIR is set, in-memory otherwise. Loaded
    // values rehydrate on boot — server defaults apply only on
    // first install or when persistence is unavailable.
    let tuning_store = if let Some(ref dir) = cfg.data_dir {
        let p = Arc::new(
            crate::tuning::RedbTuningPersistence::open(&dir.join("tuning"))
                .map_err(|e| format!("tuning store open: {e}"))?,
        );
        tracing::info!(path = %dir.join("tuning").display(), "tuning store: persistent (redb)");
        crate::tuning::TuningStore::with_persistence(p)
    } else {
        tracing::info!("tuning store: in-memory (no persistence)");
        crate::tuning::TuningStore::in_memory()
    };
    // ADR-025 W3 live-hook: tuning-change observer. W4/W5 will add
    // per-subsystem subscribers (compaction throttle, scrub
    // scheduler, raft snapshot interval). For W3 we land the wiring
    // skeleton — a single `tracing` subscriber so operators see
    // every SetTuningParams in the audit log even before subsystem
    // hooks are connected. This also keeps `subscribe()` exercised
    // in production so future hooks have a known-working channel.
    {
        let mut rx = tuning_store.subscribe();
        tokio::spawn(async move {
            // Skip the initial value; only log changes.
            let _ = rx.borrow_and_update();
            while rx.changed().await.is_ok() {
                let p = *rx.borrow();
                tracing::info!(
                    compaction_rate_mb_s = p.compaction_rate_mb_s,
                    gc_interval_s = p.gc_interval_s,
                    rebalance_rate_mb_s = p.rebalance_rate_mb_s,
                    scrub_interval_h = p.scrub_interval_h,
                    max_concurrent_repairs = p.max_concurrent_repairs,
                    stream_proc_poll_ms = p.stream_proc_poll_ms,
                    inline_threshold_bytes = p.inline_threshold_bytes,
                    raft_snapshot_interval = p.raft_snapshot_interval,
                    "tuning: SetTuningParams applied"
                );
            }
        });
    }
    // ADR-025 W4 deps. Same `Arc<MaintenanceMode>` is wired into
    // both the storage admin handler (flips the flag) and the
    // ClusterChunkServer (consults it on every PutFragment).
    // EvacuationRegistry is admin-only today; W5's EvacuateDevice
    // will be the producer.
    let maintenance_mode = Arc::new(kiseki_chunk_cluster::maintenance::MaintenanceMode::new());
    let evacuation_registry = Arc::new(kiseki_chunk::evacuation::EvacuationRegistry::new());
    // ADR-025 W5 deps: pool overrides (thresholds + rebalance
    // tracker) and the log-store handle for SplitShard/MergeShards.
    let pool_mutations = crate::pool_overrides::PoolMutationDeps::new();
    // ADR-025 W7: event broker channels for the streaming RPCs.
    // Same handles will be wired into chunk subsystem (DeviceHealth
    // producer) and chunk-cluster (IOStats sampler) as those
    // producers land. Today the channels exist + the admin RPCs
    // can subscribe; events arrive once producers wire up.
    let event_streams = crate::event_streams::EventStreams::new();
    let mut storage_admin_handler = crate::storage_admin::StorageAdminGrpc::from_runtime()
        .with_chunk_store(Arc::clone(&local_chunk_store))
        .with_cluster(cluster_member_ids, cfg.node_id)
        .with_bootstrap_shard(bootstrap_shard)
        .with_repair_tracker(Arc::clone(&repair_tracker))
        .with_tuning_store(tuning_store)
        .with_maintenance(Arc::clone(&maintenance_mode))
        .with_evacuations(Arc::clone(&evacuation_registry))
        .with_pool_mutations(pool_mutations)
        .with_log_store(log_for_admin)
        .with_event_streams(event_streams)
        .with_metrics(Arc::clone(&storage_admin_calls_counter));
    if let (Some(ctrl), Some(raft)) = (
        cluster_control_store.as_ref(),
        raft_shard_store_for_admin.as_ref(),
    ) {
        // ADR-033 §4: route SplitShard / MergeShards through the
        // control-plane Raft group so every node creates the new
        // per-shard Raft group locally on apply. Pass the typed
        // RaftShardStore handle so `initialize_shard` is callable
        // after the control-plane RecordSplit commits.
        // `cfg.fabric_peers` doubles as the admin-RPC peer list
        // because `StorageAdminService` is mounted on the data
        // port — forwarding non-leader admin calls to the leader
        // uses the same address.
        storage_admin_handler = storage_admin_handler.with_cluster_control(
            Arc::clone(ctrl),
            Arc::clone(raft),
            bootstrap_tenant.0.to_string(),
            cfg.fabric_peers.clone(),
            Arc::clone(&cluster_control_metrics_for_admin),
        );
    }
    if let Some(ref s) = scrub_scheduler_handle {
        storage_admin_handler = storage_admin_handler.with_scrub(Arc::clone(s));
    }
    let storage_admin_svc =
        kiseki_proto::v1::storage_admin_service_server::StorageAdminServiceServer::new(
            storage_admin_handler,
        );
    // Phase 16a step 7. The ClusterChunkService gRPC server delegates
    // to the *local* AsyncChunkOps (NOT the ClusteredChunkStore) so a
    // PutFragment from a peer leader stores the fragment on this node
    // without recursing into another fan-out. SAN-role enforcement
    // lives at the interceptor layer; on plaintext (development) the
    // server still functions but rejects cross-node writes only when
    // mTLS is configured (step 12).
    //
    // The interceptor is wired UNCONDITIONALLY when TLS is configured.
    // Otherwise (development plaintext), we install the unwrapped
    // server — the SAN check would always fail with "TLS client info
    // missing" and break local development. The TLS config is
    // mutually exclusive with multi-tenant access on this port, so
    // plaintext-mode is a development-only posture.
    let cluster_chunk_svc_intercepted = cfg.tls.is_some();
    let cluster_chunk_server = kiseki_chunk_cluster::ClusterChunkServer::with_envelope_registry(
        Arc::clone(&local_chunk_store),
        "default",
        envelope_registry.clone(),
    )
    // ADR-025 W4 — same `Arc<MaintenanceMode>` shared with
    // `StorageAdminGrpc` above so the admin-flipped flag is
    // visible on this node's data path.
    .with_maintenance(Arc::clone(&maintenance_mode), bootstrap_shard)
    // Receiver-side fabric phase histograms — every incoming
    // PutFragment observes its decode + write_chunk latency on
    // `kiseki_fabric_put_recv_phase_duration_seconds`.
    .with_metrics(cluster_chunk_server_fabric);

    let mut builder = tonic::transport::Server::builder()
        // HTTP/2 flow-control windows. Match the fabric Channel
        // settings — both peers need to grant a large window for
        // 64+ MiB envelopes to flow without WINDOW_UPDATE round-trip
        // storm. 2026-05-03 GCP fabric quorum-loss root cause.
        .initial_stream_window_size(16 * 1024 * 1024)
        .initial_connection_window_size(32 * 1024 * 1024);

    // Wire mTLS if configured.
    if let Some(ref tls_files) = cfg.tls {
        let tls = build_tls(tls_files)?;
        builder = builder
            .tls_config(tls)
            .map_err(|e| format!("data-path TLS config: {e}"))?;
        tracing::info!(addr = %cfg.data_addr, "data-path gRPC listening (mTLS)");
    } else {
        tracing::warn!(
            addr = %cfg.data_addr,
            "data-path gRPC listening (PLAINTEXT — development only)",
        );
    }

    // Wait for SIGINT (ctrl-c) OR SIGTERM (kill <pid>) so the
    // server cleans up — and on a `--features pprof` build the
    // outer main can render a flamegraph SVG before exit. The BDD
    // harness sends SIGTERM; the profile driver does too.
    let shutdown = async {
        let mut sigterm = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "data-path: SIGTERM listener failed; falling back to SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("data-path: SIGINT received, draining...");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("data-path: SIGINT received, draining...");
            }
            _ = sigterm.recv() => {
                tracing::info!("data-path: SIGTERM received, draining...");
            }
        }
    };

    let mut router = builder
        .add_service(control_svc)
        .add_service(key_svc)
        .add_service(log_svc)
        .add_service(admin_svc)
        .add_service(storage_admin_svc);
    // 2026-06-01: spawn the TCP-framed fabric listener BEFORE moving
    // `cluster_chunk_server` into the tonic Router. Both listeners
    // share the same handler via Arc — a fragment arriving on either
    // transport hits the same `local` ops. The TCP-framed listener
    // is on by default (ADR-042 §2.2); the gRPC listener stays for
    // peer back-compat during rolling upgrades.
    {
        let fabric_tcp_port = data_port.saturating_add(fabric_tcp_port_offset);
        let fabric_tcp_addr: std::net::SocketAddr = format!("0.0.0.0:{fabric_tcp_port}")
            .parse()
            .expect("valid fabric TCP-framed addr");
        let handler = std::sync::Arc::new(cluster_chunk_server.clone_for_tcp_framed_handler());
        // TLS posture matches the gateway TCP-framed listener: API is
        // wired (`TcpFramedFabricListener::with_tls`) but the runtime
        // builds a rustls::ServerConfig from cfg.tls only when set.
        // Plaintext development mode otherwise — same posture the
        // gRPC fabric service takes when cfg.tls is None.
        let listener = match cfg.tls.as_ref() {
            Some(tls_files) => match build_fabric_tls_server_config(tls_files) {
                Ok(sc) => {
                    tracing::info!(
                        addr = %fabric_tcp_addr,
                        "fabric TCP-framed: mTLS enabled (cluster CA chain)",
                    );
                    kiseki_chunk_cluster::TcpFramedFabricListener::with_tls(
                        fabric_tcp_addr,
                        handler,
                        std::sync::Arc::new(sc),
                    )
                }
                Err(e) => {
                    tracing::warn!(
                        addr = %fabric_tcp_addr,
                        error = %e,
                        "fabric TCP-framed: TLS build failed, falling back to plaintext",
                    );
                    kiseki_chunk_cluster::TcpFramedFabricListener::new(fabric_tcp_addr, handler)
                }
            },
            None => kiseki_chunk_cluster::TcpFramedFabricListener::new(fabric_tcp_addr, handler),
        };
        tokio::spawn(async move {
            if let Err(e) = listener.run().await {
                tracing::error!(error = %e, addr = %fabric_tcp_addr, "fabric TCP-framed listener exited");
            }
        });
        tracing::info!(
            addr = %fabric_tcp_addr,
            "fabric TCP-framed listener spawned (ADR-042 §2.2 default for inter-node hop)",
        );
    }
    if cluster_chunk_svc_intercepted {
        router = router.add_service(cluster_chunk_server.into_tonic_server_with_san_check());
        tracing::info!(
            "ClusterChunkService gRPC: SAN-role interceptor active (mTLS) — kept for back-compat"
        );
    } else {
        router = router.add_service(cluster_chunk_server.into_tonic_server());
        tracing::warn!(
            "ClusterChunkService gRPC: NO SAN interceptor (plaintext development mode — \
             cross-node fabric is not protected against tenant certs); TCP-framed is the perf path",
        );
    }
    // ADR-042 Phase 4: register the native GatewayDataService alongside
    // the other data-path services. The interceptor wraps the same
    // tower stack; in plaintext development mode it falls through to a
    // synthetic dev principal (the runtime is single-tenant in that
    // posture). Audit emission uses NullAuditSink today; Phase 4
    // follow-up replaces it with the real `kiseki-audit` adapter.
    let native_audit: std::sync::Arc<dyn kiseki_gateway::native::san_interceptor::AuditSink> =
        std::sync::Arc::new(kiseki_gateway::native::san_interceptor::NullAuditSink);
    let native_intercept = std::sync::Arc::new(
        kiseki_gateway::native::san_interceptor::SanInterceptor::new(
            native_audit,
            cfg.tls.is_some(),
        ),
    );
    // `proxy_client_for_native` was constructed + peer-registered above
    // (before the gateway, so it backs both the gateway's #111
    // append-forwarder and the native server's proxy below).
    let proxy_fallback_enabled = std::env::var("KISEKI_NATIVE_PROXY_FALLBACK")
        .ok()
        .as_deref()
        .is_none_or(|v| matches!(v, "on" | "1" | "true" | "yes"));
    let native_server = std::sync::Arc::new(
        kiseki_gateway::native::ServerImpl::new(
            std::sync::Arc::clone(&gw) as std::sync::Arc<dyn kiseki_gateway::ops::GatewayOps>,
            native_signing_keys,
        )
        .with_proxy_client(std::sync::Arc::clone(&proxy_client_for_native)),
    );
    native_server.set_proxy_fallback_enabled(proxy_fallback_enabled);
    tracing::info!(
        proxy_fallback = proxy_fallback_enabled,
        registered_peers = proxy_client_for_native.registered_nodes().len(),
        "ADR-042 §4 native proxy fallback configured",
    );

    // ADR-042 §3.1 + §16.1 phase 4: the BindingSelector orchestrates
    // the three-phase startup. Each binding ships a probe; the
    // selector probes them all, detects port collisions, applies the
    // operator pin (`KISEKI_NATIVE_TRANSPORT`), and emits a plan in
    // priority order (Rdma > Low > Standard).
    let pin = match kiseki_transport::native::OperatorPin::parse(
        std::env::var("KISEKI_NATIVE_TRANSPORT").ok().as_deref(),
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "KISEKI_NATIVE_TRANSPORT parse failed");
            return Err(std::io::Error::other(e.to_string()).into());
        }
    };
    let mut selector = kiseki_transport::native::BindingSelector::new();
    selector.register(Box::new(kiseki_gateway::native::grpc::GrpcProbe::new(
        cfg.data_addr.to_string(),
    )));
    // Port plan: 9100 data-gRPC, 9101 advisory, 9102 advisory-stream,
    // 9103 native TCP-framed. ADR-042 §2.2 originally specified 9101
    // for TCP-framed which collided with ADR-021's advisory listener
    // (both used `KISEKI_*_ADDR` defaults at 9101). The collision was
    // silent on the 2026-05-07 GCP run — the advisory listener won
    // the race and TCP-framed exited with EADDRINUSE.
    selector.register(Box::new(
        kiseki_gateway::native::tcp_framed::TcpFramedProbe::new("0.0.0.0:9103"),
    ));
    let selector = selector.with_pin(pin);
    let (plan, report) = match selector.plan().await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "native binding selector failed");
            return Err(std::io::Error::other(e.to_string()).into());
        }
    };
    let banner = kiseki_transport::native::selector::render_banner(&plan, &report);
    for line in banner.lines() {
        tracing::info!("{}", line);
    }

    // ADR-042 §1.7: publish the local node's binding set into the
    // TopologyInjector so `GetTopology` carries the per-node
    // BindingEndpoint list every client needs for §3.2 per-edge
    // selection. Single-node deployment: this node's bindings ARE
    // the topology. Multi-node deployments augment via control-
    // plane gossip (a follow-up — once gossip lands, this same
    // helper produces the LOCAL entry while the cluster-wide list
    // is assembled across nodes).
    {
        let local_node_info = kiseki_gateway::native::topology_proto::node_info_from_plan(
            cfg.node_id,
            kiseki_proto::v1::native::NodeState::Active,
            &plan,
        );
        native_server
            .topology()
            .replace(kiseki_gateway::native::server::TopologySnapshot {
                nodes: vec![local_node_info],
                shards_by_tenant: std::collections::HashMap::new(),
            });
    }

    // ADR-042 §1.8: the binding-agnostic handler (`ServerImpl`) is
    // wrapped by per-binding adapters. Same `ServerImpl` instance
    // backs every binding the plan includes — multiple bindings
    // concurrently when the operator hasn't pinned one.
    let mut tcp_framed_listener_to_spawn: Option<
        kiseki_gateway::native::tcp_framed::TcpFramedListener,
    > = None;
    for binding in &plan.spawn_order {
        match binding.binding_id {
            kiseki_proto::native_contract::BindingId::Grpc => {
                let adapter = kiseki_gateway::native::grpc::GrpcAdapter::new(
                    std::sync::Arc::clone(&native_server),
                );
                let native_intercept_for_tonic = std::sync::Arc::clone(&native_intercept);
                let native_inner = kiseki_proto::v1::native::gateway_data_service_server::GatewayDataServiceServer::new(
                    adapter,
                )
                // Streaming PUT can deliver up to the configured
                // per-stream cap (default 64 MiB, env
                // KISEKI_NATIVE_STREAM_CAP overrides). The server-
                // side decoded-message-size default is 4 MiB; raise
                // to match the per-stream cap so a single
                // PutObjectChunk payload doesn't get OutOfRange'd at
                // the codec boundary. Encoding cap covers GET
                // responses with similarly large payloads.
                .max_decoding_message_size(64 * 1024 * 1024)
                .max_encoding_message_size(64 * 1024 * 1024);
                let native_svc = tonic::service::interceptor::InterceptedService::new(
                    native_inner,
                    move |req: tonic::Request<()>| native_intercept_for_tonic.intercept(req),
                );
                router = router.add_service(native_svc);
                tracing::info!(
                    require_tls = cfg.tls.is_some(),
                    "gRPC native binding registered on shared data_addr (ADR-042 §2.1)",
                );
            }
            kiseki_proto::native_contract::BindingId::TcpFramed => {
                let addr_str = if let kiseki_proto::native_contract::ListenAddr::HostPort(s) =
                    &binding.addr
                {
                    s.clone()
                } else {
                    // TCP-framed should always have a host:port —
                    // FabricDescriptor is RDMA-only.
                    tracing::warn!("TCP-framed binding probed with non-HostPort addr; skipping",);
                    continue;
                };
                tcp_framed_listener_to_spawn =
                    Some(kiseki_gateway::native::tcp_framed::TcpFramedListener::new(
                        addr_str.clone(),
                        std::sync::Arc::clone(&native_server),
                        // TLS wiring deferred to a follow-up slice —
                        // listener accepts the same
                        // `Arc<rustls::ServerConfig>` shape the gRPC
                        // binding uses; for now plaintext in dev mode.
                        None,
                        /* allow_plaintext = */ cfg.tls.is_none(),
                    ));
                tracing::info!(
                    addr = %addr_str,
                    require_tls = cfg.tls.is_some(),
                    "TCP-framed native binding registered (ADR-042 §2.2)",
                );
            }
            kiseki_proto::native_contract::BindingId::Ibverbs => {
                tracing::warn!(
                    "ibverbs native binding probed but listener not yet implemented (ADR-042 phase 9)",
                );
            }
            kiseki_proto::native_contract::BindingId::Libfabric { .. } => {
                tracing::warn!(
                    "libfabric native binding probed but listener not yet implemented (ADR-042 phase 10)",
                );
            }
        }
    }

    // Spawn the TCP-framed listener (if any) on the runtime — it
    // owns its socket separately from the cfg.data_addr router.
    if let Some(listener) = tcp_framed_listener_to_spawn {
        tokio::spawn(async move {
            if let Err(e) = listener.run().await {
                tracing::error!(error = %e, "TCP-framed native binding listener exited");
            }
        });
    }

    router.serve_with_shutdown(cfg.data_addr, shutdown).await?;

    tracing::info!("data-path: shut down");
    Ok(())
}

/// Run the advisory runtime on its isolated tokio runtime.
///
/// Starts both the gRPC service (on `addr`) and a TCP stream server
/// (on `stream_addr`) for non-gRPC clients. The TCP stream uses
/// length-prefixed JSON for lightweight hint submission from
/// `kiseki-client` without requiring a tonic dependency.
pub async fn run_advisory(
    addr: SocketAddr,
    stream_addr: SocketAddr,
    tls_files: Option<&TlsFiles>,
    workflow_table: Arc<std::sync::Mutex<kiseki_advisory::WorkflowTable>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let budget = BudgetConfig {
        hints_per_sec: 100,
        max_concurrent_workflows: 10,
        max_phases_per_workflow: 50,
    };

    let advisory_svc = WorkflowAdvisoryServiceServer::new(AdvisoryGrpc::with_table(
        workflow_table,
        budget.clone(),
    ));

    // Shared budget enforcer for the TCP stream server.
    let stream_budget = Arc::new(std::sync::Mutex::new(kiseki_advisory::BudgetEnforcer::new(
        budget,
    )));

    // Start TCP advisory stream server alongside gRPC.
    tokio::spawn(async move {
        if let Err(e) =
            kiseki_advisory::stream::run_advisory_stream_server(stream_addr, stream_budget).await
        {
            tracing::error!(error = %e, "advisory TCP stream server error");
        }
    });

    let mut builder = tonic::transport::Server::builder();

    if let Some(files) = tls_files {
        let tls = build_tls(files)?;
        builder = builder
            .tls_config(tls)
            .map_err(|e| format!("advisory TLS config: {e}"))?;
        tracing::info!(%addr, "advisory gRPC listening (mTLS)");
    } else {
        tracing::warn!(%addr, "advisory gRPC listening (PLAINTEXT — development only)");
    }

    // Wait for SIGINT or SIGTERM. The advisory runtime is awaited
    // by `main` after the data-path completes; if it doesn't honor
    // SIGTERM the binary hangs after a `kill <pid>` and (on
    // `--features pprof` builds) the flamegraph SVG never renders.
    let shutdown = async {
        let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("advisory: SIGINT received, draining...");
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("advisory: SIGINT received, draining...");
            }
            _ = sigterm.recv() => {
                tracing::info!("advisory: SIGTERM received, draining...");
            }
        }
    };

    builder
        .add_service(advisory_svc)
        .serve_with_shutdown(addr, shutdown)
        .await?;

    tracing::info!("advisory: shut down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    /// GH #192 / PR #232: the gateway namespace registrar (a)
    /// registers a control-plane namespace into the gateway's
    /// composition store WITH the command's full creation-time
    /// fidelity (tier policy, size-band pools) so restart replay
    /// restores the creator's policy and writes stop failing with
    /// `NamespaceNotFound`, (b) is idempotent (keep-first: a live
    /// registration — possibly carrying post-create policy updates —
    /// is preserved), and (c) skips legacy non-UUID namespace ids
    /// like the admin handler does.
    #[tokio::test]
    async fn gateway_namespace_registrar_wires_full_fidelity_and_is_idempotent() {
        use crate::cluster_control::NamespaceRegistrar;

        let gw = std::sync::Arc::new(kiseki_gateway::InMemoryGateway::new(
            kiseki_composition::composition::CompositionStore::new(),
            kiseki_chunk::arc_async(kiseki_chunk::store::ChunkStore::new()),
            kiseki_crypto::keys::SystemMasterKey::new(
                [0xAB; 32],
                kiseki_common::tenancy::KeyEpoch(1),
            ),
        ));
        let registrar = super::GatewayNamespaceRegistrar {
            gw: std::sync::Arc::clone(&gw),
        };

        let tenant = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(42));
        let ns_uuid = uuid::Uuid::from_u128(0x192);
        let ns_id = kiseki_common::ids::NamespaceId(ns_uuid);
        let fidelity = crate::cluster_control::NamespaceFidelity {
            tier_policy: vec![kiseki_composition::namespace::TierQuota {
                tier: "fast".to_owned(),
                quota_bytes: 2048,
            }],
            size_band_pools: kiseki_composition::namespace::NamespaceSizeBandPools {
                ec: Some("bulk".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        };

        // Restart-shaped: the namespace exists in the control-plane
        // Raft but not in the (volatile) gateway registry.
        assert!(gw.compositions_handle().namespace(ns_id).is_none());
        registrar.register_namespace(&ns_uuid.to_string(), tenant, &fidelity);
        let ns = gw
            .compositions_handle()
            .namespace(ns_id)
            .expect("registrar registered the namespace");
        assert_eq!(ns.tenant_id, tenant);
        // PR #232: full fidelity from the command — restart replay no
        // longer re-registers with defaults.
        assert_eq!(ns.tier_policy, fidelity.tier_policy);
        assert_eq!(ns.size_band_pools, fidelity.size_band_pools);
        assert!(!ns.read_only);

        // Keep-first: install an updated registration (read_only
        // flipped as the post-create-update marker), re-run the
        // registrar, and assert the existing registration is
        // preserved.
        let mut updated = ns.clone();
        updated.read_only = true;
        gw.add_namespace_sync(updated);
        registrar.register_namespace(&ns_uuid.to_string(), tenant, &fidelity);
        assert!(
            gw.compositions_handle()
                .namespace(ns_id)
                .expect("still registered")
                .read_only,
            "re-registration must be a no-op on an existing namespace",
        );

        // Legacy non-UUID ids: skipped without panicking.
        registrar.register_namespace("not-a-uuid", tenant, &fidelity);
    }

    /// The 5 canonical persistent store paths that the runtime constructs
    /// under `data_dir`. Three are fjall keyspaces (directories), one is
    /// a redb database (small-object inline store), and one is a chunk
    /// device file. All must be in distinct subdirectories under
    /// `data_dir`.
    ///
    /// Layout (from `runtime::run_main`):
    ///   raft/log/           — Raft log shard store (fjall, dir)
    ///   keys/epochs/        — Key manager epochs (fjall, dir)
    ///   chunks/meta/        — Chunk + fragment meta (fjall, dir; ADR-022 rev-4)
    ///   chunks/data.dev     — Raw block device for chunk ciphertext
    ///   small/objects.redb  — Small object inline store (still redb)
    fn canonical_store_paths(data_dir: &std::path::Path) -> [PathBuf; 5] {
        [
            data_dir.join("raft").join("log"),
            data_dir.join("keys").join("epochs"),
            data_dir.join("chunks").join("meta"),
            data_dir.join("small").join("objects.redb"),
            data_dir.join("chunks").join("data.dev"),
        ]
    }

    #[test]
    fn store_layout_paths_are_distinct_and_under_data_dir() {
        let data_dir =
            std::env::temp_dir().join(format!("kiseki-store-layout-test-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();

        let paths = canonical_store_paths(&data_dir);

        // All 5 paths must be distinct.
        let unique: HashSet<&PathBuf> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            5,
            "all 5 store paths must be distinct: {paths:?}"
        );

        // Each path must be under data_dir.
        for path in &paths {
            assert!(
                path.starts_with(&data_dir),
                "store path {path:?} must be under data_dir {data_dir:?}"
            );
        }

        // The remaining redb store keeps the .redb extension; the
        // three fjall keyspaces are directories with no extension;
        // the chunk device file uses a `.dev` extension.
        let redb_path = &paths[3];
        assert_eq!(
            redb_path.extension().and_then(|e| e.to_str()),
            Some("redb"),
            "redb store path must have .redb extension: {redb_path:?}"
        );
        for fjall_path in &paths[..3] {
            assert!(
                fjall_path.extension().is_none(),
                "fjall keyspace path must have no extension: {fjall_path:?}"
            );
        }

        // Subdirectories must collapse to {raft, keys, chunks, small}
        // — chunks/meta + chunks/data.dev share the same first
        // component.
        let subdirs: HashSet<_> = paths
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&data_dir)
                    .ok()
                    .and_then(|rel| rel.components().next())
                    .map(|c| c.as_os_str().to_owned())
            })
            .collect();
        assert_eq!(
            subdirs.len(),
            4,
            "stores collapse to 4 distinct first-level subdirs \
             (chunks/meta + chunks/data.dev share `chunks`): {subdirs:?}"
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// Phase 16 e2e fix: fabric peer addresses must point at the
    /// data-path port (where `ClusterChunkService` listens), not the
    /// Raft port. Pre-fix: `PutFragment` fan-out hit the Raft gRPC
    /// server, returned an unimplemented error, and quorum collapsed
    /// to leader-only.
    #[test]
    fn fabric_addr_remaps_raft_port_to_data_port() {
        assert_eq!(
            super::fabric_addr_from_raft_peer("kiseki-node2:9300", 9100),
            "kiseki-node2:9100",
        );
        assert_eq!(
            super::fabric_addr_from_raft_peer("10.0.0.5:9300", 9100),
            "10.0.0.5:9100",
        );
    }

    /// IPv6 host literals are bracketed. `rsplit_once(':')` keeps the
    /// brackets on the host side, which is the form `tonic::Uri` parses.
    #[test]
    fn fabric_addr_preserves_ipv6_brackets() {
        assert_eq!(
            super::fabric_addr_from_raft_peer("[2001:db8::1]:9300", 9100),
            "[2001:db8::1]:9100",
        );
    }

    /// Defensive — if the caller passed a port-less string we return
    /// it verbatim so the existing log-and-skip branch in `run_main`
    /// fires on the resulting connect error rather than silently
    /// fabricating an address.
    #[test]
    fn fabric_addr_passes_through_when_port_missing() {
        assert_eq!(
            super::fabric_addr_from_raft_peer("kiseki-node2", 9100),
            "kiseki-node2",
        );
    }

    // ---------------------------------------------------------------------
    // compute_storage_ds_addrs — what the MDS will hand out via
    // GETDEVICEINFO. Wrong answers here send pNFS clients to the wrong
    // host:port and every read fails with `Connection refused`.
    // ---------------------------------------------------------------------

    /// `KISEKI_DS_PEERS` wins outright — it's the only mode that can
    /// represent localhost-multi-node where every peer has a distinct
    /// ephemeral DS port.
    /// #103: `KISEKI_PEER_DATA_ADDRS` parses to per-node native-data
    /// endpoints; malformed entries are skipped; absent → empty (caller
    /// falls back to uniform-port derivation).
    #[test]
    fn parse_peer_data_addrs_explicit_and_malformed() {
        assert!(super::parse_peer_data_addrs(None).is_empty());
        assert!(super::parse_peer_data_addrs(Some("")).is_empty());
        let got = super::parse_peer_data_addrs(Some(
            "1=127.0.0.1:41001, 2=127.0.0.1:41002 ,bad,3=,=127.0.0.1:9,4=127.0.0.1:41004",
        ));
        assert_eq!(
            got,
            vec![
                (1, "127.0.0.1:41001".to_string()),
                (2, "127.0.0.1:41002".to_string()),
                (4, "127.0.0.1:41004".to_string()),
            ],
            "explicit entries parsed; `bad` (no =), `3=` (empty addr), `=…` (empty id) skipped"
        );
    }

    #[test]
    fn ds_peers_take_priority() {
        let ds_peers = vec![
            (1, "127.0.0.1:40001".to_string()),
            (2, "127.0.0.1:40002".to_string()),
        ];
        let raft_peers = vec![(1, "127.0.0.1:9301".to_string())];
        let local: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let got = super::compute_storage_ds_addrs(&ds_peers, &raft_peers, Some(local));
        assert_eq!(got, vec!["127.0.0.1:40001", "127.0.0.1:40002"]);
    }

    /// No `ds_peers` but `raft_peers` present → host-substitute each
    /// peer with the local DS port. The container/hostnamed deployment.
    #[test]
    fn raft_peers_substitute_local_ds_port() {
        let raft_peers = vec![
            (1, "kiseki-node1:9300".to_string()),
            (2, "kiseki-node2:9300".to_string()),
        ];
        let local: std::net::SocketAddr = "127.0.0.1:2052".parse().unwrap();
        let got = super::compute_storage_ds_addrs(&[], &raft_peers, Some(local));
        assert_eq!(got, vec!["kiseki-node1:2052", "kiseki-node2:2052"]);
    }

    /// Single-node with no peers but a configured `ds_addr` MUST
    /// advertise that address. Regression: pre-fix this returned an
    /// empty Vec, `MdsLayoutManager` defaulted to `127.0.0.1:2052`, and
    /// every pNFS read failed with `Connection refused` whenever the
    /// real DS bound to a different port (e.g. ephemeral in tests).
    #[test]
    fn single_node_advertises_local_ds() {
        let local: std::net::SocketAddr = "127.0.0.1:40577".parse().unwrap();
        let got = super::compute_storage_ds_addrs(&[], &[], Some(local));
        assert_eq!(got, vec!["127.0.0.1:40577"]);
    }

    /// Single-node with the canonical port still advertises it — the
    /// fix doesn't special-case 2052.
    #[test]
    fn single_node_default_port_still_advertised() {
        let local: std::net::SocketAddr = "127.0.0.1:2052".parse().unwrap();
        let got = super::compute_storage_ds_addrs(&[], &[], Some(local));
        assert_eq!(got, vec!["127.0.0.1:2052"]);
    }

    /// No peers and no `ds_addr` → empty Vec. The caller upstack
    /// (`MdsLayoutManager`) makes its own decision; we don't fabricate.
    #[test]
    fn no_peers_no_local_addr_returns_empty() {
        let got = super::compute_storage_ds_addrs(&[], &[], None);
        assert!(got.is_empty());
    }

    // === ADR-008 rev 2 / ADR-014 — S3 307 peer map for `runtime` ===

    /// Each `raft_peers` entry's host gets the local S3 port
    /// substituted. The 307 `Location:` header in `s3_server` uses
    /// this map to look up the leader hint by `NodeId`.
    #[test]
    fn s3_peer_addrs_substitutes_local_s3_port() {
        let raft_peers = vec![
            (1, "kiseki-node1:9300".to_string()),
            (2, "kiseki-node2:9300".to_string()),
            (3, "kiseki-node3:9300".to_string()),
        ];
        let local_s3: std::net::SocketAddr = "0.0.0.0:9000".parse().unwrap();
        let got = super::compute_s3_peer_addrs(&raft_peers, local_s3);
        assert_eq!(got.len(), 3);
        assert_eq!(got.get(&1).map(String::as_str), Some("kiseki-node1:9000"));
        assert_eq!(got.get(&2).map(String::as_str), Some("kiseki-node2:9000"));
        assert_eq!(got.get(&3).map(String::as_str), Some("kiseki-node3:9000"));
    }

    /// Single-node / empty `raft_peers` → empty peer map. The 307
    /// helper in `s3_server` falls back to 503 + `Retry-After` in
    /// that case (no peer to redirect to).
    #[test]
    fn s3_peer_addrs_empty_raft_peers_returns_empty_map() {
        let local_s3: std::net::SocketAddr = "0.0.0.0:9000".parse().unwrap();
        let got = super::compute_s3_peer_addrs(&[], local_s3);
        assert!(got.is_empty());
    }

    /// Raft peer entry that has no port colon still parses cleanly:
    /// the whole string is treated as the host. Defensive against
    /// misconfigured peer lists (the 307 path's peer-map lookup
    /// works against any string; bad host → DNS / connect failure
    /// at the client, not a panic on the server).
    #[test]
    fn s3_peer_addrs_handles_hostonly_entry() {
        let raft_peers = vec![(1, "kiseki-node1".to_string())];
        let local_s3: std::net::SocketAddr = "0.0.0.0:9000".parse().unwrap();
        let got = super::compute_s3_peer_addrs(&raft_peers, local_s3);
        assert_eq!(got.get(&1).map(String::as_str), Some("kiseki-node1:9000"));
    }
}
