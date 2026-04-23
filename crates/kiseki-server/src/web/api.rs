//! REST API endpoints for the admin web UI.
//!
//! All endpoints return JSON or HTML fragments. HTMX polls these for live updates.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;

use super::aggregator::MetricsAggregator;

/// Shared state for API handlers.
#[derive(Clone)]
pub struct UiState {
    /// Metrics aggregator for cluster-wide view.
    pub aggregator: Arc<MetricsAggregator>,
    /// Function to encode local Prometheus metrics.
    pub metrics_encode: Arc<dyn Fn() -> String + Send + Sync>,
}

/// Build the web UI router.
pub fn ui_router(state: UiState) -> Router {
    Router::new()
        .route("/ui", get(dashboard_page))
        .route("/ui/", get(dashboard_page))
        .route("/ui/api/cluster", get(api_cluster_summary))
        .route("/ui/api/nodes", get(api_nodes))
        .route("/ui/fragment/cluster-cards", get(fragment_cluster_cards))
        .route("/ui/fragment/node-table", get(fragment_node_table))
        .route("/ui/fragment/chart-data", get(fragment_chart_data))
        .with_state(state)
}

async fn api_cluster_summary(State(state): State<UiState>) -> impl IntoResponse {
    let metrics_text = (state.metrics_encode)();
    state.aggregator.update_local(metrics_text).await;
    let summary = state.aggregator.cluster_summary().await;
    axum::Json(summary)
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
            "<tr><td>{}</td><td>{badge}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
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
