//! Node lifecycle and drain orchestration (ADR-035).
//!
//! Implements the node state machine
//! (Active | Degraded | Failed | Draining | Evicted) and the drain
//! protocol with I-N4 capacity pre-check, I-N6 audit recording, and
//! I-N7 cancellation.
//!
//! Real Raft membership changes are delegated to the caller (which
//! owns the `RaftTestCluster` or production Raft handles); this module
//! is the control-plane state machine that gates them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use kiseki_common::ids::NodeId;
use kiseki_common::locks::LockOrDie;
use kiseki_common::raft_adapter::{MembershipError, RaftMembershipAdapter};

/// Per-node lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum NodeState {
    /// Healthy, accepting new shard assignments.
    Active,
    /// Degraded but still serving (device failures, SMART warnings).
    Degraded,
    /// Heartbeat timeout — not currently reachable.
    Failed,
    /// Operator-initiated graceful removal in progress.
    Draining,
    /// Terminal — removed from all Raft groups.
    Evicted,
}

/// Drain progress recorded against a node in `Draining` state.
#[derive(Clone, Debug, Default)]
pub struct DrainProgress {
    /// Total shards held by the node at drain start.
    pub total_shards: u32,
    /// Shards that have completed voter replacement.
    pub completed_shards: u32,
}

impl DrainProgress {
    /// Whether all voter replacements have completed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.completed_shards >= self.total_shards
    }
}

/// Persisted node record (ADR-035 §2 `NodeRecord`, abridged).
#[derive(Clone, Debug)]
pub struct NodeRecord {
    /// Node identifier.
    pub node_id: NodeId,
    /// Current lifecycle state.
    pub state: NodeState,
    /// Active drain progress, set while `state == Draining`.
    pub drain_progress: Option<DrainProgress>,
    /// Shards (by Raft node id under each shard's group) the node holds
    /// a voter slot for. For the in-memory test we just count entries.
    pub voter_in_shards: Vec<u64>,
}

/// Audit-event categories emitted by drain orchestration (ADR-035 §5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeAuditEvent {
    /// `DrainNode(target)` accepted; node moved Active → Draining.
    DrainRequested {
        /// Node the drain was requested against.
        node_id: NodeId,
        /// Admin identity that issued the request.
        admin: String,
    },
    /// `DrainNode(target)` rejected; reason recorded.
    DrainRefused {
        /// Node the drain was attempted against.
        node_id: NodeId,
        /// Admin identity that issued the request.
        admin: String,
        /// Human-readable refusal reason (typically the I-N4 message).
        reason: String,
    },
    /// `CancelDrain(target)` accepted; node moved Draining → Active.
    DrainCancelled {
        /// Node whose drain was cancelled.
        node_id: NodeId,
        /// Admin identity that issued the cancellation.
        admin: String,
    },
    /// All voter replacements complete; node moved Draining → Evicted.
    Evicted {
        /// Node that completed eviction.
        node_id: NodeId,
        /// Admin identity associated with the closing transition.
        admin: String,
    },
    /// Per-shard voter replacement completed during a drain.
    VoterReplaced {
        /// Node being drained.
        node_id: NodeId,
        /// Index of the affected shard within the node's voter list.
        shard_idx: u32,
        /// Node that received the replacement voter slot.
        replacement: NodeId,
    },
}

/// Errors returned by the drain orchestrator.
#[derive(Debug, thiserror::Error)]
pub enum DrainError {
    /// I-N4: pre-check failed because removing the node would leave
    /// at least one shard short of the replication factor.
    #[error("DrainRefused: insufficient capacity to maintain RF={0}")]
    InsufficientCapacity(u32),
    /// Operator referenced an unknown node.
    #[error("unknown node: {0:?}")]
    UnknownNode(NodeId),
    /// Operation forbidden in the node's current state (e.g.,
    /// `CancelDrain` on an Active or Evicted node).
    #[error("invalid state transition: {from:?} → {to:?}")]
    InvalidTransition {
        /// State the node was in when the transition was attempted.
        from: NodeState,
        /// State the operator tried to move it to.
        to: NodeState,
    },
    /// The Raft membership adapter rejected a step of the drain.
    #[error("raft membership: {0}")]
    Membership(#[from] MembershipError),
}

/// Replication factor enforced by the orchestrator (I-N4).
const REPLICATION_FACTOR: u32 = 3;

/// In-memory drain orchestrator. Wraps a `NodeRegistry` and an audit
/// trail; production deployments persist both in the control-plane
/// Raft group (ADR-035 §2). The state machine is the same.
#[derive(Default)]
pub struct DrainOrchestrator {
    inner: Mutex<Inner>,
    /// Optional pNFS topology bus (ADR-038 §D10). When set, drain
    /// state transitions emit `NodeDraining` / `NodeRestored` events
    /// AFTER the state is recorded — matching the post-Raft-commit
    /// invariant in production.
    event_bus: std::sync::OnceLock<std::sync::Arc<crate::topology_events::TopologyEventBus>>,
}

#[derive(Default)]
struct Inner {
    nodes: HashMap<NodeId, NodeRecord>,
    audit: Vec<NodeAuditEvent>,
}

impl DrainOrchestrator {
    /// Create an empty orchestrator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the pNFS topology event bus (ADR-038 §D10). Idempotent
    /// per `OnceLock` semantics — second call is a no-op.
    #[must_use]
    pub fn with_event_bus(
        self,
        bus: std::sync::Arc<crate::topology_events::TopologyEventBus>,
    ) -> Self {
        let _ = self.event_bus.set(bus);
        self
    }

    /// Register a node in the cluster (typically called when a node
    /// joins). Idempotent on the node id.
    pub fn register_node(&self, node_id: NodeId, voter_in_shards: Vec<u64>) {
        let mut inner = self.inner.lock().lock_or_die("node_lifecycle.inner");
        inner.nodes.entry(node_id).or_insert(NodeRecord {
            node_id,
            state: NodeState::Active,
            drain_progress: None,
            voter_in_shards,
        });
    }

    /// Replace a node's voter-in-shards list. The orchestrator's
    /// `register_node` is `or_insert`, so callers that need to
    /// adjust the voter set after registration use this helper.
    /// No-op if the node is unknown.
    pub fn set_voters(&self, node_id: NodeId, voter_in_shards: Vec<u64>) {
        let mut inner = self.inner.lock().lock_or_die("node_lifecycle.inner");
        if let Some(rec) = inner.nodes.get_mut(&node_id) {
            rec.voter_in_shards = voter_in_shards;
            // If the node was already in Draining, refresh the total
            // count so subsequent `record_voter_replaced` advances
            // toward the new totals.
            if rec.state == NodeState::Draining {
                let total = u32::try_from(rec.voter_in_shards.len()).unwrap_or(0);
                rec.drain_progress = Some(DrainProgress {
                    total_shards: total,
                    completed_shards: 0,
                });
            }
        }
    }

    /// Mark a node as `Draining` directly (used to set up the
    /// "node in Draining state" precondition for cancel scenarios).
    pub fn set_state(&self, node_id: NodeId, state: NodeState) {
        let mut inner = self.inner.lock().lock_or_die("node_lifecycle.inner");
        if let Some(rec) = inner.nodes.get_mut(&node_id) {
            rec.state = state;
            if state == NodeState::Draining && rec.drain_progress.is_none() {
                let total = u32::try_from(rec.voter_in_shards.len()).unwrap_or(0);
                rec.drain_progress = Some(DrainProgress {
                    total_shards: total,
                    completed_shards: 0,
                });
            }
        }
    }

    /// Lookup the current state of a node.
    #[must_use]
    pub fn state(&self, node_id: NodeId) -> Option<NodeState> {
        self.inner
            .lock()
            .lock_or_die("node_lifecycle.inner")
            .nodes
            .get(&node_id)
            .map(|n| n.state)
    }

    /// Snapshot of all audit events so far.
    pub fn audit(&self) -> Vec<NodeAuditEvent> {
        self.inner
            .lock()
            .lock_or_die("node_lifecycle.inner")
            .audit
            .clone()
    }

    /// I-N4 pre-check: would removing `target` leave any shard short
    /// of `REPLICATION_FACTOR` voters in `{Active, Degraded}` state?
    ///
    /// Considers the entire node registry — not the target's voter
    /// list — because replacements draw from any other surviving
    /// drain-eligible node not already in the shard's voter set.
    fn precheck(inner: &Inner, target: NodeId) -> Result<(), DrainError> {
        // Drain-eligible replacement candidates: not the target, and
        // currently in {Active, Degraded}. Failed nodes are excluded
        // (they cannot host new voters).
        let candidate_count = inner
            .nodes
            .iter()
            .filter(|(id, n)| {
                **id != target && matches!(n.state, NodeState::Active | NodeState::Degraded)
            })
            .count();

        let target_shards = inner
            .nodes
            .get(&target)
            .map_or(0, |n| n.voter_in_shards.len());

        // Every shard the target holds is currently {target ∪ RF-1
        // others}. Replacing target requires a candidate that is *not*
        // already in that voter set — i.e., a node outside the
        // RF-sized voter set. With a uniform topology that means the
        // cluster needs at least RF+1 drain-eligible nodes (target +
        // RF-1 surviving voters + ≥1 replacement). Equivalently, the
        // count of drain-eligible candidates excluding target must be
        // at least RF.
        if target_shards > 0
            && u32::try_from(candidate_count).unwrap_or(u32::MAX) < REPLICATION_FACTOR
        {
            return Err(DrainError::InsufficientCapacity(REPLICATION_FACTOR));
        }
        Ok(())
    }

    /// Request a drain on `target`. Records `DrainRequested` on success
    /// or `DrainRefused` on capacity failure (both visible via `audit()`).
    pub fn request_drain(&self, target: NodeId, admin: &str) -> Result<(), DrainError> {
        let mut inner = self.inner.lock().lock_or_die("node_lifecycle.inner");
        if !inner.nodes.contains_key(&target) {
            return Err(DrainError::UnknownNode(target));
        }
        if let Err(e) = Self::precheck(&inner, target) {
            inner.audit.push(NodeAuditEvent::DrainRefused {
                node_id: target,
                admin: admin.to_owned(),
                reason: e.to_string(),
            });
            return Err(e);
        }

        let rec = inner.nodes.get_mut(&target).expect("checked above");
        let from = rec.state;
        if !matches!(
            from,
            NodeState::Active | NodeState::Degraded | NodeState::Failed
        ) {
            return Err(DrainError::InvalidTransition {
                from,
                to: NodeState::Draining,
            });
        }
        rec.state = NodeState::Draining;
        let total = u32::try_from(rec.voter_in_shards.len()).unwrap_or(0);
        rec.drain_progress = Some(DrainProgress {
            total_shards: total,
            completed_shards: 0,
        });
        inner.audit.push(NodeAuditEvent::DrainRequested {
            node_id: target,
            admin: admin.to_owned(),
        });
        // Drop the lock before emitting (avoid holding lock across
        // subscribers' channel sends).
        drop(inner);
        if let Some(bus) = self.event_bus.get() {
            let hlc_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
            let _ = bus.emit(crate::topology_events::TopologyEvent::NodeDraining {
                node_id: target,
                hlc_ms,
            });
        }
        Ok(())
    }

    /// Cancel an in-progress drain (I-N7). Already-completed voter
    /// replacements stay where they are — see ADR-035 §4.
    pub fn cancel_drain(&self, target: NodeId, admin: &str) -> Result<(), DrainError> {
        let mut inner = self.inner.lock().lock_or_die("node_lifecycle.inner");
        let rec = inner
            .nodes
            .get_mut(&target)
            .ok_or(DrainError::UnknownNode(target))?;
        if rec.state != NodeState::Draining {
            return Err(DrainError::InvalidTransition {
                from: rec.state,
                to: NodeState::Active,
            });
        }
        rec.state = NodeState::Active;
        rec.drain_progress = None;
        inner.audit.push(NodeAuditEvent::DrainCancelled {
            node_id: target,
            admin: admin.to_owned(),
        });
        drop(inner);
        if let Some(bus) = self.event_bus.get() {
            let hlc_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
            let _ = bus.emit(crate::topology_events::TopologyEvent::NodeRestored {
                node_id: target,
                hlc_ms,
            });
        }
        Ok(())
    }

    /// Mark a per-shard voter replacement complete during a drain. When
    /// all voter replacements complete the node transitions to Evicted
    /// and an `Evicted` audit event is emitted.
    pub fn record_voter_replaced(
        &self,
        target: NodeId,
        shard_idx: u32,
        replacement: NodeId,
        admin: &str,
    ) {
        let mut inner = self.inner.lock().lock_or_die("node_lifecycle.inner");

        // Compute the state transition first, then push audit events,
        // to keep two disjoint mutable borrows on `inner` from
        // overlapping (nodes vs audit).
        let (replaced, evicted) = match inner.nodes.get_mut(&target) {
            Some(rec) if rec.state == NodeState::Draining => {
                if let Some(progress) = rec.drain_progress.as_mut() {
                    progress.completed_shards = progress.completed_shards.saturating_add(1);
                }
                let done = rec
                    .drain_progress
                    .as_ref()
                    .is_some_and(DrainProgress::is_complete);
                if done {
                    rec.state = NodeState::Evicted;
                    rec.drain_progress = None;
                }
                (true, done)
            }
            _ => (false, false),
        };

        if replaced {
            inner.audit.push(NodeAuditEvent::VoterReplaced {
                node_id: target,
                shard_idx,
                replacement,
            });
        }
        if evicted {
            inner.audit.push(NodeAuditEvent::Evicted {
                node_id: target,
                admin: admin.to_owned(),
            });
        }
    }

    /// Refresh the per-node `voter_in_shards` count from the supplied
    /// adapter. Avoids the stale-snapshot trap (integrator finding 5b)
    /// where the orchestrator's view of the cluster diverges from the
    /// actual Raft membership over time.
    pub async fn resync_from_raft<A: RaftMembershipAdapter + ?Sized>(
        &self,
        adapter: &A,
    ) -> Result<(), DrainError> {
        let voters = adapter.voter_ids().await?;
        let mut inner = self.inner.lock().lock_or_die("node_lifecycle.inner");
        for (id, rec) in &mut inner.nodes {
            // Each shard the adapter reports the node holds adds an entry.
            // We count occurrences (a node may hold multiple voter slots
            // across shards in a multi-shard cluster).
            let count = voters.iter().filter(|v| *v == id).count();
            rec.voter_in_shards.clear();
            for i in 0..count {
                rec.voter_in_shards.push(i as u64);
            }
        }
        Ok(())
    }

    /// Drive every per-shard voter replacement for a node that is
    /// already in `Draining` state. Calls `add_learner` + `replace_voter`
    /// on `adapter` once per shard the target holds, records each
    /// completion via `record_voter_replaced`, and auto-evicts on the
    /// final step. Operators that pre-stage state via `request_drain`
    /// (e.g. acceptance tests that need to assert intermediate state)
    /// invoke this directly; full end-to-end callers use [`Self::execute_drain`].
    pub async fn drive_voter_replacements<A: RaftMembershipAdapter + ?Sized>(
        &self,
        target: NodeId,
        replacement: NodeId,
        admin: &str,
        adapter: &mut A,
    ) -> Result<(), DrainError> {
        let shard_count = self
            .inner
            .lock()
            .lock_or_die("node_lifecycle.inner")
            .nodes
            .get(&target)
            .map_or(0, |n| n.voter_in_shards.len());

        for shard_idx in 0..shard_count {
            adapter.add_learner(replacement).await?;
            adapter.replace_voter(target, replacement).await?;
            self.record_voter_replaced(
                target,
                u32::try_from(shard_idx).unwrap_or(u32::MAX),
                replacement,
                admin,
            );
        }
        Ok(())
    }

    /// End-to-end drain orchestration (ADR-035 §3): pre-check, mark
    /// Draining, drive every per-shard voter replacement through
    /// `adapter`, record audit, auto-evict on completion.
    ///
    /// `replacement` is the operator-chosen target for the new voter
    /// slots. Production deployments will call this from the control
    /// service; tests call it from BDD step definitions.
    pub async fn execute_drain<A: RaftMembershipAdapter + ?Sized>(
        &self,
        target: NodeId,
        replacement: NodeId,
        admin: &str,
        adapter: &mut A,
    ) -> Result<(), DrainError> {
        // Sync registry from authoritative Raft state first so the I-N4
        // pre-check sees real voter counts, not the operator's guess.
        self.resync_from_raft(adapter).await?;

        // State transition + audit (returns InsufficientCapacity on I-N4 fail).
        self.request_drain(target, admin)?;
        self.drive_voter_replacements(target, replacement, admin, adapter)
            .await
    }

    /// Snapshot of the full registry — used by tests to observe state
    /// transitions without exposing the internal mutex.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<NodeId, NodeRecord> {
        let inner = self.inner.lock().lock_or_die("node_lifecycle.inner");
        inner.nodes.iter().map(|(k, v)| (*k, v.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(i: u128) -> NodeId {
        NodeId(u64::try_from(i).expect("fits"))
    }

    #[test]
    fn drain_succeeds_with_replacement_capacity() {
        let orch = DrainOrchestrator::new();
        // 5 nodes, target n7 — plenty of capacity.
        for i in 1..=5u128 {
            orch.register_node(n(i), vec![u64::try_from(i).expect("fits")]);
        }
        orch.register_node(n(7), vec![7]);

        orch.request_drain(n(7), "alice").expect("drain accepted");
        assert_eq!(orch.state(n(7)), Some(NodeState::Draining));
        assert!(matches!(
            orch.audit().last(),
            Some(NodeAuditEvent::DrainRequested { node_id, .. }) if *node_id == n(7),
        ));
    }

    #[test]
    fn drain_refused_when_only_three_active_nodes() {
        let orch = DrainOrchestrator::new();
        for i in 1..=3u128 {
            orch.register_node(n(i), vec![u64::try_from(i).expect("fits")]);
        }
        let err = orch
            .request_drain(n(1), "alice")
            .expect_err("expected refusal");
        assert!(matches!(err, DrainError::InsufficientCapacity(3)));
        assert!(matches!(
            orch.audit().last(),
            Some(NodeAuditEvent::DrainRefused { .. })
        ));
        // Node remains Active.
        assert_eq!(orch.state(n(1)), Some(NodeState::Active));
    }

    #[test]
    fn drain_re_issued_after_replacement_node_added() {
        let orch = DrainOrchestrator::new();
        for i in 1..=3u128 {
            orch.register_node(n(i), vec![u64::try_from(i).expect("fits")]);
        }
        // First attempt — refused.
        assert!(orch.request_drain(n(1), "alice").is_err());
        // Operator adds n4.
        orch.register_node(n(4), vec![]);
        // Re-issue — accepted.
        orch.request_drain(n(1), "alice").expect("drain accepted");
        assert_eq!(orch.state(n(1)), Some(NodeState::Draining));
        // Audit shows refusal then acceptance.
        let audit = orch.audit();
        assert!(matches!(audit[0], NodeAuditEvent::DrainRefused { .. }));
        assert!(matches!(audit[1], NodeAuditEvent::DrainRequested { .. }));
    }

    #[test]
    fn cancel_drain_returns_node_to_active() {
        let orch = DrainOrchestrator::new();
        for i in 1..=5u128 {
            orch.register_node(n(i), vec![u64::try_from(i).expect("fits")]);
        }
        orch.request_drain(n(1), "alice").unwrap();
        orch.cancel_drain(n(1), "alice").unwrap();
        assert_eq!(orch.state(n(1)), Some(NodeState::Active));
        assert!(matches!(
            orch.audit().last(),
            Some(NodeAuditEvent::DrainCancelled { .. })
        ));
    }

    #[test]
    fn voter_replacement_completes_drives_eviction() {
        let orch = DrainOrchestrator::new();
        for i in 1..=5u128 {
            orch.register_node(n(i), vec![u64::try_from(i).expect("fits")]);
        }
        // Target holds two shards.
        orch.register_node(n(7), vec![701, 702]);
        orch.request_drain(n(7), "alice").unwrap();

        orch.record_voter_replaced(n(7), 0, n(2), "alice");
        assert_eq!(orch.state(n(7)), Some(NodeState::Draining));
        orch.record_voter_replaced(n(7), 1, n(3), "alice");

        assert_eq!(orch.state(n(7)), Some(NodeState::Evicted));
        let audit = orch.audit();
        assert!(audit
            .iter()
            .any(|e| matches!(e, NodeAuditEvent::Evicted { node_id, .. } if *node_id == n(7))));
    }

    /// Capturing adapter — records every membership call so the test can
    /// assert that `execute_drain` actually drove the adapter.
    #[derive(Default)]
    struct CapturingAdapter {
        added: std::sync::Mutex<Vec<NodeId>>,
        replaced: std::sync::Mutex<Vec<(NodeId, NodeId)>>,
        voters: Vec<NodeId>,
    }

    impl RaftMembershipAdapter for CapturingAdapter {
        fn add_learner(
            &mut self,
            replacement: NodeId,
        ) -> kiseki_common::raft_adapter::MembershipFuture<'_, ()> {
            Box::pin(async move {
                self.added.lock().unwrap().push(replacement);
                Ok(())
            })
        }

        fn replace_voter(
            &mut self,
            target: NodeId,
            replacement: NodeId,
        ) -> kiseki_common::raft_adapter::MembershipFuture<'_, ()> {
            Box::pin(async move {
                self.replaced.lock().unwrap().push((target, replacement));
                Ok(())
            })
        }

        fn voter_ids(&self) -> kiseki_common::raft_adapter::MembershipFuture<'_, Vec<NodeId>> {
            let v = self.voters.clone();
            Box::pin(async move { Ok(v) })
        }
    }

    /// ADR-035 §1: a Failed node remains drain-eligible — operator
    /// decides the node is permanently lost and drives the drain.
    #[test]
    fn drain_accepted_for_failed_node() {
        let orch = DrainOrchestrator::new();
        for i in 1..=5u128 {
            orch.register_node(n(i), vec![u64::try_from(i).expect("fits")]);
        }
        // Mark target as Failed (heartbeat timeout) — it's still in the
        // registry, just not reachable.
        orch.set_state(n(1), NodeState::Failed);

        // Operator drains it. State machine accepts Failed → Draining.
        orch.request_drain(n(1), "alice")
            .expect("drain of a Failed node must be accepted");
        assert_eq!(orch.state(n(1)), Some(NodeState::Draining));
    }

    /// ADR-035 §4: `cancel_drain` returns the node to Active. Per the
    /// architect note, completed voter replacements DO NOT roll back
    /// — the cluster keeps the new placements, the cancelled node just
    /// stops draining.
    #[test]
    fn cancel_drain_does_not_roll_back_completed_replacements() {
        let orch = DrainOrchestrator::new();
        for i in 1..=5u128 {
            orch.register_node(n(i), vec![u64::try_from(i).expect("fits")]);
        }
        orch.register_node(n(7), vec![701, 702]);

        orch.request_drain(n(7), "alice").unwrap();
        // First shard's voter replacement completes BEFORE cancellation.
        orch.record_voter_replaced(n(7), 0, n(2), "alice");

        // Operator cancels mid-drain.
        orch.cancel_drain(n(7), "alice").unwrap();
        assert_eq!(orch.state(n(7)), Some(NodeState::Active));

        // The completed VoterReplaced audit entry MUST still be there —
        // no rollback. The cluster has new placements that survive cancel.
        let audit = orch.audit();
        let kept = audit
            .iter()
            .filter(|e| matches!(e, NodeAuditEvent::VoterReplaced { .. }))
            .count();
        assert_eq!(kept, 1, "completed VoterReplaced must persist past cancel");
    }

    /// Audit events MUST be ordered by occurrence so a replay can
    /// reconstruct the exact transition sequence.
    #[test]
    fn audit_event_order_matches_call_order() {
        let orch = DrainOrchestrator::new();
        for i in 1..=5u128 {
            orch.register_node(n(i), vec![u64::try_from(i).expect("fits")]);
        }
        orch.register_node(n(7), vec![701]);

        orch.request_drain(n(7), "alice").unwrap();
        orch.record_voter_replaced(n(7), 0, n(2), "alice");

        let audit = orch.audit();
        // Order: DrainRequested → VoterReplaced → Evicted (eviction
        // fires automatically once all replacements complete).
        assert!(
            matches!(audit[0], NodeAuditEvent::DrainRequested { .. }),
            "first event is DrainRequested",
        );
        assert!(
            matches!(audit[1], NodeAuditEvent::VoterReplaced { .. }),
            "second event is VoterReplaced",
        );
        assert!(
            matches!(audit[2], NodeAuditEvent::Evicted { .. }),
            "third event is Evicted",
        );
        assert_eq!(audit.len(), 3, "no extra events between transitions");
    }

    #[tokio::test]
    async fn execute_drain_drives_adapter_and_evicts() {
        let orch = DrainOrchestrator::new();
        for i in 1..=4u128 {
            orch.register_node(n(i), vec![]);
        }
        // Target known to hold two shards in the adapter's view (n7
        // appears twice in the voter list — multi-shard membership).
        orch.register_node(n(7), vec![]);
        let mut adapter = CapturingAdapter {
            voters: vec![n(1), n(2), n(3), n(4), n(7), n(7)],
            ..CapturingAdapter::default()
        };

        orch.execute_drain(n(7), n(8), "alice", &mut adapter)
            .await
            .expect("drain should complete");

        // Resync populated voter_in_shards from the adapter's voter list.
        // Two shard slots → two add_learner + two replace_voter calls.
        assert_eq!(adapter.added.lock().unwrap().len(), 2);
        assert_eq!(adapter.replaced.lock().unwrap().len(), 2);
        assert_eq!(orch.state(n(7)), Some(NodeState::Evicted));
        let audit = orch.audit();
        assert!(audit
            .iter()
            .any(|e| matches!(e, NodeAuditEvent::Evicted { node_id, .. } if *node_id == n(7))));
    }
}
