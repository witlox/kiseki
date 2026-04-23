//! Server-side event store and metric history.
//!
//! Lightweight ring buffers for metric time-series (3h, 10s interval)
//! and cluster events (last 10,000). Works without external infrastructure.
//! Shared between the web UI and CLI (`kiseki-admin`).

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;

use super::aggregator::NodeSummary;

// ---------------------------------------------------------------------------
// Metric history — time-series ring buffer
// ---------------------------------------------------------------------------

/// A timestamped cluster-wide metric snapshot.
#[derive(Clone, Debug, serde::Serialize)]
pub struct MetricPoint {
    /// Epoch milliseconds.
    pub timestamp_ms: u64,
    /// Cluster-wide aggregate metrics.
    pub summary: NodeSummary,
    /// Number of healthy nodes at this point.
    pub healthy_nodes: usize,
    /// Total nodes at this point.
    pub total_nodes: usize,
}

/// Rolling metric history (ring buffer).
pub struct MetricHistory {
    points: VecDeque<MetricPoint>,
    max_points: usize,
}

impl MetricHistory {
    /// Create a new history with the given capacity.
    ///
    /// At 10s intervals, 3 hours = 1080 points.
    #[must_use]
    pub fn new(max_points: usize) -> Self {
        Self {
            points: VecDeque::with_capacity(max_points),
            max_points,
        }
    }

    /// Record a new data point.
    pub fn push(&mut self, point: MetricPoint) {
        if self.points.len() >= self.max_points {
            self.points.pop_front();
        }
        self.points.push_back(point);
    }

    /// Get all points within the last N hours.
    #[must_use]
    pub fn since_hours(&self, hours: f64) -> Vec<&MetricPoint> {
        let cutoff = now_ms().saturating_sub((hours * 3_600_000.0) as u64);
        self.points
            .iter()
            .filter(|p| p.timestamp_ms >= cutoff)
            .collect()
    }

    /// Get all points.
    #[must_use]
    pub fn all(&self) -> Vec<&MetricPoint> {
        self.points.iter().collect()
    }

    /// Number of stored points.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Cluster events — structured event log
// ---------------------------------------------------------------------------

/// Event severity level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
    Critical,
}

/// Event category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Node,
    Shard,
    Device,
    Tenant,
    Security,
    Admin,
    Gateway,
    Raft,
}

/// A structured cluster event.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ClusterEvent {
    /// Epoch milliseconds.
    pub timestamp_ms: u64,
    /// Severity level.
    pub severity: Severity,
    /// Event category.
    pub category: Category,
    /// Source identifier (node address, shard ID, device name).
    pub source: String,
    /// Human-readable message.
    pub message: String,
    /// Optional structured details (JSON string for drill-down).
    pub details: Option<String>,
}

/// Event store — ring buffer of recent cluster events.
pub struct EventStore {
    events: VecDeque<ClusterEvent>,
    max_events: usize,
}

impl EventStore {
    /// Create a new event store.
    #[must_use]
    pub fn new(max_events: usize) -> Self {
        Self {
            events: VecDeque::with_capacity(max_events),
            max_events,
        }
    }

    /// Record a new event.
    pub fn push(&mut self, event: ClusterEvent) {
        if self.events.len() >= self.max_events {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// Convenience: record an info event.
    pub fn info(&mut self, category: Category, source: &str, message: &str) {
        self.push(ClusterEvent {
            timestamp_ms: now_ms(),
            severity: Severity::Info,
            category,
            source: source.to_owned(),
            message: message.to_owned(),
            details: None,
        });
    }

    /// Convenience: record a warning event.
    pub fn warn(&mut self, category: Category, source: &str, message: &str) {
        self.push(ClusterEvent {
            timestamp_ms: now_ms(),
            severity: Severity::Warning,
            category,
            source: source.to_owned(),
            message: message.to_owned(),
            details: None,
        });
    }

    /// Convenience: record an error event.
    pub fn error(&mut self, category: Category, source: &str, message: &str) {
        self.push(ClusterEvent {
            timestamp_ms: now_ms(),
            severity: Severity::Error,
            category,
            source: source.to_owned(),
            message: message.to_owned(),
            details: None,
        });
    }

    /// Query events by severity and time window.
    #[must_use]
    pub fn query(
        &self,
        severity: Option<Severity>,
        category: Option<Category>,
        hours: f64,
    ) -> Vec<&ClusterEvent> {
        let cutoff = now_ms().saturating_sub((hours * 3_600_000.0) as u64);
        self.events
            .iter()
            .filter(|e| e.timestamp_ms >= cutoff)
            .filter(|e| severity.is_none() || Some(e.severity) == severity)
            .filter(|e| category.is_none() || Some(e.category) == category)
            .collect()
    }

    /// Get the most recent N events.
    #[must_use]
    pub fn recent(&self, n: usize) -> Vec<&ClusterEvent> {
        self.events.iter().rev().take(n).collect()
    }

    /// Total events stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Count by severity.
    #[must_use]
    pub fn count_by_severity(&self, severity: Severity) -> usize {
        self.events
            .iter()
            .filter(|e| e.severity == severity)
            .count()
    }
}

// ---------------------------------------------------------------------------
// Combined diagnostic store (shared between UI + CLI)
// ---------------------------------------------------------------------------

/// Combined diagnostic state — metric history + event store.
///
/// Wrapped in `Arc<RwLock<_>>` for shared async access.
pub struct DiagnosticStore {
    pub metrics: MetricHistory,
    pub events: EventStore,
}

impl DiagnosticStore {
    /// Create with default capacities (3h metrics @ 10s, 10K events).
    #[must_use]
    pub fn new() -> Self {
        Self {
            metrics: MetricHistory::new(1080), // 3h at 10s intervals
            events: EventStore::new(10_000),
        }
    }

    /// Record a metric snapshot and generate events from it.
    pub fn record_snapshot(
        &mut self,
        summary: NodeSummary,
        healthy_nodes: usize,
        total_nodes: usize,
    ) {
        let point = MetricPoint {
            timestamp_ms: now_ms(),
            summary,
            healthy_nodes,
            total_nodes,
        };
        self.metrics.push(point);

        // Auto-generate events from health changes.
        if healthy_nodes < total_nodes {
            let down = total_nodes - healthy_nodes;
            self.events.warn(
                Category::Node,
                "cluster",
                &format!("{down} of {total_nodes} nodes unreachable"),
            );
        }
    }
}

impl Default for DiagnosticStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe handle to the diagnostic store.
pub type SharedDiagnostics = Arc<RwLock<DiagnosticStore>>;

/// Create a new shared diagnostic store.
#[must_use]
pub fn new_shared() -> SharedDiagnostics {
    Arc::new(RwLock::new(DiagnosticStore::new()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_history_ring_buffer() {
        let mut h = MetricHistory::new(3);
        for i in 0..5 {
            h.push(MetricPoint {
                timestamp_ms: i * 1000,
                summary: NodeSummary::default(),
                healthy_nodes: 1,
                total_nodes: 1,
            });
        }
        assert_eq!(h.len(), 3); // oldest 2 evicted
        assert_eq!(h.all()[0].timestamp_ms, 2000);
    }

    #[test]
    fn metric_history_since_hours() {
        let mut h = MetricHistory::new(100);
        let now = now_ms();
        // Point from 2 hours ago.
        h.push(MetricPoint {
            timestamp_ms: now - 2 * 3_600_000,
            summary: NodeSummary::default(),
            healthy_nodes: 1,
            total_nodes: 1,
        });
        // Point from 30 min ago.
        h.push(MetricPoint {
            timestamp_ms: now - 30 * 60_000,
            summary: NodeSummary::default(),
            healthy_nodes: 1,
            total_nodes: 1,
        });
        assert_eq!(h.since_hours(1.0).len(), 1); // only the 30-min one
        assert_eq!(h.since_hours(3.0).len(), 2); // both
    }

    #[test]
    fn event_store_push_and_query() {
        let mut s = EventStore::new(100);
        s.info(Category::Node, "node-1", "node started");
        s.warn(Category::Device, "nvme0", "SMART wear 85%");
        s.error(Category::Shard, "shard-7", "leader election timeout");

        assert_eq!(s.len(), 3);
        assert_eq!(s.query(Some(Severity::Error), None, 1.0).len(), 1);
        assert_eq!(s.query(None, Some(Category::Device), 1.0).len(), 1);
        assert_eq!(s.query(None, None, 1.0).len(), 3);
    }

    #[test]
    fn event_store_ring_buffer() {
        let mut s = EventStore::new(3);
        for i in 0..5 {
            s.info(Category::Node, "n", &format!("event {i}"));
        }
        assert_eq!(s.len(), 3);
        assert_eq!(s.recent(1)[0].message, "event 4");
    }

    #[test]
    fn event_store_count_by_severity() {
        let mut s = EventStore::new(100);
        s.info(Category::Admin, "admin", "backup started");
        s.info(Category::Admin, "admin", "backup completed");
        s.error(Category::Node, "node-3", "unreachable");
        assert_eq!(s.count_by_severity(Severity::Info), 2);
        assert_eq!(s.count_by_severity(Severity::Error), 1);
        assert_eq!(s.count_by_severity(Severity::Critical), 0);
    }

    #[test]
    fn diagnostic_store_generates_events_on_unhealthy() {
        let mut ds = DiagnosticStore::new();
        ds.record_snapshot(NodeSummary::default(), 2, 3); // 1 node down
        assert_eq!(ds.events.len(), 1);
        assert_eq!(ds.events.recent(1)[0].severity, Severity::Warning);
    }

    #[test]
    fn event_recent_returns_newest_first() {
        let mut s = EventStore::new(100);
        s.info(Category::Node, "n", "first");
        s.info(Category::Node, "n", "second");
        s.info(Category::Node, "n", "third");
        let r = s.recent(2);
        assert_eq!(r[0].message, "third");
        assert_eq!(r[1].message, "second");
    }
}
