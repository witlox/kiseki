//! Metrics aggregator — scrapes peer nodes and caches results.
//!
//! Each node periodically fetches `/metrics` and `/health` from all
//! known peers. The UI serves the merged cluster-wide view from
//! local + cached peer data.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

/// Cached metrics snapshot from a single node.
#[derive(Clone, Debug, serde::Serialize)]
pub struct NodeSnapshot {
    /// Node identifier.
    pub node_id: String,
    /// Node address (for display).
    pub address: String,
    /// Whether the node is healthy.
    pub healthy: bool,
    /// Raw Prometheus metrics text.
    pub metrics_text: String,
    /// When this snapshot was taken.
    #[serde(skip)]
    pub fetched_at: Instant,
    /// Parsed key metrics for the dashboard.
    pub summary: NodeSummary,
}

/// Parsed summary metrics for quick dashboard display.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct NodeSummary {
    /// Raft entries applied.
    pub raft_entries: u64,
    /// Chunk bytes written.
    pub chunk_write_bytes: u64,
    /// Chunk bytes read.
    pub chunk_read_bytes: u64,
    /// Gateway requests total.
    pub gateway_requests: u64,
    /// Active transport connections.
    pub transport_connections: i64,
    /// Shard delta count (aggregate).
    pub shard_deltas: u64,
}

/// Cluster-wide metrics aggregator.
pub struct MetricsAggregator {
    /// Cached snapshots keyed by node address.
    snapshots: Arc<RwLock<HashMap<String, NodeSnapshot>>>,
    /// Scrape interval.
    interval: Duration,
    /// Stale threshold — snapshots older than this are marked unhealthy.
    stale_threshold: Duration,
    /// This node's address.
    local_addr: String,
}

impl MetricsAggregator {
    /// Create a new aggregator.
    #[must_use]
    pub fn new(local_addr: String, interval_secs: u64) -> Self {
        Self {
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            interval: Duration::from_secs(interval_secs),
            stale_threshold: Duration::from_secs(interval_secs * 3 / 2),
            local_addr,
        }
    }

    /// Get a handle to the shared snapshot cache (for API handlers).
    #[must_use]
    pub fn snapshots(&self) -> Arc<RwLock<HashMap<String, NodeSnapshot>>> {
        Arc::clone(&self.snapshots)
    }

    /// Update the local node's snapshot from in-process metrics.
    pub async fn update_local(&self, metrics_text: String) {
        let summary = parse_summary(&metrics_text);
        let snapshot = NodeSnapshot {
            node_id: self.local_addr.clone(),
            address: self.local_addr.clone(),
            healthy: true,
            metrics_text,
            fetched_at: Instant::now(),
            summary,
        };
        let mut cache = self.snapshots.write().await;
        cache.insert(self.local_addr.clone(), snapshot);
    }

    /// Scrape a single peer and cache the result.
    pub async fn scrape_peer(&self, peer_addr: &str) {
        let metrics_url = format!("http://{peer_addr}/metrics");
        let health_url = format!("http://{peer_addr}/health");

        let healthy = reqwest_get_ok(&health_url).await;
        let metrics_text = reqwest_get_body(&metrics_url).await.unwrap_or_default();
        let summary = parse_summary(&metrics_text);

        let snapshot = NodeSnapshot {
            node_id: peer_addr.to_owned(),
            address: peer_addr.to_owned(),
            healthy,
            metrics_text,
            fetched_at: Instant::now(),
            summary,
        };

        let mut cache = self.snapshots.write().await;
        cache.insert(peer_addr.to_owned(), snapshot);
    }

    /// Get all cached snapshots (for the API).
    pub async fn all_snapshots(&self) -> Vec<NodeSnapshot> {
        let cache = self.snapshots.read().await;
        let now = Instant::now();
        cache
            .values()
            .map(|s| {
                let mut s = s.clone();
                if now.duration_since(s.fetched_at) > self.stale_threshold {
                    s.healthy = false;
                }
                s
            })
            .collect()
    }

    /// Get the cluster-wide summary (aggregated across all nodes).
    pub async fn cluster_summary(&self) -> ClusterSummary {
        let snapshots = self.all_snapshots().await;
        let total_nodes = snapshots.len();
        let healthy_nodes = snapshots.iter().filter(|s| s.healthy).count();

        let mut total = NodeSummary::default();
        for s in &snapshots {
            total.raft_entries += s.summary.raft_entries;
            total.chunk_write_bytes += s.summary.chunk_write_bytes;
            total.chunk_read_bytes += s.summary.chunk_read_bytes;
            total.gateway_requests += s.summary.gateway_requests;
            total.transport_connections += s.summary.transport_connections;
            total.shard_deltas += s.summary.shard_deltas;
        }

        ClusterSummary {
            total_nodes,
            healthy_nodes,
            aggregate: total,
        }
    }

    /// Scrape interval for the background task.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }
}

/// Cluster-wide aggregated summary.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ClusterSummary {
    pub total_nodes: usize,
    pub healthy_nodes: usize,
    pub aggregate: NodeSummary,
}

/// Parse key metrics from Prometheus text format.
fn parse_summary(text: &str) -> NodeSummary {
    let mut summary = NodeSummary::default();
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((key, val)) = line.split_once(' ') {
            let v: f64 = val.trim().parse().unwrap_or(0.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            match key {
                "kiseki_raft_entries_total" => summary.raft_entries = v as u64,
                "kiseki_chunk_write_bytes_total" => summary.chunk_write_bytes = v as u64,
                "kiseki_chunk_read_bytes_total" => summary.chunk_read_bytes = v as u64,
                "kiseki_transport_connections_active" => summary.transport_connections = v as i64,
                _ => {
                    if key.starts_with("kiseki_gateway_requests_total") {
                        summary.gateway_requests += v as u64;
                    } else if key.starts_with("kiseki_shard_delta_count") {
                        summary.shard_deltas += v as u64;
                    }
                }
            }
        }
    }
    summary
}

/// Simple HTTP GET that returns true if response contains "OK" or HTTP 200.
async fn reqwest_get_ok(url: &str) -> bool {
    reqwest_get_body(url)
        .await
        .is_some_and(|body| body.contains("OK") || !body.is_empty())
}

/// Simple HTTP GET that returns the body as a string.
#[allow(clippy::items_after_statements)]
async fn reqwest_get_body(url: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let stream = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(extract_host_port(url)?),
    )
    .await
    .ok()?
    .ok()?;

    let path = extract_path(url);
    let host = extract_host_port(url)?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");

    let mut stream = stream;
    stream.write_all(req.as_bytes()).await.ok()?;
    stream.flush().await.ok()?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.ok()?;

    let text = String::from_utf8_lossy(&buf);
    // Skip HTTP headers.
    let body_start = text.find("\r\n\r\n").map(|i| i + 4)?;
    Some(text[body_start..].to_string())
}

fn extract_host_port(url: &str) -> Option<String> {
    url.strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .map(String::from)
}

fn extract_path(url: &str) -> String {
    url.strip_prefix("http://")
        .and_then(|rest| rest.find('/').map(|i| &rest[i..]))
        .unwrap_or("/")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_summary_extracts_metrics() {
        let text = r#"
# HELP kiseki_raft_entries_total Total entries
# TYPE kiseki_raft_entries_total counter
kiseki_raft_entries_total 42
kiseki_chunk_write_bytes_total 1048576
kiseki_chunk_read_bytes_total 524288
kiseki_transport_connections_active 5
kiseki_gateway_requests_total{method="PUT",status="200"} 10
kiseki_gateway_requests_total{method="GET",status="200"} 20
kiseki_shard_delta_count{shard="shard-1"} 100
kiseki_shard_delta_count{shard="shard-2"} 200
"#;
        let s = parse_summary(text);
        assert_eq!(s.raft_entries, 42);
        assert_eq!(s.chunk_write_bytes, 1_048_576);
        assert_eq!(s.chunk_read_bytes, 524_288);
        assert_eq!(s.transport_connections, 5);
        assert_eq!(s.gateway_requests, 30);
        assert_eq!(s.shard_deltas, 300);
    }

    #[test]
    fn extract_host_port_works() {
        assert_eq!(
            extract_host_port("http://10.0.0.1:9090/metrics"),
            Some("10.0.0.1:9090".into())
        );
    }

    #[test]
    fn extract_path_works() {
        assert_eq!(extract_path("http://10.0.0.1:9090/metrics"), "/metrics");
        assert_eq!(extract_path("http://10.0.0.1:9090"), "/");
    }

    #[tokio::test]
    async fn aggregator_local_update() {
        let agg = MetricsAggregator::new("127.0.0.1:9090".into(), 10);
        agg.update_local("kiseki_raft_entries_total 99\n".into())
            .await;
        let snaps = agg.all_snapshots().await;
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].summary.raft_entries, 99);
        assert!(snaps[0].healthy);
    }

    #[tokio::test]
    async fn cluster_summary_aggregates() {
        let agg = MetricsAggregator::new("node-1".into(), 10);
        agg.update_local("kiseki_raft_entries_total 10\n".into())
            .await;
        // Manually insert a "peer" snapshot.
        {
            let snaps = agg.snapshots();
            let mut cache = snaps.write().await;
            cache.insert(
                "node-2".into(),
                NodeSnapshot {
                    node_id: "node-2".into(),
                    address: "node-2".into(),
                    healthy: true,
                    metrics_text: "kiseki_raft_entries_total 20\n".into(),
                    fetched_at: Instant::now(),
                    summary: parse_summary("kiseki_raft_entries_total 20\n"),
                },
            );
        }
        let summary = agg.cluster_summary().await;
        assert_eq!(summary.total_nodes, 2);
        assert_eq!(summary.healthy_nodes, 2);
        assert_eq!(summary.aggregate.raft_entries, 30);
    }
}
