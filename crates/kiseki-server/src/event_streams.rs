//! Event broker channels for the streaming admin RPCs (ADR-025
//! W7 — `DeviceHealth` and `IOStats`).
//!
//! Both RPCs are server-streaming: the admin client subscribes
//! and receives events until they hang up. The producers live
//! in the data-path subsystems (chunk, chunk-cluster) — this
//! module provides the channel plumbing so the admin handler
//! can consume without coupling to producer internals.
//!
//! ## Channel model
//!
//! `tokio::sync::broadcast::channel(1024)` for each event source.
//! Each subscriber gets its own `Receiver`; on lag (subscriber
//! falls more than 1024 events behind) the receiver is signalled
//! `RecvError::Lagged(count)` and the RPC handler closes the
//! stream with `Status::resource_exhausted` so the operator
//! reconnects.
//!
//! ## Why broadcast and not mpsc
//!
//! Multiple admin clients may want the same stream
//! simultaneously (one operator on `kiseki-storage device-health`
//! plus a Grafana exporter polling). `broadcast` fans the same
//! event out to every subscriber; `mpsc` would force one
//! consumer per channel.
//!
//! ## Producer side (W7 minimum, expand incrementally)
//!
//! The W7 channels are constructed and wired into the admin
//! handler today, but the *producers* on the data path land in
//! follow-on PRs alongside the relevant subsystem changes:
//!
//! - `DeviceHealthBroker` — chunk subsystem on device-state
//!   transitions (online → degraded, drive replacement, etc).
//!   Today the chunk store doesn't emit transitions; the broker
//!   sits idle until the producer lands.
//! - `IoStatsBroker` — chunk-cluster on a periodic sample
//!   timer (1-second cadence by default; configurable via
//!   `IOStatsRequest.sample_interval_ms`). Today the broker
//!   sits idle.
//!
//! Tests exercise the channels directly to prove the wiring
//! without depending on the producers.

use std::sync::Arc;

use kiseki_proto::v1 as pb;
use tokio::sync::broadcast;

/// Default broadcast channel capacity. 1024 events ≈ a few
/// minutes of headroom at sane sample rates.
pub const DEFAULT_CAPACITY: usize = 1024;

/// Shared broker for `DeviceHealth` server-streaming RPC.
/// Cheap to clone via `Arc` — one instance lives in
/// `KisekiMetrics`-adjacent state and is handed to both the
/// admin handler (consumer) and the chunk-store device-state
/// observer (producer).
#[derive(Clone, Debug)]
pub struct DeviceHealthBroker {
    tx: Arc<broadcast::Sender<pb::DeviceHealthEvent>>,
}

impl DeviceHealthBroker {
    /// Construct with the default capacity (1024).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Construct with a custom capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx: Arc::new(tx) }
    }

    /// Publish an event. Returns the number of receivers that got
    /// the event (0 if no admin client is currently streaming —
    /// the event is dropped, not buffered, which is what we want
    /// for an observer channel).
    ///
    /// Wired by the chunk subsystem's device-state observer; the
    /// admin handler today only consumes via [`Self::subscribe`].
    #[allow(dead_code)] // wired alongside the device-state observer follow-on
    pub fn publish(&self, event: pb::DeviceHealthEvent) -> usize {
        // `send` returns Err only when there are zero subscribers —
        // we treat that as "no observers, no harm".
        self.tx.send(event).unwrap_or(0)
    }

    /// Subscribe a fresh receiver. Each admin RPC call subscribes
    /// once at the top of the handler.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<pb::DeviceHealthEvent> {
        self.tx.subscribe()
    }

    /// Number of currently-active subscribers. Test/inspection
    /// helper; not used by production code today.
    #[allow(dead_code)]
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for DeviceHealthBroker {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared broker for `IOStats` server-streaming RPC. Wired
/// alongside the chunk-cluster periodic sampler.
#[derive(Clone, Debug)]
pub struct IoStatsBroker {
    tx: Arc<broadcast::Sender<pb::IoStatsEvent>>,
}

impl IoStatsBroker {
    /// Construct with the default capacity (1024).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Construct with a custom capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx: Arc::new(tx) }
    }

    /// Publish a sample. See [`DeviceHealthBroker::publish`] for
    /// the no-subscribers semantics.
    #[allow(dead_code)] // wired alongside the chunk-cluster sampler follow-on
    pub fn publish(&self, event: pb::IoStatsEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Subscribe a fresh receiver.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<pb::IoStatsEvent> {
        self.tx.subscribe()
    }

    /// Number of currently-active subscribers. Test/inspection
    /// helper.
    #[allow(dead_code)]
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for IoStatsBroker {
    fn default() -> Self {
        Self::new()
    }
}

/// Both brokers bundled — passed as a single dep into
/// `StorageAdminGrpc::with_event_streams`.
#[derive(Clone, Debug)]
pub struct EventStreams {
    /// Producer/consumer channel for the `DeviceHealth` RPC.
    pub device_health: DeviceHealthBroker,
    /// Producer/consumer channel for the `IOStats` RPC.
    pub io_stats: IoStatsBroker,
}

impl EventStreams {
    /// Construct fresh brokers with the default capacities.
    #[must_use]
    pub fn new() -> Self {
        Self {
            device_health: DeviceHealthBroker::new(),
            io_stats: IoStatsBroker::new(),
        }
    }
}

impl Default for EventStreams {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dh_event(id: &str) -> pb::DeviceHealthEvent {
        pb::DeviceHealthEvent {
            device_id: id.to_owned(),
            event: "io_error".into(),
            detail: "transient".into(),
            at: "1970-01-01T00:00:00Z".into(),
        }
    }

    fn iostats_event(pool: &str) -> pb::IoStatsEvent {
        pb::IoStatsEvent {
            sampled_at: format!("ms:0 pool={pool}"),
            reads_per_sec: 1,
            writes_per_sec: 2,
            read_bytes_per_sec: 1024,
            write_bytes_per_sec: 2048,
            pool_stats: vec![],
        }
    }

    #[tokio::test]
    async fn device_health_subscriber_receives_published_event() {
        let b = DeviceHealthBroker::new();
        let mut rx = b.subscribe();
        let delivered = b.publish(dh_event("dev-1"));
        assert_eq!(delivered, 1, "single subscriber must receive");
        let ev = rx.recv().await.expect("recv");
        assert_eq!(ev.device_id, "dev-1");
    }

    #[tokio::test]
    async fn device_health_publish_with_no_subscribers_is_silent() {
        let b = DeviceHealthBroker::new();
        // No subscribers. publish() returns 0 and does not panic.
        let delivered = b.publish(dh_event("orphan"));
        assert_eq!(delivered, 0);
    }

    #[tokio::test]
    async fn device_health_multiple_subscribers_each_get_event() {
        let b = DeviceHealthBroker::new();
        let mut a = b.subscribe();
        let mut c = b.subscribe();
        b.publish(dh_event("dev-2"));
        assert_eq!(a.recv().await.unwrap().device_id, "dev-2");
        assert_eq!(c.recv().await.unwrap().device_id, "dev-2");
    }

    #[tokio::test]
    async fn device_health_subscriber_count_reflects_active() {
        let b = DeviceHealthBroker::new();
        assert_eq!(b.receiver_count(), 0);
        let r1 = b.subscribe();
        assert_eq!(b.receiver_count(), 1);
        let r2 = b.subscribe();
        assert_eq!(b.receiver_count(), 2);
        drop(r1);
        assert_eq!(b.receiver_count(), 1);
        drop(r2);
        assert_eq!(b.receiver_count(), 0);
    }

    #[tokio::test]
    async fn device_health_lagged_subscriber_reports_recv_error() {
        let b = DeviceHealthBroker::with_capacity(2);
        let mut rx = b.subscribe();
        // Publish 5 events without consuming → lag.
        for i in 0..5 {
            b.publish(dh_event(&format!("dev-{i}")));
        }
        // First recv reports Lagged(_); buffer was 2, we sent 5,
        // so 3 events were dropped from the receiver's view.
        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => {
                assert!(n >= 1, "lag count should be positive");
            }
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn io_stats_subscriber_receives_published_event() {
        let b = IoStatsBroker::new();
        let mut rx = b.subscribe();
        let delivered = b.publish(iostats_event("hot"));
        assert_eq!(delivered, 1);
        let ev = rx.recv().await.expect("recv");
        assert!(ev.sampled_at.contains("pool=hot"));
        assert_eq!(ev.reads_per_sec, 1);
    }

    #[tokio::test]
    async fn io_stats_publish_with_no_subscribers_is_silent() {
        let b = IoStatsBroker::new();
        assert_eq!(b.publish(iostats_event("cold")), 0);
    }

    #[tokio::test]
    async fn event_streams_bundle_has_independent_channels() {
        let s = EventStreams::new();
        let mut dh_rx = s.device_health.subscribe();
        let mut io_rx = s.io_stats.subscribe();
        s.device_health.publish(dh_event("d"));
        s.io_stats.publish(iostats_event("p"));
        assert_eq!(dh_rx.recv().await.unwrap().device_id, "d");
        assert!(io_rx.recv().await.unwrap().sampled_at.contains("pool=p"));
    }
}
