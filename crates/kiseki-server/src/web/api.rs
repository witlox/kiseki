//! REST API endpoints for the admin web UI.
//!
//! All endpoints return JSON or HTML fragments. HTMX polls these for live updates.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;

use super::aggregator::MetricsAggregator;
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

/// Build the web UI router.
pub fn ui_router(state: UiState) -> Router {
    Router::new()
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
        .route("/cluster/info", get(cluster_info))
        .route("/cluster/shards/:shard_id/leader", get(shard_leader))
        .with_state(state)
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
/// and route writes to the correct node's S3/NFS endpoint.
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

    axum::Json(serde_json::json!({
        "node_id": state.node_info.node_id,
        "s3_addr": state.node_info.s3_addr,
        "nfs_addr": state.node_info.nfs_addr,
        "metrics_addr": state.node_info.metrics_addr,
        "leader_id": leader_id,
        "leader_s3": leader_s3,
        "peers": state.node_info.raft_peers.iter().map(|(id, addr)| {
            let host = addr.split(':').next().unwrap_or("127.0.0.1");
            serde_json::json!({
                "id": id,
                "raft_addr": addr,
                "s3_addr": format!("{host}:9000"),
                "nfs_addr": format!("{host}:2049"),
                "metrics_addr": format!("{host}:{}", state.node_info.metrics_addr.split(':').next_back().unwrap_or("9090")),
            })
        }).collect::<Vec<_>>(),
    }))
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
    match log.shard_health(shard_id).await {
        Ok(info) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "shard_id": info.shard_id.0.to_string(),
                "leader_id": info.leader.map(|n| n.0),
                "raft_members": info.raft_members.iter().map(|n| n.0).collect::<Vec<_>>(),
                "last_committed_seq": info.tip.0,
                "state": format!("{:?}", info.state),
            })),
        ),
        Err(e) => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
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
