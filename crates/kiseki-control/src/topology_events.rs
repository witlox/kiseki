//! Topology-event pub/sub bus (ADR-038 §D10, Phase 15d).
//!
//! Drain (ADR-035), shard split/merge (ADR-033/034), composition
//! deletion, and fh4 MAC-key rotation all need to deliver a fan-out
//! signal to the gateway-resident pNFS `LayoutManager` so it can fire
//! LAYOUTRECALL within the I-PN5 1-sec SLA.
//!
//! Producers emit **after** the underlying control-Raft commit
//! (so aborted transactions never fire), and the bus exposes a
//! standard `tokio::sync::broadcast` channel (capacity 1024). On
//! subscriber lag the receiver returns a `Lag(n)` indication —
//! consumers MUST invalidate their layout cache when this happens
//! (I-PN9). The 5-min I-PN4 layout TTL remains the ultimate safety
//! net even when every event-bus subscription fails.

use kiseki_common::ids::{CompositionId, NamespaceId, NodeId, OrgId, ShardId};

/// Cluster-topology events that affect outstanding pNFS layouts.
///
/// Spec: I-PN9, ADR-038 §D10.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopologyEvent {
    /// ADR-035 drain orchestrator transitioned `node_id` into the
    /// `Draining` state. Layouts referencing this node as a DS must
    /// be recalled.
    NodeDraining {
        /// Node entering drain.
        node_id: NodeId,
        /// HLC ms-since-epoch of the underlying state-transition commit.
        hlc_ms: u64,
    },
    /// `CancelDrain` returned a node to `Active` — gateways may stop
    /// blacklisting it.
    NodeRestored {
        /// Node returned to service.
        node_id: NodeId,
        /// HLC ms.
        hlc_ms: u64,
    },
    /// ADR-033 shard split commit. Layouts whose composition lives in
    /// the affected key range must be recalled.
    ShardSplit {
        /// Shard that was split.
        parent: ShardId,
        /// Resulting shards (always two for a split).
        children: [ShardId; 2],
        /// HLC ms.
        hlc_ms: u64,
    },
    /// ADR-034 shard merge commit.
    ShardMerged {
        /// Shards that were merged.
        inputs: Vec<ShardId>,
        /// Resulting shard id.
        merged: ShardId,
        /// HLC ms.
        hlc_ms: u64,
    },
    /// A composition was deleted; outstanding layouts referencing it
    /// must return `NFS4ERR_STALE` per RFC 8435 §6.
    CompositionDeleted {
        /// Owning tenant.
        tenant: OrgId,
        /// Namespace.
        namespace: NamespaceId,
        /// Composition that was deleted.
        composition: CompositionId,
        /// HLC ms.
        hlc_ms: u64,
    },
    /// fh4 MAC key rotation — every outstanding fh4 becomes invalid
    /// against the new key, so a bulk recall is required.
    KeyRotation {
        /// Identifier of the previous `K_layout`.
        old_key_id: String,
        /// Identifier of the new `K_layout` now in service.
        new_key_id: String,
        /// HLC ms.
        hlc_ms: u64,
    },
}

/// Capacity of the broadcast channel. ADR-038 §D10 default; tests
/// override via `TopologyEventBus::with_capacity`.
pub const DEFAULT_BUS_CAPACITY: usize = 1024;

/// Outcome of a single `recv()` on a topology subscription. Mirrors
/// `tokio::sync::broadcast::error::RecvError` shape but is owned by
/// us so the pub API stays stable across tokio bumps.
#[derive(Debug, PartialEq, Eq)]
pub enum TopologyRecvResult {
    /// Normal delivery.
    Event(TopologyEvent),
    /// Subscriber lagged; `n` events were dropped. Subscriber MUST
    /// invalidate its layout cache (I-PN9).
    Lag(u64),
    /// Sender side closed (bus dropped) — terminal.
    Closed,
}

/// Pub/sub bus that delivers `TopologyEvent`s to subscribers.
#[derive(Clone)]
pub struct TopologyEventBus {
    sender: tokio::sync::broadcast::Sender<TopologyEvent>,
    /// Total events successfully sent (for tests that need a structural
    /// witness — counts everything `emit()` calls, not subscriber receives).
    sent_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Cumulative count of `Lag(n)` indications observed by all
    /// subscribers. Phase 15c surfaces this through the
    /// `pnfs_topology_event_lag_total` Prometheus counter.
    lag_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// A single subscription. Wraps `tokio::sync::broadcast::Receiver`.
pub struct TopologyEventSubscriber {
    rx: tokio::sync::broadcast::Receiver<TopologyEvent>,
    lag_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl TopologyEventBus {
    /// Create a bus with the default (1024-event) capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUS_CAPACITY)
    }

    /// Create a bus with a specific channel capacity. Tests use small
    /// values to deterministically force lag.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _initial_rx) = tokio::sync::broadcast::channel(capacity.max(1));
        Self {
            sender,
            sent_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            lag_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Subscribe. Receivers see only events emitted *after* this call.
    #[must_use]
    pub fn subscribe(&self) -> TopologyEventSubscriber {
        TopologyEventSubscriber {
            rx: self.sender.subscribe(),
            lag_count: std::sync::Arc::clone(&self.lag_count),
        }
    }

    /// Emit an event AFTER its underlying control-Raft commit.
    /// Aborted transactions MUST NOT call this (I-PN9).
    ///
    /// Returns `Ok(receiver_count)` on success. `Err` only when there
    /// are zero subscribers — most production paths ignore the result.
    pub fn emit(&self, event: TopologyEvent) -> Result<usize, EmitError> {
        match self.sender.send(event) {
            Ok(n) => {
                self.sent_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(n)
            }
            Err(_) => Err(EmitError::NoSubscribers),
        }
    }

    /// Total number of successful `emit()` calls. Counts events
    /// regardless of whether subscribers were attached.
    #[must_use]
    pub fn sent_count(&self) -> u64 {
        self.sent_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cumulative number of `Lag(n)` indications observed by any
    /// subscriber on this bus. Surfaces I-PN9's metric witness.
    #[must_use]
    pub fn lag_count(&self) -> u64 {
        self.lag_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for TopologyEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl TopologyEventSubscriber {
    /// Wait for the next event. Increments the bus's lag counter on
    /// `Lag` so I-PN9's Prometheus counter reflects subscriber-side
    /// drops (the broadcast channel itself only knows about send-side).
    pub async fn recv(&mut self) -> TopologyRecvResult {
        match self.rx.recv().await {
            Ok(ev) => TopologyRecvResult::Event(ev),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                self.lag_count
                    .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                TopologyRecvResult::Lag(n)
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => TopologyRecvResult::Closed,
        }
    }

    /// Non-blocking variant for tests. Returns `None` if no event
    /// is currently buffered.
    pub fn try_recv(&mut self) -> Option<TopologyRecvResult> {
        match self.rx.try_recv() {
            Ok(ev) => Some(TopologyRecvResult::Event(ev)),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                self.lag_count
                    .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                Some(TopologyRecvResult::Lag(n))
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                Some(TopologyRecvResult::Closed)
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => None,
        }
    }
}

/// Failure modes from [`TopologyEventBus::emit`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EmitError {
    /// No subscribers attached. Almost always benign — the bus is
    /// designed to drop unobserved events. Producers in the runtime
    /// log this at `debug!` rather than treating it as a failure.
    #[error("no topology-event subscribers attached")]
    NoSubscribers,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::{CompositionId, NamespaceId, NodeId, OrgId, ShardId};

    fn drain_event(node: u64) -> TopologyEvent {
        TopologyEvent::NodeDraining {
            node_id: NodeId(node),
            hlc_ms: 1_000,
        }
    }

    #[tokio::test]
    async fn subscribe_then_emit_delivers_one_event() {
        let bus = TopologyEventBus::new();
        let mut sub = bus.subscribe();
        let _ = bus.emit(drain_event(1)).expect("subscriber attached");
        let got = sub.recv().await;
        assert_eq!(got, TopologyRecvResult::Event(drain_event(1)));
        assert_eq!(bus.sent_count(), 1);
    }

    #[tokio::test]
    async fn emit_with_no_subscribers_errors() {
        let bus = TopologyEventBus::new();
        // Default channel keeps no buffer for non-existent receivers,
        // so emit returns NoSubscribers.
        let err = bus.emit(drain_event(1)).unwrap_err();
        assert_eq!(err, EmitError::NoSubscribers);
        // sent_count is unchanged on error — only successful sends
        // count, by spec.
        assert_eq!(bus.sent_count(), 0);
    }

    #[tokio::test]
    async fn shard_split_event_round_trips() {
        let bus = TopologyEventBus::new();
        let mut sub = bus.subscribe();
        let ev = TopologyEvent::ShardSplit {
            parent: ShardId(uuid::Uuid::from_u128(1)),
            children: [
                ShardId(uuid::Uuid::from_u128(2)),
                ShardId(uuid::Uuid::from_u128(3)),
            ],
            hlc_ms: 2_000,
        };
        let _ = bus.emit(ev.clone()).unwrap();
        assert_eq!(sub.recv().await, TopologyRecvResult::Event(ev));
    }

    #[tokio::test]
    async fn composition_deleted_round_trips() {
        let bus = TopologyEventBus::new();
        let mut sub = bus.subscribe();
        let ev = TopologyEvent::CompositionDeleted {
            tenant: OrgId(uuid::Uuid::from_u128(7)),
            namespace: NamespaceId(uuid::Uuid::from_u128(8)),
            composition: CompositionId(uuid::Uuid::from_u128(9)),
            hlc_ms: 3_000,
        };
        let _ = bus.emit(ev.clone()).unwrap();
        assert_eq!(sub.recv().await, TopologyRecvResult::Event(ev));
    }

    #[tokio::test]
    async fn lag_is_signaled_when_subscriber_falls_behind() {
        // Capacity 4 → ringer overflows quickly.
        let bus = TopologyEventBus::with_capacity(4);
        let mut sub = bus.subscribe();
        // Emit 10 events without consuming any — channel-overflow.
        for i in 0..10 {
            let _ = bus.emit(drain_event(i));
        }
        // First recv reports lag (n=6 dropped, since capacity is 4).
        match sub.try_recv() {
            Some(TopologyRecvResult::Lag(n)) => assert!(n >= 1),
            other => panic!("expected Lag, got {other:?}"),
        }
        assert!(bus.lag_count() >= 1);
    }

    #[tokio::test]
    async fn try_recv_returns_none_when_empty() {
        let bus = TopologyEventBus::new();
        let mut sub = bus.subscribe();
        assert!(sub.try_recv().is_none());
    }

    #[tokio::test]
    async fn multiple_subscribers_each_see_every_event() {
        let bus = TopologyEventBus::new();
        let mut s1 = bus.subscribe();
        let mut s2 = bus.subscribe();
        let _ = bus.emit(drain_event(1)).unwrap();
        let _ = bus.emit(drain_event(2)).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                s1.recv().await,
                TopologyRecvResult::Event(TopologyEvent::NodeDraining { .. })
            ));
            assert!(matches!(
                s2.recv().await,
                TopologyRecvResult::Event(TopologyEvent::NodeDraining { .. })
            ));
        }
    }

    #[tokio::test]
    async fn key_rotation_carries_old_and_new_ids() {
        let bus = TopologyEventBus::new();
        let mut sub = bus.subscribe();
        let ev = TopologyEvent::KeyRotation {
            old_key_id: "v1".into(),
            new_key_id: "v2".into(),
            hlc_ms: 4_000,
        };
        let _ = bus.emit(ev.clone()).unwrap();
        assert_eq!(sub.recv().await, TopologyRecvResult::Event(ev));
    }
}
