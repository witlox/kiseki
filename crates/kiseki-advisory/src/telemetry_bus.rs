//! Per-workload telemetry subscription bus.
//!
//! Implements `subscribe_backpressure` and `subscribe_qos_headroom` for the
//! advisory subsystem. Subscribers receive only events scoped to their own
//! workload (I-WA5: per-caller scoping).
//!
//! Bounded mpsc channels prevent slow subscribers from blocking the
//! advisory runtime; on overflow, the oldest unread event is dropped.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::telemetry::BackpressureSeverity;

/// Bucketed `QoS` headroom — k-anonymous (I-WA5/I-WA6) representation of
/// remaining capacity within the workload's I-T2 quota.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum QosHeadroomBucket {
    /// Plenty of budget remaining.
    Ample,
    /// Half budget remaining.
    Moderate,
    /// Approaching budget cap.
    Tight,
    /// Budget exhausted — backpressure imminent.
    Exhausted,
}

/// A backpressure event delivered to a per-workload subscription.
#[derive(Clone, Debug)]
pub struct BackpressureEvent {
    /// Severity of the backpressure signal.
    pub severity: BackpressureSeverity,
    /// Suggested retry-after window. Bucketed (I-WA5) — never the raw queue depth.
    pub retry_after_ms: u64,
}

/// Workload identifier — opaque string from `DeclareWorkflow`.
pub type WorkloadId = String;

/// Bounded queue depth per subscriber. Sized to keep memory bounded
/// while letting the subscriber catch up across a small burst.
const SUBSCRIBER_CAPACITY: usize = 64;

/// Bucket the soft-backpressure retry-after window — only a fixed set of
/// values is ever exposed to subscribers (I-WA5).
#[must_use]
pub fn bucket_retry_after_ms(raw_ms: u64) -> u64 {
    const BUCKETS: [u64; 4] = [50, 100, 250, 500];
    BUCKETS
        .iter()
        .copied()
        .find(|b| *b >= raw_ms)
        .unwrap_or(*BUCKETS.last().expect("non-empty"))
}

/// In-process subscription bus. Owned by the advisory runtime and shared
/// (via `Arc`) with everything that emits telemetry (gateways, log layer).
#[derive(Default)]
pub struct TelemetryBus {
    backpressure: Mutex<HashMap<WorkloadId, mpsc::Sender<BackpressureEvent>>>,
    qos_headroom: Mutex<HashMap<WorkloadId, mpsc::Sender<QosHeadroomBucket>>>,
}

impl TelemetryBus {
    /// Create an empty bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe `workload` to backpressure events. The previous subscription
    /// (if any) is replaced; its receiver will see no further events.
    pub fn subscribe_backpressure(&self, workload: &str) -> mpsc::Receiver<BackpressureEvent> {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_CAPACITY);
        self.backpressure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(workload.to_owned(), tx);
        rx
    }

    /// Subscribe `workload` to QoS-headroom events. Replaces any prior
    /// subscription for the same workload.
    pub fn subscribe_qos_headroom(&self, workload: &str) -> mpsc::Receiver<QosHeadroomBucket> {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_CAPACITY);
        self.qos_headroom
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(workload.to_owned(), tx);
        rx
    }

    /// Whether `workload` currently has a backpressure subscription.
    #[must_use]
    pub fn has_backpressure_subscription(&self, workload: &str) -> bool {
        self.backpressure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(workload)
            .is_some_and(|tx| !tx.is_closed())
    }

    /// Whether `workload` currently has a QoS-headroom subscription.
    #[must_use]
    pub fn has_qos_subscription(&self, workload: &str) -> bool {
        self.qos_headroom
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(workload)
            .is_some_and(|tx| !tx.is_closed())
    }

    /// Emit a backpressure event scoped to `workload`. No-op if no
    /// subscription exists. Drops the event if the subscriber is full
    /// (preserves the data path; advisory must never block).
    pub fn emit_backpressure(&self, workload: &str, event: BackpressureEvent) {
        if let Some(tx) = self
            .backpressure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(workload)
            .cloned()
        {
            let _ = tx.try_send(event);
        }
    }

    /// Emit a QoS-headroom bucket scoped to `workload`. No-op without a
    /// subscription; drops on full channel.
    pub fn emit_qos_headroom(&self, workload: &str, bucket: QosHeadroomBucket) {
        if let Some(tx) = self
            .qos_headroom
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(workload)
            .cloned()
        {
            let _ = tx.try_send(bucket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn backpressure_subscriber_receives_only_own_events() {
        let bus = TelemetryBus::new();
        let mut alice = bus.subscribe_backpressure("alice");
        let mut bob = bus.subscribe_backpressure("bob");

        bus.emit_backpressure(
            "alice",
            BackpressureEvent {
                severity: BackpressureSeverity::Soft,
                retry_after_ms: 100,
            },
        );

        let alice_evt = alice.recv().await.expect("alice receives event");
        assert_eq!(alice_evt.severity, BackpressureSeverity::Soft);
        assert_eq!(alice_evt.retry_after_ms, 100);

        // Bob has no event waiting — try_recv would return Empty.
        assert!(bob.try_recv().is_err(), "bob must not see alice's event");
    }

    #[tokio::test]
    async fn qos_headroom_per_workload_isolation() {
        let bus = TelemetryBus::new();
        let mut alice = bus.subscribe_qos_headroom("alice");
        let mut bob = bus.subscribe_qos_headroom("bob");

        bus.emit_qos_headroom("alice", QosHeadroomBucket::Tight);
        bus.emit_qos_headroom("bob", QosHeadroomBucket::Ample);

        assert_eq!(alice.recv().await.unwrap(), QosHeadroomBucket::Tight);
        assert_eq!(bob.recv().await.unwrap(), QosHeadroomBucket::Ample);
    }

    #[test]
    fn retry_after_buckets_to_fixed_set() {
        assert_eq!(bucket_retry_after_ms(0), 50);
        assert_eq!(bucket_retry_after_ms(50), 50);
        assert_eq!(bucket_retry_after_ms(75), 100);
        assert_eq!(bucket_retry_after_ms(150), 250);
        assert_eq!(bucket_retry_after_ms(10_000), 500);
    }
}
