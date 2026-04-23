//! REST API endpoints for the admin web UI.
//!
//! All endpoints return JSON or HTML fragments. HTMX polls these for live updates.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
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
