//! REST API endpoints for the admin web UI.
//!
//! All endpoints return JSON or HTML fragments. HTMX polls these for live updates.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;

use super::aggregator::MetricsAggregator;
use super::auth::{admin_required, cluster_info_required, AuthConfig};
use super::events::SharedDiagnostics;

/// Shared state for API handlers.
#[derive(Clone)]
pub struct UiState {
    /// Metrics aggregator for cluster-wide view.
    pub aggregator: Arc<MetricsAggregator>,
    /// Function to encode local Prometheus metrics.
    pub metrics_encode: Arc<dyn Fn() -> String + Send + Sync>,
    /// Diagnostic store for metric history + events.
    pub diagnostics: SharedDiagnostics,
    /// Log store for shard health / leader queries.
    pub log_store: Option<Arc<dyn kiseki_log::LogOps + Send + Sync>>,
    /// This node's identity.
    pub node_info: NodeInfo,
    /// Shared composition store handle (ADR-040 / I-2): the per-shard
    /// leader endpoint surfaces the hydrator's halt flag from here so
    /// load balancers can route around a halted node.
    pub compositions: Option<Arc<kiseki_composition::composition::CompositionStore>>,
    /// Local chunk store — `/admin/chunk/{id}` reports per-node fragment
    /// presence by calling `list_fragments` on this handle. Operators
    /// use the endpoint to debug placement / GC / under-replication.
    pub local_chunk_store: Option<Arc<dyn kiseki_chunk::AsyncChunkOps>>,
    /// Cluster-control state machine handle (ADR-033 §4): exposes the
    /// per-namespace shard maps the `/cluster/info` `shards` field
    /// (ADR-008 rev 2) projects. `None` on single-node deployments.
    pub cluster_control: Option<Arc<crate::cluster_control::ControlStateMachine>>,
    /// Writable handle to the control-plane Raft store. Lets admin
    /// HTTP routes submit `ControlCommand`s (e.g. `CreateNamespace`
    /// for #68's multi-shard namespace endpoint). `None` on single-
    /// node deployments — paired with `cluster_control` above.
    pub cluster_control_store: Option<Arc<crate::cluster_control::OpenRaftControlStore>>,
    /// In-process audit log. The Audit dashboard tab + `kiseki-admin
    /// audit query` read from here.
    pub audit: Option<super::admin_extra::AuditHandle>,
    /// Key manager. Powers `kiseki-admin keys status / keys rotate`.
    pub key_manager: Option<super::admin_extra::KeyManagerHandle>,
    /// Tenant store — orgs, projects, workloads.
    pub tenants: Option<super::admin_extra::TenantHandle>,
    /// Namespace store — exposed for the Tenants tab's namespace list.
    pub namespaces: Option<super::admin_extra::NamespaceHandle>,
    /// Drain orchestrator — `kiseki-admin drain {start,cancel,status}`.
    pub drain: Option<super::admin_extra::DrainHandle>,
}

/// Static node identity exposed via `/cluster/info`.
#[derive(Clone, serde::Serialize)]
pub struct NodeInfo {
    pub node_id: u64,
    pub s3_addr: String,
    pub nfs_addr: String,
    pub metrics_addr: String,
    pub raft_peers: Vec<(u64, String)>,
}

/// One row in the `/cluster/info` `shards` array (ADR-008 rev 2).
///
/// ADR-033 §4 holds the source-of-truth `NamespaceShardMap` on the
/// control-plane Raft group; this wire shape projects each shard onto
/// the JSON contract clients consume to populate their topology cache
/// without going through gRPC `GetTopology` (ADR-042 §4) first.
///
/// Field semantics (matching ADR-008 rev 2 §"Wire shape"):
///
/// - `shard_id`: UUID string form, matching ADR-033 §4 / ADR-042 §1.
/// - `leader_id`: `NodeId` u64. `None` when the responding node has
///   not yet observed a leader (cold start, mid-election).
/// - `leader_data_addr`: `host:port` of the leader's native gateway
///   (`KISEKI_DATA_ADDR`, default 9100). `None` when the
///   responding node cannot resolve the leader's address.
/// - `range_start` / `range_end`: hex-encoded 32-byte hashed-key
///   bounds, matching `NamespaceShardMap.ShardRange` (ADR-033 §4).
///   `range_end = "0xFF…FF"` is the inclusive upper bound for the
///   last shard.
/// - `namespace_id`: the namespace the shard belongs to. Surfaced so
///   multi-namespace clusters can route per-namespace without an
///   extra round-trip.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ShardInfoJson {
    /// UUID string form.
    pub shard_id: String,
    /// Owning namespace.
    pub namespace_id: String,
    /// Best-effort leader's `NodeId`.
    #[serde(default)]
    pub leader_id: Option<u64>,
    /// Best-effort leader's data-port address (`host:port`).
    #[serde(default)]
    pub leader_data_addr: Option<String>,
    /// Hex-encoded 32-byte inclusive lower bound, prefixed `0x`.
    pub range_start: String,
    /// Hex-encoded 32-byte exclusive upper bound, prefixed `0x`.
    pub range_end: String,
}

/// Full JSON shape of `/cluster/info` as of ADR-008 rev 2.
///
/// Defined as a typed struct (not just `serde_json::json!`) so
/// `kiseki-client::discovery` and the BDD/unit-test surfaces can
/// share the deserialization path. The handler still constructs the
/// response value, but lives behind this contract.
///
/// `peers` and `node_info` fields stay flat for ADR-008 rev-1
/// compatibility (older clients ignore unknown fields).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClusterInfoResponse {
    /// This node's `NodeId`.
    pub node_id: u64,
    /// This node's S3 address (`host:port`).
    pub s3_addr: String,
    /// This node's NFS address (`host:port`).
    pub nfs_addr: String,
    /// This node's metrics address (`host:port`).
    pub metrics_addr: String,
    /// Bootstrap-shard leader id (rev-1 retained). Per ADR-008 rev 2
    /// `shards` is the authoritative per-shard map; this remains for
    /// older clients.
    #[serde(default)]
    pub leader_id: Option<u64>,
    /// Bootstrap-shard leader S3 address (rev-1 retained).
    #[serde(default)]
    pub leader_s3: Option<String>,
    /// Cluster peers — id + addresses for every replica.
    pub peers: Vec<PeerInfoJson>,
    /// ADR-008 rev 2: per-shard leader map. Empty when this node has
    /// not yet observed any namespace's shard map (cold start, no
    /// control-plane connectivity, single-node compose).
    #[serde(default)]
    pub shards: Vec<ShardInfoJson>,
}

/// Per-peer record on `/cluster/info` `peers[]`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PeerInfoJson {
    /// Peer `NodeId`.
    pub id: u64,
    /// Raft address (`host:raft_port`).
    pub raft_addr: String,
    /// S3 address (`host:9000`).
    pub s3_addr: String,
    /// NFS address (`host:2049`).
    pub nfs_addr: String,
    /// Metrics address (`host:9090`).
    pub metrics_addr: String,
}

/// Build the web UI router.
///
/// Three auth tiers, all sharing the same `UiState`:
///
/// 1. **admin tier** — `/admin/*` plus `/ui/*` (dashboard HTML +
///    fragment polls). Gated by `admin_required` middleware: requires
///    `KISEKI_ADMIN_TOKEN`. `KISEKI_ADMIN_AUTH_DISABLED=true` opts
///    out for dev / single-machine compose. ADR-008 rev 2
///    §"Authorization" + UI/CLI follow-up D1.
/// 2. **cluster-info tier** — just `/cluster/info` +
///    `/cluster/shards/{id}/leader`. Gated by
///    `cluster_info_required`: requires admin OR client token.
///    `KISEKI_CLUSTER_INFO_PUBLIC=true` reverts to the historical
///    open posture (LB probes, public topology).
/// 3. **open tier** — none today; `/health`, `/metrics`, `/ui/logo`
///    live on the outer router in `metrics.rs` and stay open.
pub fn ui_router(state: UiState, auth: AuthConfig) -> Router {
    // Admin tier: all /admin/* and /ui/* (UI fragments are admin-only
    // because they expose the same cluster-wide stats as /admin/*).
    let admin_routes = Router::new()
        .route("/ui", get(dashboard_page))
        .route("/ui/", get(dashboard_page))
        .route("/ui/api/cluster", get(api_cluster_summary))
        .route("/ui/api/nodes", get(api_nodes))
        .route("/ui/api/history", get(api_history))
        .route("/ui/api/events", get(api_events))
        .route("/ui/fragment/cluster-cards", get(fragment_cluster_cards))
        .route("/ui/fragment/node-table", get(fragment_node_table))
        .route("/ui/fragment/chart-data", get(fragment_chart_data))
        .route("/ui/fragment/alerts", get(fragment_alerts))
        .route("/ui/api/ops/maintenance", post(ops_maintenance))
        .route("/ui/api/ops/backup", post(ops_backup))
        .route("/ui/api/ops/scrub", post(ops_scrub))
        .route("/admin/chunk/{chunk_id}", get(admin_inspect_chunk))
        .route(
            "/admin/composition/{composition_id}",
            get(admin_inspect_composition),
        )
        // Test-only knobs (gated by `KISEKI_ENABLE_TEST_KNOBS=1` at
        // request time). Used by BDD scenarios that need
        // deterministic fault injection without iptables/netfilter:
        //   POST /admin/test/fabric/slow-ms/{ms}     — set per-RPC sleep on incoming PutFragment
        //   POST /admin/test/fabric/deny-incoming/{0|1}  — refuse all incoming PutFragment
        //   DELETE /admin/test/chunk/{id}/fragment/{idx} — drop a single
        //                                                  local fragment file
        //   POST /admin/test/shard/{id}/advance-watermark/{pos} — drive the
        //                                                  replicated AdvanceWatermark (P3a)
        .route(
            "/admin/test/fabric/slow-ms/{ms}",
            post(admin_test_fabric_slow),
        )
        .route(
            "/admin/test/fabric/deny-incoming/{enabled}",
            post(admin_test_fabric_deny),
        )
        .route(
            "/admin/test/chunk/{chunk_id}/fragment/{fragment_index}",
            axum::routing::delete(admin_test_drop_fragment),
        )
        .route(
            "/admin/test/shard/{shard_id}/advance-watermark/{position}",
            post(admin_test_advance_watermark),
        )
        .merge(super::admin_extra::admin_extra_routes())
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            admin_required,
        ));

    // Cluster-info tier: any-authenticated principal (admin OR
    // client token). Per-shard leader probes share this tier because
    // they expose the same per-shard routing data clients need for
    // bootstrap and stale-leader retry per ADR-008 rev 2.
    let cluster_info_routes = Router::new()
        .route("/cluster/info", get(cluster_info))
        .route("/cluster/shards/{shard_id}/leader", get(shard_leader))
        .layer(axum::middleware::from_fn_with_state(
            auth,
            cluster_info_required,
        ));

    admin_routes.merge(cluster_info_routes).with_state(state)
}

async fn api_cluster_summary(State(state): State<UiState>) -> impl IntoResponse {
    let metrics_text = (state.metrics_encode)();
    state.aggregator.update_local(metrics_text).await;
    let summary = state.aggregator.cluster_summary().await;
    axum::Json(summary)
}

/// Query params for history endpoint.
#[derive(serde::Deserialize)]
struct HistoryParams {
    /// Number of hours to retrieve. Default: 3.
    hours: Option<f64>,
}

async fn api_history(
    State(state): State<UiState>,
    axum::extract::Query(params): axum::extract::Query<HistoryParams>,
) -> impl IntoResponse {
    let hours = params.hours.unwrap_or(3.0);
    let diag = state.diagnostics.read().await;
    let points = diag.metrics.since_hours(hours);
    axum::Json(serde_json::json!({
        "hours": hours,
        "points": points,
    }))
}

/// Query params for events endpoint.
#[derive(serde::Deserialize)]
struct EventParams {
    /// Filter by severity: info, warning, error, critical.
    severity: Option<String>,
    /// Filter by category: node, shard, device, tenant, security, admin.
    category: Option<String>,
    /// Hours to look back. Default: 3.
    hours: Option<f64>,
    /// Maximum events to return. Default: 100.
    limit: Option<usize>,
}

async fn api_events(
    State(state): State<UiState>,
    axum::extract::Query(params): axum::extract::Query<EventParams>,
) -> impl IntoResponse {
    use super::events::{Category, Severity};

    let hours = params.hours.unwrap_or(3.0);
    let severity = params.severity.as_deref().and_then(|s| match s {
        "info" => Some(Severity::Info),
        "warning" => Some(Severity::Warning),
        "error" => Some(Severity::Error),
        "critical" => Some(Severity::Critical),
        _ => None,
    });
    let category = params.category.as_deref().and_then(|c| match c {
        "node" => Some(Category::Node),
        "shard" => Some(Category::Shard),
        "device" => Some(Category::Device),
        "tenant" => Some(Category::Tenant),
        "security" => Some(Category::Security),
        "admin" => Some(Category::Admin),
        "gateway" => Some(Category::Gateway),
        "raft" => Some(Category::Raft),
        _ => None,
    });

    let diag = state.diagnostics.read().await;
    let events = diag.events.query(severity, category, hours);
    let limit = params.limit.unwrap_or(100);
    let events: Vec<_> = events.into_iter().rev().take(limit).collect();

    axum::Json(serde_json::json!({
        "count": events.len(),
        "events": events,
    }))
}

async fn api_nodes(State(state): State<UiState>) -> impl IntoResponse {
    let metrics_text = (state.metrics_encode)();
    state.aggregator.update_local(metrics_text).await;
    let nodes = state.aggregator.all_snapshots().await;
    axum::Json(nodes)
}

async fn fragment_cluster_cards(State(state): State<UiState>) -> Html<String> {
    let metrics_text = (state.metrics_encode)();
    state.aggregator.update_local(metrics_text).await;
    let summary = state.aggregator.cluster_summary().await;

    let health_class = if summary.healthy_nodes == summary.total_nodes {
        "healthy"
    } else if summary.healthy_nodes > 0 {
        "degraded"
    } else {
        "down"
    };

    Html(format!(
        r#"<div class="card {health_class}"><h3>Cluster Health</h3><div class="big-number">{}/{}</div><div class="label">nodes healthy</div></div>
<div class="card"><h3>Raft Entries</h3><div class="big-number">{}</div><div class="label">total applied</div></div>
<div class="card"><h3>Gateway Requests</h3><div class="big-number">{}</div><div class="label">total served</div></div>
<div class="card"><h3>Data Written</h3><div class="big-number">{}</div><div class="label">chunk bytes</div></div>
<div class="card"><h3>Data Read</h3><div class="big-number">{}</div><div class="label">chunk bytes</div></div>
<div class="card"><h3>Connections</h3><div class="big-number">{}</div><div class="label">active transport</div></div>"#,
        summary.healthy_nodes,
        summary.total_nodes,
        format_number(summary.aggregate.raft_entries),
        format_number(summary.aggregate.gateway_requests),
        format_bytes(summary.aggregate.chunk_write_bytes),
        format_bytes(summary.aggregate.chunk_read_bytes),
        summary.aggregate.transport_connections,
    ))
}

#[allow(clippy::items_after_statements)]
async fn fragment_node_table(State(state): State<UiState>) -> Html<String> {
    use std::fmt::Write;
    let metrics_text = (state.metrics_encode)();
    state.aggregator.update_local(metrics_text).await;
    let nodes = state.aggregator.all_snapshots().await;
    let mut html = String::from(
        "<table><thead><tr><th>Node</th><th>Status</th><th>Raft</th><th>Requests</th><th>Written</th><th>Read</th><th>Conns</th></tr></thead><tbody>",
    );
    for node in &nodes {
        let badge = if node.healthy {
            r#"<span class="badge healthy">Healthy</span>"#
        } else {
            r#"<span class="badge down">Unreachable</span>"#
        };
        let _ = write!(
            html,
            "<tr data-addr=\"{0}\"><td>{0}</td><td>{badge}</td><td>{1}</td><td>{2}</td><td>{3}</td><td>{4}</td><td>{5}</td></tr>",
            node.address,
            format_number(node.summary.raft_entries),
            format_number(node.summary.gateway_requests),
            format_bytes(node.summary.chunk_write_bytes),
            format_bytes(node.summary.chunk_read_bytes),
            node.summary.transport_connections,
        );
    }
    html.push_str("</tbody></table>");
    Html(html)
}

async fn fragment_chart_data(State(state): State<UiState>) -> impl IntoResponse {
    let metrics_text = (state.metrics_encode)();
    state.aggregator.update_local(metrics_text).await;
    let nodes = state.aggregator.all_snapshots().await;

    let labels: Vec<&str> = nodes.iter().map(|n| n.address.as_str()).collect();
    let writes: Vec<u64> = nodes.iter().map(|n| n.summary.chunk_write_bytes).collect();
    let reads: Vec<u64> = nodes.iter().map(|n| n.summary.chunk_read_bytes).collect();
    let requests: Vec<u64> = nodes.iter().map(|n| n.summary.gateway_requests).collect();

    axum::Json(serde_json::json!({
        "labels": labels,
        "writes": writes,
        "reads": reads,
        "requests": requests,
    }))
}

#[allow(clippy::items_after_statements)]
async fn fragment_alerts(State(state): State<UiState>) -> Html<String> {
    use std::fmt::Write;
    let metrics_text = (state.metrics_encode)();
    state.aggregator.update_local(metrics_text).await;
    let nodes = state.aggregator.all_snapshots().await;
    let now = chrono_lite();

    let mut html = String::new();

    // Check for unhealthy nodes.
    let unhealthy: Vec<_> = nodes.iter().filter(|n| !n.healthy).collect();
    if unhealthy.is_empty() {
        let _ = write!(
            html,
            r#"<div class="alert-row"><span class="dot green"></span><span class="msg">All {} nodes healthy</span><span class="time">{now}</span></div>"#,
            nodes.len()
        );
    } else {
        for n in &unhealthy {
            let _ = write!(
                html,
                r#"<div class="alert-row"><span class="dot red"></span><span class="msg">Node <b>{}</b> unreachable</span><span class="time">{now}</span></div>"#,
                n.address
            );
        }
    }

    let _ = write!(
        html,
        r#"<div class="alert-row"><span class="dot blue"></span><span class="msg">Capacity monitoring active ({} nodes reporting)</span><span class="time">{now}</span></div>"#,
        nodes.len()
    );

    for n in &nodes {
        if n.summary.gateway_requests > 0 {
            let _ = write!(
                html,
                r#"<div class="alert-row"><span class="dot green"></span><span class="msg">{}: {} gateway requests served</span><span class="time">{now}</span></div>"#,
                n.address,
                format_number(n.summary.gateway_requests)
            );
        }
    }

    if html.is_empty() {
        html.push_str(r#"<div class="alert-row"><span class="dot green"></span><span class="msg">No alerts</span></div>"#);
    }

    Html(html)
}

// --- Operations endpoints ---

#[derive(serde::Deserialize)]
struct MaintenanceParams {
    enabled: bool,
}

async fn ops_maintenance(
    State(state): State<UiState>,
    axum::Json(params): axum::Json<MaintenanceParams>,
) -> impl IntoResponse {
    let msg = if params.enabled {
        "Maintenance mode enabled"
    } else {
        "Maintenance mode disabled"
    };
    let mut diag = state.diagnostics.write().await;
    diag.events
        .info(super::events::Category::Admin, "admin-ui", msg);
    axum::Json(serde_json::json!({"status": "ok", "message": msg}))
}

async fn ops_backup(State(state): State<UiState>) -> impl IntoResponse {
    let mut diag = state.diagnostics.write().await;
    diag.events.info(
        super::events::Category::Admin,
        "admin-ui",
        "Backup requested",
    );
    axum::Json(serde_json::json!({"status": "ok", "message": "Backup initiated (background)"}))
}

async fn ops_scrub(State(state): State<UiState>) -> impl IntoResponse {
    let mut diag = state.diagnostics.write().await;
    diag.events.info(
        super::events::Category::Admin,
        "admin-ui",
        "Scrub requested",
    );
    axum::Json(serde_json::json!({"status": "ok", "message": "Scrub initiated (background)"}))
}

/// Cluster info: this node's identity, leader, and peer map.
///
/// Benchmark scripts and clients use this to discover the Raft leader
/// and route writes to the correct node's S3/NFS endpoint. ADR-008
/// rev 2 adds the `shards: [...]` top-level array so native clients
/// learn per-shard leaders on bootstrap (without needing gRPC
/// `GetTopology`, which they can't reach until the topology cache is
/// primed).
async fn cluster_info(State(state): State<UiState>) -> impl IntoResponse {
    let bootstrap_shard = kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1));

    let (leader_id, leader_s3) = if let Some(ref log) = state.log_store {
        match log.shard_health(bootstrap_shard).await {
            Ok(info) => {
                let lid = info.leader.map(|n| n.0);
                // Resolve leader's S3 address from the peer list.
                let leader_s3 = lid.and_then(|id| {
                    state
                        .node_info
                        .raft_peers
                        .iter()
                        .find(|(pid, _)| *pid == id)
                        .map(|(_, addr)| {
                            // Raft addr is host:raft_port → S3 is host:9000
                            let host = addr.split(':').next().unwrap_or("127.0.0.1");
                            format!("{host}:9000")
                        })
                });
                (lid, leader_s3)
            }
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };

    let metrics_port = state
        .node_info
        .metrics_addr
        .split(':')
        .next_back()
        .unwrap_or("9090")
        .to_owned();

    let peers: Vec<PeerInfoJson> = state
        .node_info
        .raft_peers
        .iter()
        .map(|(id, addr)| {
            let host = addr.split(':').next().unwrap_or("127.0.0.1");
            PeerInfoJson {
                id: *id,
                raft_addr: addr.clone(),
                s3_addr: format!("{host}:9000"),
                nfs_addr: format!("{host}:2049"),
                metrics_addr: format!("{host}:{metrics_port}"),
            }
        })
        .collect();

    // ADR-008 rev 2 `shards: [...]` — populated from the control-plane
    // state machine when present, else empty (operational degradation
    // path: rev-1 clients fall back to seed-only routing).
    let shards = build_shards_from_state(&state).await;

    axum::Json(ClusterInfoResponse {
        node_id: state.node_info.node_id,
        s3_addr: state.node_info.s3_addr.clone(),
        nfs_addr: state.node_info.nfs_addr.clone(),
        metrics_addr: state.node_info.metrics_addr.clone(),
        leader_id,
        leader_s3,
        peers,
        shards,
    })
}

/// ADR-008 rev 2 — project the control-plane `NamespaceShardMap`s onto
/// the wire-shape `Vec<ShardInfoJson>`. Returns an empty Vec when the
/// control-plane state machine is not wired in (single-node compose,
/// rev-1 deploys, BDD harnesses) — clients honour the empty list as
/// "fall back to seed-only routing" per ADR-008 rev 2 §"Compatibility".
async fn build_shards_from_state(state: &UiState) -> Vec<ShardInfoJson> {
    let Some(cluster_control) = state.cluster_control.as_ref() else {
        return Vec::new();
    };
    let snapshot = cluster_control.snapshot().await;

    // Build the (node_id → "host:port") map for `leader_data_addr`
    // resolution. Convention: native data port is 9100 (matches
    // `KISEKI_DATA_ADDR` default + `node_info_from_plan`).
    let peer_data_addrs: std::collections::HashMap<u64, String> = state
        .node_info
        .raft_peers
        .iter()
        .map(|(id, addr)| {
            let host = addr.split(':').next().unwrap_or("127.0.0.1");
            (*id, format!("{host}:9100"))
        })
        .collect();

    let mut shards = Vec::new();
    for (namespace_id, ns_snapshot) in snapshot.namespaces {
        for shard in ns_snapshot.shards {
            if shard.is_retiring {
                // Skip retired shards: ADR-033 §4 says routing should
                // ignore them once the merge has absorbed the range.
                continue;
            }
            let leader_id = shard.leader_node.0;
            let leader_data_addr = peer_data_addrs.get(&leader_id).cloned();
            shards.push(ShardInfoJson {
                shard_id: shard.shard_id.0.to_string(),
                namespace_id: namespace_id.clone(),
                leader_id: Some(leader_id),
                leader_data_addr,
                range_start: encode_hex_prefixed(&shard.range_start),
                range_end: encode_hex_prefixed(&shard.range_end),
            });
        }
    }
    // Sort by namespace_id + range_start so the order is
    // deterministic across nodes (HashMap iteration order is not).
    shards.sort_by(|a, b| {
        a.namespace_id
            .cmp(&b.namespace_id)
            .then_with(|| a.range_start.cmp(&b.range_start))
    });
    shards
}

/// Encode a 32-byte hashed-key bound as a `0x`-prefixed lowercase
/// hex string. Matches the wire format documented in ADR-008 rev 2
/// §"Wire shape".
fn encode_hex_prefixed(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(2 + 64);
    s.push_str("0x");
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Public re-export of `build_shards_from_state` for cross-crate tests
/// (kiseki-acceptance BDD step impls). Same body, public visibility.
#[allow(clippy::unused_async)]
pub async fn build_shards_for_test(state: &UiState) -> Vec<ShardInfoJson> {
    build_shards_from_state(state).await
}

/// Per-shard leader info (Phase 17 item 4).
///
/// `cluster/info` reports a cluster-level `leader_id` derived from the
/// bootstrap shard, but Raft elections are per-shard: a write to a
/// non-bootstrap shard can fail with `LeaderUnavailable: ShardId(X)`
/// even when `cluster/info` shows a healthy leader for shard 1.
/// Clients (and tests) that need to know "is shard X writable right
/// now?" should poll this endpoint.
///
/// Returns 404 if the shard isn't known on this node (the common
/// non-error reason — the requesting client is asking the wrong node;
/// the proper response is to retry against another peer).
async fn shard_leader(
    State(state): State<UiState>,
    axum::extract::Path(shard_id_str): axum::extract::Path<String>,
) -> impl IntoResponse {
    let Ok(uuid) = uuid::Uuid::parse_str(&shard_id_str) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "shard_id must be a UUID"})),
        );
    };
    let shard_id = kiseki_common::ids::ShardId(uuid);
    let Some(ref log) = state.log_store else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "log store not initialized"})),
        );
    };
    // ADR-040 §D6.3 / Phase 17 I-2 (amended I-CP5b, issue #87 PR-2):
    // surface the composition hydrator's per-shard halt flag so load
    // balancers and clients can route around a halt on this specific
    // shard without taking the whole node out of rotation.
    let composition_halted = if let Some(ref comps) = state.compositions {
        comps.with_storage_locked(|s| s.halted(shard_id).unwrap_or(false))
    } else {
        false
    };

    match log.shard_health(shard_id).await {
        Ok(info) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "shard_id": info.shard_id.0.to_string(),
                "leader_id": info.leader.map(|n| n.0),
                "raft_members": info.raft_members.iter().map(|n| n.0).collect::<Vec<_>>(),
                "last_committed_seq": info.tip.0,
                "state": format!("{:?}", info.state),
                "composition_hydrator_halted": composition_halted,
            })),
        ),
        Err(e) => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `GET /admin/chunk/{chunk_id}` — debug a single chunk's cluster
/// state and per-node fragment presence.
///
/// `chunk_id` is the 64-hex-char content-addressed chunk identifier
/// (the 32-byte HMAC). Returns the row this node holds in
/// `cluster_chunk_state` (refcount + placement + tombstone bit) and
/// the indices of fragments present in the local chunk store.
/// Operators (and the BDD acceptance suite) query each node and merge
/// the results to reason about replication and GC. Read-only; reads
/// the *local* Raft state-machine view — followers may report a
/// slightly stale `cluster_chunk_state` while their hydrator catches
/// up.
async fn admin_inspect_chunk(
    State(state): State<UiState>,
    axum::extract::Path(chunk_id_str): axum::extract::Path<String>,
) -> impl IntoResponse {
    let Some(chunk_id) = parse_chunk_id_hex(&chunk_id_str) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "chunk_id must be 64 hex characters (32-byte HMAC)",
            })),
        );
    };
    // Bootstrap shard + tenant — every cluster_chunk_state row in the
    // current build is keyed under these. When multi-tenant clusters
    // ship, the endpoint should accept ?tenant=... and ?shard=... query
    // params; for now operators only have one shard.
    let shard_id = kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1));
    let tenant_id = kiseki_common::ids::OrgId(uuid::Uuid::from_u128(1));

    let cluster_state = if let Some(ref log) = state.log_store {
        match log
            .cluster_chunk_state_get(shard_id, tenant_id, chunk_id)
            .await
        {
            Ok(Some(entry)) => Some(serde_json::json!({
                "refcount": entry.refcount,
                "placement": entry.placement,
                "tombstoned": entry.tombstoned,
                "created_ms": entry.created_ms,
                "original_len": entry.original_len,
            })),
            _ => None,
        }
    } else {
        None
    };

    let (has_chunk, fragments_local) = match state.local_chunk_store.as_ref() {
        Some(store) => {
            let has = store.list_chunk_ids().await.contains(&chunk_id);
            // EC mode tracks fragment indices separately; Replication
            // mode stores the whole chunk under one key (so this is
            // empty even when has_chunk == true). Both are useful for
            // operators — emit both.
            let frags = store.list_fragments(&chunk_id).await;
            (has, frags)
        }
        None => (false, Vec::new()),
    };

    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "node_id": state.node_info.node_id,
            "chunk_id": chunk_id_str,
            "cluster_state": cluster_state,
            "has_chunk_local": has_chunk,
            "fragments_local": fragments_local,
        })),
    )
}

/// `GET /admin/composition/{composition_id}` — return the chunk-id
/// list for a given composition. Used by tooling to chain into
/// `/admin/chunk/{chunk_id}` (operators rarely have a chunk id at
/// hand; they have the S3 etag, which is the composition id).
async fn admin_inspect_composition(
    State(state): State<UiState>,
    axum::extract::Path(comp_id_str): axum::extract::Path<String>,
) -> impl IntoResponse {
    use kiseki_composition::CompositionOps as _;
    let Ok(uuid) = uuid::Uuid::parse_str(&comp_id_str) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "composition_id must be a UUID"})),
        );
    };
    let comp_id = kiseki_common::ids::CompositionId(uuid);
    let Some(ref store) = state.compositions else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "composition store not initialized"})),
        );
    };
    // P4 (#226) read-coherence on the inspection path: rows created
    // at ack on async surfaces live in the CompositionStore's
    // volatile overlay (`pending`) until the hydrator materializes
    // them durably. Reading via raw storage (`with_storage_locked`)
    // is overlay-blind BY DESIGN (the hydrator depends on that), so
    // it 404s a fresh async object on the very node that acked the
    // PUT. Inspection must see what the data path serves — the
    // overlay-aware `CompositionOps::get` (ADR-047 §F-3 / I-CS2
    // bounded-stale visibility contract).
    match store.get(comp_id) {
        Ok(comp) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "node_id": state.node_info.node_id,
                "composition_id": comp_id_str,
                "found": true,
                "namespace_id": comp.namespace_id.0.to_string(),
                "shard_id": comp.shard_id.0.to_string(),
                "size": comp.size,
                "version": comp.version,
                "has_inline_data": comp.has_inline_data,
                "chunk_ids": comp.chunks.iter().map(hex_chunk_id).collect::<Vec<_>>(),
            })),
        ),
        Err(_) => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "node_id": state.node_info.node_id,
                "composition_id": comp_id_str,
                "found": false,
            })),
        ),
    }
}

fn parse_chunk_id_hex(s: &str) -> Option<kiseki_common::ids::ChunkId> {
    if s.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, pair) in s.as_bytes().chunks_exact(2).enumerate() {
        let hi = u8::try_from((pair[0] as char).to_digit(16)?).ok()?;
        let lo = u8::try_from((pair[1] as char).to_digit(16)?).ok()?;
        bytes[i] = (hi << 4) | lo;
    }
    Some(kiseki_common::ids::ChunkId(bytes))
}

fn hex_chunk_id(id: &kiseki_common::ids::ChunkId) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in id.0 {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn chrono_lite() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

async fn dashboard_page() -> Html<&'static str> {
    Html(include_str!("../static/dashboard.html"))
}

fn format_number(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    if bytes >= 1_099_511_627_776 {
        format!("{:.1} TB", bytes as f64 / 1_099_511_627_776.0)
    } else if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

// ---------------------------------------------------------------------------
// Test-only knobs — gated by KISEKI_ENABLE_TEST_KNOBS=1 at request time.
// Used by BDD scenarios that need deterministic fault injection
// (chunk-storage::"Read falls back to fabric…", multi-node-raft::
// "Write requires 2-of-3 quorum (D-5)" and "Composition delta arrives
// before fragment (D-10)"). The runtime guard means a production
// deployment that doesn't set the env var ignores these endpoints
// regardless of how it was built.
// ---------------------------------------------------------------------------

fn test_knobs_enabled() -> bool {
    std::env::var("KISEKI_ENABLE_TEST_KNOBS").as_deref() == Ok("1")
}

fn test_knobs_disabled_response() -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    (
        axum::http::StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": "test knobs disabled — set KISEKI_ENABLE_TEST_KNOBS=1",
        })),
    )
}

async fn admin_test_fabric_slow(
    axum::extract::Path(ms): axum::extract::Path<u64>,
) -> impl IntoResponse {
    if !test_knobs_enabled() {
        return test_knobs_disabled_response();
    }
    kiseki_chunk_cluster::set_fabric_slow_ms(ms);
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({ "fabric_slow_ms": ms })),
    )
}

async fn admin_test_fabric_deny(
    axum::extract::Path(enabled): axum::extract::Path<u8>,
) -> impl IntoResponse {
    if !test_knobs_enabled() {
        return test_knobs_disabled_response();
    }
    let deny = enabled != 0;
    kiseki_chunk_cluster::set_fabric_deny_incoming(deny);
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({ "fabric_deny_incoming": deny })),
    )
}

async fn admin_test_drop_fragment(
    State(state): State<UiState>,
    axum::extract::Path((chunk_id_str, fragment_index)): axum::extract::Path<(String, u32)>,
) -> impl IntoResponse {
    if !test_knobs_enabled() {
        return test_knobs_disabled_response();
    }
    let Some(chunk_id) = parse_chunk_id_hex(&chunk_id_str) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "chunk_id must be 64 hex characters (32-byte HMAC)",
            })),
        );
    };
    let Some(local) = state.local_chunk_store.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "local chunk store not wired into UiState",
            })),
        );
    };
    // Two storage paths: per-fragment (EC, write_fragment) AND
    // whole-envelope (Replication-N, write_chunk for fragment_index=0).
    // The named index targets the per-fragment table; the
    // delete_chunk_force pass also drains the chunks-map entry so
    // a Replication-3 reader actually misses on local read.
    let frag_removed = local
        .delete_fragment(&chunk_id, fragment_index)
        .await
        .unwrap_or(false);
    let chunk_removed = local.delete_chunk_force(&chunk_id).await.unwrap_or(false);
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "chunk_id": chunk_id_str,
            "fragment_index": fragment_index,
            "fragment_removed": frag_removed,
            "chunk_removed": chunk_removed,
        })),
    )
}

/// `POST /admin/test/shard/{shard_id}/advance-watermark/{position}` —
/// drive the shard's replicated `AdvanceWatermark` command (P3a)
/// directly, advancing the `hydrator` consumer to `position`.
///
/// Test-only: lets BDD pin the GH #223 `DeltaLogPruned` refusal
/// contract deterministically. The BDD cluster harness pins
/// `KISEKI_WATERMARK_ADVANCE_INTERVAL_MS` high so the supervisor's
/// own advance round never fires mid-suite — this knob is the only
/// way the GC boundary moves on a harnessed cluster, and it exercises
/// the REAL Raft consensus + state-machine prune path (not a mock).
///
/// Leader-only: `advance_watermark` is a `client_write`, so followers
/// answer 409 CONFLICT and the caller tries the next node.
async fn admin_test_advance_watermark(
    State(state): State<UiState>,
    axum::extract::Path((shard_id_str, position)): axum::extract::Path<(String, u64)>,
) -> impl IntoResponse {
    if !test_knobs_enabled() {
        return test_knobs_disabled_response();
    }
    let Ok(uuid) = uuid::Uuid::parse_str(&shard_id_str) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "shard_id must be a UUID"})),
        );
    };
    let Some(ref log) = state.log_store else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "log store not initialized"})),
        );
    };
    match log
        .advance_watermark(
            kiseki_common::ids::ShardId(uuid),
            kiseki_log::traits::HYDRATOR_CONSUMER,
            kiseki_common::ids::SequenceNumber(position),
        )
        .await
    {
        Ok(()) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "shard_id": shard_id_str,
                "consumer": kiseki_log::traits::HYDRATOR_CONSUMER,
                "position": position,
            })),
        ),
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::type_complexity, clippy::redundant_closure)]
mod cluster_info_rev2_tests {
    //! ADR-008 rev 2 — `build_shards_from_state` tests.
    //!
    //! Validates that the `/cluster/info` `shards` field projects the
    //! control-plane `NamespaceShardMap` onto the wire shape, with
    //! the peer-map providing the `leader_data_addr` resolution.

    use super::*;
    use crate::cluster_control::ControlStateMachine;
    use kiseki_common::ids::{NodeId, OrgId};

    async fn ui_state_with_namespaces(
        peers: Vec<(u64, String)>,
        namespaces: Vec<(String, OrgId, Vec<(uuid::Uuid, u64, [u8; 32], [u8; 32])>)>,
    ) -> UiState {
        let aggregator = Arc::new(MetricsAggregator::new("127.0.0.1:9090".to_owned(), 10));
        let diagnostics = super::super::events::new_shared();
        let state = ControlStateMachine::new();
        // Synthesise the state machine's inner map directly — avoids
        // dragging in a full openraft store for a unit test.
        {
            let mut inner = state.inner.lock().await;
            for (ns_id, tenant_id, shards) in namespaces {
                let snapshots: Vec<crate::cluster_control::state_machine::ShardSnapshot> = shards
                    .into_iter()
                    .map(|(sid, leader_node, rs, re)| {
                        crate::cluster_control::state_machine::ShardSnapshot {
                            shard_id: kiseki_common::ids::ShardId(sid),
                            range_start: rs,
                            range_end: re,
                            leader_node: NodeId(leader_node),
                            is_retiring: false,
                        }
                    })
                    .collect();
                inner.namespaces.insert(
                    ns_id.clone(),
                    crate::cluster_control::NamespaceShardMapSnapshot {
                        namespace_id: ns_id,
                        tenant_id,
                        version: 1,
                        shards: snapshots,
                        fidelity: crate::cluster_control::NamespaceFidelity::default(),
                    },
                );
            }
        }
        UiState {
            aggregator,
            metrics_encode: Arc::new(String::new),
            diagnostics,
            log_store: None,
            node_info: NodeInfo {
                node_id: 1,
                s3_addr: "10.0.0.1:9000".to_owned(),
                nfs_addr: "10.0.0.1:2049".to_owned(),
                metrics_addr: "10.0.0.1:9090".to_owned(),
                raft_peers: peers,
            },
            compositions: None,
            local_chunk_store: None,
            cluster_control: Some(Arc::new(state)),
            cluster_control_store: None,
            audit: None,
            key_manager: None,
            tenants: None,
            namespaces: None,
            drain: None,
        }
    }

    #[tokio::test]
    async fn build_shards_emits_one_entry_per_shard() {
        let s_id = uuid::Uuid::from_u128(1);
        let state = ui_state_with_namespaces(
            vec![
                (1, "10.0.0.1:7000".to_owned()),
                (2, "10.0.0.2:7000".to_owned()),
                (3, "10.0.0.3:7000".to_owned()),
            ],
            vec![(
                "trials".to_owned(),
                OrgId(uuid::Uuid::from_u128(1)),
                vec![(s_id, 2, [0u8; 32], [0xff; 32])],
            )],
        )
        .await;
        let shards = build_shards_from_state(&state).await;
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].shard_id, s_id.to_string());
        assert_eq!(shards[0].namespace_id, "trials");
        assert_eq!(shards[0].leader_id, Some(2));
        // Resolved from peer (2)'s raft_addr → host 10.0.0.2 → 9100
        // (KISEKI_DATA_ADDR default).
        assert_eq!(shards[0].leader_data_addr.as_deref(), Some("10.0.0.2:9100"));
        assert_eq!(
            shards[0].range_start,
            "0x0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            shards[0].range_end,
            "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        );
    }

    #[tokio::test]
    async fn build_shards_omits_leader_when_peer_unknown() {
        // Leader node = 7, but peer list only knows nodes 1, 2, 3.
        let s_id = uuid::Uuid::from_u128(1);
        let state = ui_state_with_namespaces(
            vec![
                (1, "10.0.0.1:7000".to_owned()),
                (2, "10.0.0.2:7000".to_owned()),
                (3, "10.0.0.3:7000".to_owned()),
            ],
            vec![(
                "trials".to_owned(),
                OrgId(uuid::Uuid::from_u128(1)),
                vec![(s_id, 7, [0u8; 32], [0xff; 32])],
            )],
        )
        .await;
        let shards = build_shards_from_state(&state).await;
        assert_eq!(shards.len(), 1);
        // leader_id is still surfaced (the control plane knows it);
        // leader_data_addr is None (we can't resolve the host).
        assert_eq!(shards[0].leader_id, Some(7));
        assert_eq!(shards[0].leader_data_addr, None);
    }

    #[tokio::test]
    async fn build_shards_emits_empty_when_no_control_plane() {
        // Single-node deploys have no cluster_control wired — the
        // empty Vec lets rev-1 clients fall back to seed-only routing.
        let aggregator = Arc::new(MetricsAggregator::new("127.0.0.1:9090".to_owned(), 10));
        let state = UiState {
            aggregator,
            metrics_encode: Arc::new(String::new),
            diagnostics: super::super::events::new_shared(),
            log_store: None,
            node_info: NodeInfo {
                node_id: 1,
                s3_addr: "10.0.0.1:9000".to_owned(),
                nfs_addr: "10.0.0.1:2049".to_owned(),
                metrics_addr: "10.0.0.1:9090".to_owned(),
                raft_peers: vec![],
            },
            compositions: None,
            local_chunk_store: None,
            cluster_control: None,
            cluster_control_store: None,
            audit: None,
            key_manager: None,
            tenants: None,
            namespaces: None,
            drain: None,
        };
        let shards = build_shards_from_state(&state).await;
        assert!(shards.is_empty());
    }

    #[tokio::test]
    async fn build_shards_emits_multiple_shards_per_namespace() {
        // ADR-033 §1 default `initial_shards = max(min(3 * 3, 64), 3)
        // = 9`. Mimic a 3-shard namespace with disjoint ranges.
        let mut shards = Vec::new();
        let split_a = [0x55u8; 32];
        let split_b = [0xaau8; 32];
        shards.push((uuid::Uuid::from_u128(1), 1, [0u8; 32], split_a));
        shards.push((uuid::Uuid::from_u128(2), 2, split_a, split_b));
        shards.push((uuid::Uuid::from_u128(3), 3, split_b, [0xffu8; 32]));
        let state = ui_state_with_namespaces(
            vec![
                (1, "10.0.0.1:7000".to_owned()),
                (2, "10.0.0.2:7000".to_owned()),
                (3, "10.0.0.3:7000".to_owned()),
            ],
            vec![("trials".to_owned(), OrgId(uuid::Uuid::from_u128(1)), shards)],
        )
        .await;
        let projected = build_shards_from_state(&state).await;
        assert_eq!(projected.len(), 3);
        // Sorted by range_start.
        assert_eq!(projected[0].leader_id, Some(1));
        assert_eq!(projected[1].leader_id, Some(2));
        assert_eq!(projected[2].leader_id, Some(3));
    }
}

#[cfg(test)]
#[allow(clippy::redundant_closure)] // `Arc::new(|| String::new())` mirrors sibling rev2 tests
mod ui_router_auth_tests {
    //! End-to-end router-level coverage: verifies that the auth
    //! middleware actually attaches to the right routes once they
    //! are merged into `ui_router`. The standalone middleware
    //! tests in `web::auth::tests` cover the middleware logic in
    //! isolation; this module catches wiring regressions
    //! (e.g. forgetting to apply the layer to `/admin/*` routes
    //! added through `admin_extra::admin_extra_routes`).

    use super::*;
    use crate::web::auth::AuthConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc as StdArc;
    use tower::ServiceExt;

    fn auth_config(admin: Option<&str>, client: Option<&str>) -> AuthConfig {
        AuthConfig {
            admin_token: admin.map(|s| StdArc::new(s.to_owned())),
            client_token: client.map(|s| StdArc::new(s.to_owned())),
            admin_auth_disabled: false,
            cluster_info_public: false,
        }
    }

    fn minimal_ui_state() -> UiState {
        // Minimal-but-valid state. Several admin routes will return
        // 503/SERVICE_UNAVAILABLE because the underlying handle is
        // None, but that's *after* the auth layer — these tests only
        // check the auth-layer outcome, never the handler body.
        UiState {
            aggregator: Arc::new(MetricsAggregator::new("127.0.0.1:9090".to_owned(), 10)),
            metrics_encode: Arc::new(String::new),
            diagnostics: super::super::events::new_shared(),
            log_store: None,
            node_info: NodeInfo {
                node_id: 1,
                s3_addr: "10.0.0.1:9000".to_owned(),
                nfs_addr: "10.0.0.1:2049".to_owned(),
                metrics_addr: "10.0.0.1:9090".to_owned(),
                raft_peers: vec![],
            },
            compositions: None,
            local_chunk_store: None,
            cluster_control: None,
            cluster_control_store: None,
            audit: None,
            key_manager: None,
            tenants: None,
            namespaces: None,
            drain: None,
        }
    }

    fn get_req(uri: &str, bearer: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(tok) = bearer {
            b = b.header("authorization", format!("Bearer {tok}"));
        }
        b.body(Body::empty()).unwrap()
    }

    async fn status_of(router: Router, req: Request<Body>) -> StatusCode {
        router.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn admin_route_blocked_without_bearer() {
        let router = ui_router(minimal_ui_state(), auth_config(Some("admin"), None));
        // /admin/topology/shards comes from admin_extra_routes — the
        // wiring test catches regressions where the merge-then-layer
        // order would have applied the middleware only to the inner
        // builder and bypassed admin_extra.
        assert_eq!(
            status_of(router, get_req("/admin/topology/shards", None)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn admin_route_blocked_with_client_token() {
        let router = ui_router(
            minimal_ui_state(),
            auth_config(Some("admin"), Some("client")),
        );
        assert_eq!(
            status_of(router, get_req("/admin/topology/shards", Some("client"))).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn ui_fragment_route_requires_admin() {
        let router = ui_router(minimal_ui_state(), auth_config(Some("admin"), None));
        // The dashboard fragments expose cluster-wide stats; admin
        // tier per UI/CLI follow-up D1.
        assert_eq!(
            status_of(router, get_req("/ui/fragment/cluster-cards", None)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn cluster_info_blocked_without_bearer() {
        let router = ui_router(
            minimal_ui_state(),
            auth_config(Some("admin"), Some("client")),
        );
        assert_eq!(
            status_of(router, get_req("/cluster/info", None)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn cluster_info_accepts_client_token() {
        let router = ui_router(
            minimal_ui_state(),
            auth_config(Some("admin"), Some("client")),
        );
        // Real handler runs and returns 200; we don't validate body.
        assert_eq!(
            status_of(router, get_req("/cluster/info", Some("client"))).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn cluster_info_accepts_admin_token() {
        let router = ui_router(
            minimal_ui_state(),
            auth_config(Some("admin"), Some("client")),
        );
        assert_eq!(
            status_of(router, get_req("/cluster/info", Some("admin"))).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn cluster_info_public_override_lets_anonymous_through() {
        let mut cfg = auth_config(Some("admin"), Some("client"));
        cfg.cluster_info_public = true;
        let router = ui_router(minimal_ui_state(), cfg);
        assert_eq!(
            status_of(router, get_req("/cluster/info", None)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn admin_disabled_override_lets_anonymous_through_to_admin() {
        let mut cfg = auth_config(Some("admin"), None);
        cfg.admin_auth_disabled = true;
        let router = ui_router(minimal_ui_state(), cfg);
        // /admin/topology/shards returns 200 even without auth.
        assert_eq!(
            status_of(router, get_req("/admin/topology/shards", None)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn shard_leader_route_is_cluster_info_tier() {
        // Per-shard leader probes feed the same client-bootstrap
        // path; gate them with the cluster-info tier (client token
        // suffices).
        let router = ui_router(
            minimal_ui_state(),
            auth_config(Some("admin"), Some("client")),
        );
        let uuid = uuid::Uuid::from_u128(1);
        let path = format!("/cluster/shards/{uuid}/leader");

        // No token → 401.
        assert_eq!(
            status_of(router.clone(), get_req(&path, None)).await,
            StatusCode::UNAUTHORIZED
        );
        // Client token → through the auth layer. Handler returns 503
        // (no log_store wired) which is the *post-auth* response and
        // confirms the layer let the request through.
        assert_eq!(
            status_of(router, get_req(&path, Some("client"))).await,
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod admin_inspect_composition_tests {
    //! P4 (#226/#227) read-coherence on the inspection path
    //! (ADR-047 §F-3 / I-CS2): a composition created at ack on an
    //! async surface lives in the `CompositionStore`'s volatile
    //! overlay until the hydrator materializes it. The
    //! `/admin/composition/{id}` handler must consult the
    //! overlay-aware `CompositionOps::get` — the raw-storage read it
    //! previously used is overlay-blind BY DESIGN (the hydrator
    //! depends on that) and 404'd a fresh async object on the very
    //! node that acked the PUT, which is exactly the Tier-2
    //! chunk-storage regression this pins.

    use super::*;
    use kiseki_common::ids::{ChunkId, NamespaceId, OrgId, ShardId};
    use kiseki_composition::composition::CompositionStore;
    use kiseki_composition::Namespace;

    fn ui_state_with_compositions(store: Arc<CompositionStore>) -> UiState {
        UiState {
            aggregator: Arc::new(MetricsAggregator::new("127.0.0.1:9090".to_owned(), 10)),
            metrics_encode: Arc::new(String::new),
            diagnostics: super::super::events::new_shared(),
            log_store: None,
            node_info: NodeInfo {
                node_id: 1,
                s3_addr: "10.0.0.1:9000".to_owned(),
                nfs_addr: "10.0.0.1:2049".to_owned(),
                metrics_addr: "10.0.0.1:9090".to_owned(),
                raft_peers: vec![],
            },
            compositions: Some(store),
            local_chunk_store: None,
            cluster_control: None,
            cluster_control_store: None,
            audit: None,
            key_manager: None,
            tenants: None,
            namespaces: None,
            drain: None,
        }
    }

    #[tokio::test]
    async fn volatile_overlay_row_is_visible_to_admin_inspection() {
        let store = Arc::new(CompositionStore::default());
        let ns_id = NamespaceId(uuid::Uuid::from_u128(7));
        store.add_namespace(Namespace {
            id: ns_id,
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
            tier_policy: Vec::new(),
            size_band_pools: kiseki_composition::namespace::NamespaceSizeBandPools::default(),
        });
        // Volatile-at-ack row (P4): lands in the overlay, NOT storage.
        let comp_id = store
            .create_volatile(ns_id, vec![ChunkId([7u8; 32])], 1024)
            .expect("volatile create");

        // Pin the regression mechanism: the raw-storage read the
        // handler previously used is overlay-blind — it must NOT see
        // the row (the hydrator's staging path depends on exactly
        // this), which is why the handler must not read through it.
        let raw = store
            .with_storage_locked(|s| s.get(comp_id))
            .expect("raw storage read");
        assert!(
            raw.is_none(),
            "overlay row unexpectedly visible at the raw storage layer — \
             the P4 overlay contract changed; revisit this test AND the \
             admin handler",
        );

        // The admin inspection handler must see it (overlay-aware get).
        let state = ui_state_with_compositions(Arc::clone(&store));
        let resp =
            admin_inspect_composition(State(state), axum::extract::Path(comp_id.0.to_string()))
                .await
                .into_response();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::OK,
            "admin inspection must surface the volatile-at-ack row \
             (ADR-047 §F-3 async-ack visibility)",
        );
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["found"], serde_json::Value::Bool(true));
        assert_eq!(
            json["chunk_ids"].as_array().map(Vec::len),
            Some(1),
            "chunk-id list must come from the overlay row: {json}",
        );
    }

    #[tokio::test]
    async fn unknown_composition_still_reports_not_found() {
        let store = Arc::new(CompositionStore::default());
        let state = ui_state_with_compositions(store);
        let resp = admin_inspect_composition(
            State(state),
            axum::extract::Path(uuid::Uuid::from_u128(99).to_string()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
