//! Prometheus metrics registry and HTTP endpoint.
//!
//! Exposes `/metrics` (Prometheus text format) and `/health` (200 OK)
//! on a dedicated HTTP port (default 9090).

use std::net::SocketAddr;

use axum::routing::get;
use axum::Router;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};

/// Application-wide metrics registry.
///
/// Created once at server boot and shared via `Arc` across all
/// subsystems. Each subsystem records into its own metrics.
#[derive(Clone)]
#[allow(dead_code)] // Fields used incrementally as subsystems wire in metrics
pub struct KisekiMetrics {
    registry: Registry,

    // --- Raft ---
    /// Raft commit latency in seconds.
    pub raft_commit_latency: HistogramVec,
    /// Total Raft entries applied.
    pub raft_entries_total: IntCounter,

    // --- Chunk ---
    /// Chunk bytes written.
    pub chunk_write_bytes: IntCounter,
    /// Chunk bytes read.
    pub chunk_read_bytes: IntCounter,
    /// EC encode latency in seconds.
    pub chunk_ec_encode_latency: HistogramVec,

    // --- Gateway ---
    /// S3/NFS request count by method and status.
    pub gateway_requests_total: IntCounterVec,
    /// Gateway request duration in seconds.
    pub gateway_request_duration: HistogramVec,

    // --- Pool ---
    /// Pool capacity bytes (total).
    pub pool_capacity_total: IntGaugeVec,
    /// Pool capacity bytes (used).
    pub pool_capacity_used: IntGaugeVec,

    // --- Transport ---
    /// Active transport connections.
    pub transport_connections_active: IntGauge,
    /// Idle transport connections.
    pub transport_connections_idle: IntGauge,

    // --- Shard ---
    /// Delta count per shard.
    pub shard_delta_count: IntGaugeVec,

    // --- Key management ---
    /// Key rotation count.
    pub key_rotation_total: IntCounter,
    /// Crypto-shred count.
    pub crypto_shred_total: IntCounter,

    // --- Cluster fabric (Phase 16a) ---
    /// Cross-node chunk fabric metrics. Wired into
    /// `ClusteredChunkStore` and `GrpcFabricPeer` via
    /// `with_metrics(...)` at runtime construction.
    pub fabric: std::sync::Arc<kiseki_chunk_cluster::FabricMetrics>,

    // --- Gateway retry budget (ADR-040 §D7 + §D10 — F-4 closure) ---
    /// Read-path retry counters. Wired into `InMemoryGateway` via
    /// `with_retry_metrics(...)` at runtime construction.
    pub gateway_retry: std::sync::Arc<kiseki_gateway::metrics::GatewayRetryMetrics>,
}

impl KisekiMetrics {
    /// Create and register all metrics.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new() -> Self {
        let registry = Registry::new();

        let raft_commit_latency = HistogramVec::new(
            HistogramOpts::new("kiseki_raft_commit_latency_seconds", "Raft commit latency")
                .buckets(vec![
                    0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0,
                ]),
            &["shard"],
        )
        .expect("metric");
        registry
            .register(Box::new(raft_commit_latency.clone()))
            .expect("register");

        let raft_entries_total =
            IntCounter::new("kiseki_raft_entries_total", "Total Raft entries applied")
                .expect("metric");
        registry
            .register(Box::new(raft_entries_total.clone()))
            .expect("register");

        let chunk_write_bytes =
            IntCounter::new("kiseki_chunk_write_bytes_total", "Chunk bytes written")
                .expect("metric");
        registry
            .register(Box::new(chunk_write_bytes.clone()))
            .expect("register");

        let chunk_read_bytes =
            IntCounter::new("kiseki_chunk_read_bytes_total", "Chunk bytes read").expect("metric");
        registry
            .register(Box::new(chunk_read_bytes.clone()))
            .expect("register");

        let chunk_ec_encode_latency = HistogramVec::new(
            HistogramOpts::new("kiseki_chunk_ec_encode_seconds", "EC encode latency")
                .buckets(vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05]),
            &["strategy"],
        )
        .expect("metric");
        registry
            .register(Box::new(chunk_ec_encode_latency.clone()))
            .expect("register");

        let gateway_requests_total = IntCounterVec::new(
            Opts::new("kiseki_gateway_requests_total", "Gateway request count"),
            &["method", "status"],
        )
        .expect("metric");
        registry
            .register(Box::new(gateway_requests_total.clone()))
            .expect("register");

        let gateway_request_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_gateway_request_duration_seconds",
                "Gateway request duration",
            )
            .buckets(vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0]),
            &["method"],
        )
        .expect("metric");
        registry
            .register(Box::new(gateway_request_duration.clone()))
            .expect("register");

        let pool_capacity_total = IntGaugeVec::new(
            Opts::new("kiseki_pool_capacity_total_bytes", "Pool total capacity"),
            &["pool"],
        )
        .expect("metric");
        registry
            .register(Box::new(pool_capacity_total.clone()))
            .expect("register");

        let pool_capacity_used = IntGaugeVec::new(
            Opts::new("kiseki_pool_capacity_used_bytes", "Pool used capacity"),
            &["pool"],
        )
        .expect("metric");
        registry
            .register(Box::new(pool_capacity_used.clone()))
            .expect("register");

        let transport_connections_active = IntGauge::new(
            "kiseki_transport_connections_active",
            "Active transport connections",
        )
        .expect("metric");
        registry
            .register(Box::new(transport_connections_active.clone()))
            .expect("register");

        let transport_connections_idle = IntGauge::new(
            "kiseki_transport_connections_idle",
            "Idle transport connections",
        )
        .expect("metric");
        registry
            .register(Box::new(transport_connections_idle.clone()))
            .expect("register");

        let shard_delta_count = IntGaugeVec::new(
            Opts::new("kiseki_shard_delta_count", "Delta count per shard"),
            &["shard"],
        )
        .expect("metric");
        registry
            .register(Box::new(shard_delta_count.clone()))
            .expect("register");

        let key_rotation_total =
            IntCounter::new("kiseki_key_rotation_total", "Key rotations performed")
                .expect("metric");
        registry
            .register(Box::new(key_rotation_total.clone()))
            .expect("register");

        let crypto_shred_total = IntCounter::new(
            "kiseki_crypto_shred_total",
            "Crypto-shred operations performed",
        )
        .expect("metric");
        registry
            .register(Box::new(crypto_shred_total.clone()))
            .expect("register");

        let fabric = std::sync::Arc::new(
            kiseki_chunk_cluster::FabricMetrics::register(&registry)
                .expect("fabric metrics register"),
        );

        let gateway_retry = std::sync::Arc::new(
            kiseki_gateway::metrics::GatewayRetryMetrics::register(&registry)
                .expect("gateway retry metrics register"),
        );

        Self {
            registry,
            raft_commit_latency,
            raft_entries_total,
            chunk_write_bytes,
            chunk_read_bytes,
            chunk_ec_encode_latency,
            gateway_requests_total,
            gateway_request_duration,
            pool_capacity_total,
            pool_capacity_used,
            transport_connections_active,
            transport_connections_idle,
            shard_delta_count,
            key_rotation_total,
            crypto_shred_total,
            fabric,
            gateway_retry,
        }
    }

    /// Encode all metrics as Prometheus text format.
    #[must_use]
    pub fn encode(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap_or(());
        String::from_utf8(buffer).unwrap_or_default()
    }
}

impl Default for KisekiMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Start the metrics + admin UI HTTP server on the given address.
///
/// Serves:
/// - `GET /metrics` — Prometheus text exposition format
/// - `GET /health` — `200 OK` (load balancer probe)
/// - `GET /cluster/info` — JSON cluster info with leader discovery
/// - `GET /ui` — Admin dashboard (HTMX + Chart.js)
/// - `GET /ui/api/*` — JSON API endpoints
/// - `GET /ui/fragment/*` — HTMX HTML partial endpoints
/// - `GET /ui/logo` — Logo image
pub async fn run_metrics_server(
    addr: SocketAddr,
    metrics: KisekiMetrics,
    peer_addrs: Vec<String>,
    log_store: Option<std::sync::Arc<dyn kiseki_log::LogOps + Send + Sync>>,
    node_info: crate::web::api::NodeInfo,
    compositions: Option<
        std::sync::Arc<tokio::sync::Mutex<kiseki_composition::composition::CompositionStore>>,
    >,
) -> std::io::Result<()> {
    use crate::web;

    // Set up the metrics aggregator for cluster-wide view.
    let metrics_addr = addr.to_string();
    let aggregator = std::sync::Arc::new(web::aggregator::MetricsAggregator::new(metrics_addr, 10));

    // Diagnostic store: metric history (3h) + event log (10K events).
    let diagnostics = web::events::new_shared();

    // Clone metrics for the encode closure.
    let metrics_for_ui = metrics.clone();
    let ui_state = web::api::UiState {
        aggregator: std::sync::Arc::clone(&aggregator),
        metrics_encode: std::sync::Arc::new(move || metrics_for_ui.encode()),
        diagnostics: std::sync::Arc::clone(&diagnostics),
        log_store,
        node_info,
        compositions,
    };

    // Build combined router: metrics + health + admin UI.
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(health_handler))
        .route("/ui/logo", get(logo_handler))
        .with_state(metrics)
        .merge(web::api::ui_router(ui_state));

    tracing::info!(addr = %addr, "metrics + admin UI server listening");

    // Spawn background peer scraper + diagnostic recorder.
    let scrape_agg = std::sync::Arc::clone(&aggregator);
    let scrape_diag = std::sync::Arc::clone(&diagnostics);
    let scrape_peers = peer_addrs;
    tokio::spawn(async move {
        let interval = scrape_agg.interval();
        loop {
            for peer in &scrape_peers {
                scrape_agg.scrape_peer(peer).await;
            }
            // Record cluster snapshot into diagnostic history.
            let summary = scrape_agg.cluster_summary().await;
            {
                let mut diag = scrape_diag.write().await;
                diag.record_snapshot(
                    summary.aggregate,
                    summary.healthy_nodes,
                    summary.total_nodes,
                );
            }
            tokio::time::sleep(interval).await;
        }
    });

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .await
        .map_err(std::io::Error::other)
}

async fn logo_handler() -> impl axum::response::IntoResponse {
    // Serve the embedded logo image.
    let logo_bytes: &[u8] = include_bytes!("static/logo.png");
    (
        axum::http::StatusCode::OK,
        [("content-type", "image/png")],
        logo_bytes,
    )
}

async fn metrics_handler(
    axum::extract::State(metrics): axum::extract::State<KisekiMetrics>,
) -> String {
    metrics.encode()
}

async fn health_handler() -> &'static str {
    "OK"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_encode_after_observation() {
        let m = KisekiMetrics::new();
        // Observe some values so they appear in the output.
        m.raft_commit_latency
            .with_label_values(&["test"])
            .observe(0.001);
        m.chunk_write_bytes.inc_by(100);
        m.gateway_requests_total
            .with_label_values(&["GET", "200"])
            .inc();

        let output = m.encode();
        assert!(
            output.contains("kiseki_raft_commit_latency_seconds"),
            "histogram should appear after observation"
        );
        assert!(
            output.contains("kiseki_chunk_write_bytes_total"),
            "counter should appear after increment"
        );
        assert!(
            output.contains("kiseki_gateway_requests_total"),
            "counter vec should appear after increment"
        );
    }

    #[test]
    fn counter_increments() {
        let m = KisekiMetrics::new();
        m.raft_entries_total.inc();
        m.raft_entries_total.inc();
        assert_eq!(m.raft_entries_total.get(), 2);
    }

    #[test]
    fn histogram_observes() {
        let m = KisekiMetrics::new();
        m.raft_commit_latency
            .with_label_values(&["shard-1"])
            .observe(0.005);
        let output = m.encode();
        assert!(output.contains("shard-1"));
    }

    #[test]
    fn gateway_request_counter() {
        let m = KisekiMetrics::new();
        m.gateway_requests_total
            .with_label_values(&["PUT", "200"])
            .inc();
        m.gateway_requests_total
            .with_label_values(&["GET", "404"])
            .inc();
        let output = m.encode();
        assert!(output.contains("PUT"));
        assert!(output.contains("GET"));
    }

    #[test]
    fn gauge_set_and_read() {
        let m = KisekiMetrics::new();
        m.transport_connections_active.set(42);
        assert_eq!(m.transport_connections_active.get(), 42);
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let resp = health_handler().await;
        assert_eq!(resp, "OK");
    }
}
