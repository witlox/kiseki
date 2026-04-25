# ADR-035: Node Lifecycle and Drain Protocol

**Status**: Accepted
**Date**: 2026-04-25
**Deciders**: Architect + domain expert
**Adversarial review**: 2026-04-25 (10 findings across ADR-033/034/035: 3H 6M 1L, all resolved)
**Context**: ADR-026 (Raft topology), ADR-033 (persistent shard map),
I-N1..I-N7, I-SF4 (migration concurrency cap), F-O4, F-O5

## Problem

Kiseki has no node lifecycle management. The Raft membership primitives
exist (`AddLearner`, `PromoteVoter`, `RemoveVoter` in `kiseki-raft/
src/membership.rs`) but there is no orchestration layer that:

1. Tracks node state (Active, Draining, Evicted)
2. Coordinates leadership transfer off a retiring node
3. Replaces voters on a draining node's shards with new voters on
   surviving nodes
4. Enforces RF=3 at every intermediate state
5. Refuses unsafe drains (would violate RF)
6. Supports cancellation of in-progress drains

Without this, hardware retirement or node replacement requires manual
Raft membership manipulation — error-prone and undocumented.

## Decision

### 1. Node state machine (I-N1)

Five states covering the full node lifecycle: onboarding, steady state,
degradation, unplanned failure, and operator-initiated offboarding.

```
                    automatic               automatic
        ┌──────── (resolved) ◄──────── (heartbeat timeout) ◄────┐
        │                                                        │
        ▼         automatic              automatic               │
     Active ──────────────► Degraded ────────────► Failed         │
        │                      │                     │            │
        │ operator             │ operator            │ operator   │
        │ DrainNode            │ DrainNode            │ DrainNode  │
        │                      │                     │            │
        └──────────┬───────────┘                     │            │
                   ▼                                 │            │
               Draining ◄───────────────────────────┘            │
                   │                                              │
        ┌──────────┼──────────────────────┐                      │
        │ cancel   │ all shards done      │                      │
        ▼          ▼                      │                      │
     Active     Evicted (TERMINAL)        │                      │
                                          │                      │
     Active ◄─── (node recovers) ─── Failed ─────────────────────┘
```

| State | Meaning | Serves traffic? | New shard assignments? |
|-------|---------|-----------------|------------------------|
| Active | Healthy, fully operational | Yes | Yes |
| Degraded | Partial device failures, SMART warnings, high error rate — still reachable | Yes (reduced capacity) | No (except as drain replacement target — ADV-035-10) |
| Failed | Unreachable (heartbeat timeout, crash, network partition) | No | No |
| Draining | Operator-initiated graceful removal | Yes (existing shards only) | No |
| Evicted | Terminal — removed from all Raft groups | No | No |

#### Transitions

| From | To | Trigger | Reversible? |
|------|----|---------|-------------|
| (new) | Active | Node joins cluster, registers with control plane | No |
| Active | Degraded | Automatic: device failure count > threshold, SMART warning, sustained high error rate | Yes |
| Degraded | Active | Automatic: all degradation conditions clear (devices repaired/evacuated, error rate normal) | — |
| Active | Failed | Automatic: heartbeat timeout (configurable, default 30s) | Yes |
| Degraded | Failed | Automatic: heartbeat timeout while degraded | Yes |
| Failed | Active | Automatic: node recovers, heartbeat resumes, Raft log catches up | — |
| Active | Draining | Operator: `DrainNode(X)` after I-N4 pre-check | Yes (cancel) |
| Degraded | Draining | Operator: `DrainNode(X)` — degraded nodes are drain candidates | Yes (cancel) |
| Failed | Draining | Operator: `DrainNode(X)` — operator decides node is not coming back | Yes (cancel) |
| Draining | Active | Operator: `CancelDrain(X)` (I-N7) | — |
| Draining | Evicted | Automatic: all voter replacements complete | No |

#### Degraded detection

A node enters `Degraded` when any of:
- ≥ 1 device in `Failed` state (I-D2)
- Any device reports SMART wear > 80% (SSD) or > 50 bad sectors (HDD)
  (below the auto-evacuation thresholds of I-D3 but worth flagging)
- Sustained error rate > 0.1% on any device over 5-minute window
- Node self-reports clock quality `Unsync` (I-T6)

A `Degraded` node clears to `Active` automatically when all
degradation conditions resolve — no admin confirmation required.
`Degraded` is an observation of device/health conditions, not an
operator decision; when conditions clear (e.g., failed disk swapped
via `AddDevice`, SMART warning resolved, error rate drops), the
corresponding `DegradationReason` is removed, and when the list
empties the node returns to `Active` and resumes accepting new shard
assignments.

Degradation does not trigger automatic drain. The operator decides
whether to drain based on degradation severity. A future ADR may
add auto-drain policies (e.g., "drain after 24h degraded").

#### Failed detection and recovery

A node enters `Failed` when no heartbeat has been received for the
`node_failure_timeout` (default 30s, configurable). This is detected
by the control plane's node health monitor.

On failure:
- Raft election handles leader failover for affected shards (~300ms)
- The node's shards continue with remaining voters (RF-1 temporarily)
- No automatic voter replacement — the node may recover

If the node recovers (heartbeat resumes):
- Returns to `Active` (or `Degraded` if pre-failure conditions persist)
- Raft log catch-up from leader (snapshot if too far behind)

If the operator decides the node is permanently lost:
- `DrainNode(X)` transitions `Failed → Draining`
- Voter replacement proceeds as normal (the failed node's voter slots
  are replaced on surviving nodes)

**Evicted is terminal.** Re-adding a node requires a fresh node
identity (new `NodeId`). This prevents stale state from a previously
evicted node from contaminating the cluster.

### 2. Node state storage

Node state is stored in the control plane's Raft group alongside the
namespace shard map (ADR-033). Data model:

```
NodeRecord {
    node_id: NodeId,
    addr: String,
    state: NodeState,         // Active | Degraded | Failed | Draining | Evicted
    degradation_reasons: Vec<DegradationReason>,  // empty when Active
    drain_progress: Option<DrainProgress>,
    last_heartbeat: HLC,
    joined_at: HLC,
    state_changed_at: HLC,
}

DegradationReason {
    kind: DegradationKind,    // DeviceFailed | SmartWarning | HighErrorRate | ClockUnsync
    device_id: Option<DeviceId>,
    detail: String,
    detected_at: HLC,
}

DrainProgress {
    total_shards: u32,
    completed_shards: u32,
    in_flight_shards: Vec<ShardId>,
    pending_shards: Vec<ShardId>,
}
```

### 3. Drain protocol

#### Pre-check (I-N4)

Before accepting `DrainNode(target)`:

1. For every shard where `target` holds a voter slot, compute the
   voter set after removal: `remaining_voters = voters - {target}`
2. For each shard, check whether a replacement voter can be placed
   on a surviving node in `{Active, Degraded}` state that is not
   already in the voter set (Degraded nodes are eligible as drain
   replacement targets because they are still serving traffic —
   ADV-035-10)
3. If any shard cannot find a replacement host:
   reject with `DrainRefused: insufficient capacity to maintain RF=N`
4. Record the refusal in the cluster audit shard (I-N6)

**Why refuse rather than auto-add:** the operator is in the best
position to choose which replacement hardware to add. Auto-provisioning
would require infrastructure integration (cloud API, BMC, etc.) that
is out of scope.

#### Phase 1: Leadership transfer (I-N2)

For every shard where `target` is the leader:

1. Select a new leader per I-L12 (fewest leaders for namespace,
   tie-break on lowest NodeId, from the existing voter set)
2. Use openraft's `trigger_transfer_leader()` to transfer leadership
3. Wait for the new leader to confirm (election completes)
4. `target` is now a follower (or voter) for this shard, not leader

Leadership transfers are fast (single election round, ~300ms worst case)
and do not require data movement.

#### Phase 2: Voter replacement (I-N3, I-N5)

For every shard where `target` holds a voter slot:

```
1. Select replacement node:
   - Active node not already in the shard's voter/learner set
   - Per I-L12: fewest leaders for the namespace, tie-break NodeId
2. Add replacement as learner: AddLearner(replacement, addr)
3. Wait for learner to catch up to leader's committed index
   (snapshot transfer if far behind)
4. Promote learner to voter: PromoteVoter(replacement)
5. Remove target from voter set: RemoveVoter(target)
```

**Critical invariant:** at every intermediate step, the shard has
≥ RF voters. Step 4 (promote) runs before step 5 (remove), so the
voter count temporarily increases to RF+1 before dropping back to RF.

**Draining a Failed node (ADV-035-5)**: voter removal of a Failed
node proceeds via the Raft leader without the target's participation.
The leader commits the membership change; the removed node does not
need to acknowledge. If the removed node later recovers with stale
membership, it receives `AppendEntries` with a higher term that
includes its removal — it steps down and does not attempt to rejoin
the voter set. The `NodeRecord` in the control plane (state=Evicted)
is the authoritative source; stale Raft state on a recovered-then-
evicted node is harmless.

#### Concurrency control (I-SF4)

Voter replacements for different shards can run in parallel, bounded
by:

```
max_concurrent_migrations = max(1, active_node_count / 10)
```

This prevents Raft instability from too many simultaneous membership
changes. Remaining replacements are queued and processed in shard-ID
order.

#### Phase 3: Eviction

When all voter replacements complete (and leadership is transferred
for all affected shards):

1. Transition node state: `Draining → Evicted`
2. Record transition in cluster audit shard (I-N6)
3. Signal completion to the operator with a per-shard summary

### 4. Drain cancellation (I-N7)

**Architect validation of A-N7:** drain cancellation with no rollback
of completed voter replacements is confirmed as the correct behavior.

Rationale:
- Rolling back completed voter replacements requires removing the new
  voter and re-adding the old one. This is complex and risks a window
  where RF drops below 3.
- The completed replacements are valid placements — they just happen
  to not include the original node. The cluster operates correctly.
- If the operator wants the cancelling node to rejoin specific shards,
  they can use explicit voter management (future `AddVoterToShard` RPC).

On `CancelDrain(target)`:

1. Verify `target.state == Draining` (reject if Active or Evicted)
2. Abort in-flight voter replacements (learners being added for
   pending shards are removed)
3. Transition: `Draining → Active`
4. `target` resumes accepting new leader and voter assignments
5. Shards that completed voter replacement remain with their new
   placement — no rollback
6. Record cancellation in cluster audit shard

### 5. Audit events (I-N6)

| Event | Fields | Audit shard |
|-------|--------|-------------|
| `NodeDegraded` | node_id, reasons, timestamp | Cluster |
| `NodeDegradationCleared` | node_id, timestamp | Cluster |
| `NodeFailed` | node_id, last_heartbeat, timestamp | Cluster |
| `NodeRecovered` | node_id, downtime_duration, timestamp | Cluster |
| `NodeDrainRequested` | node_id, admin_id, from_state, timestamp | Cluster |
| `NodeDrainRefused` | node_id, admin_id, reason, timestamp | Cluster |
| `NodeDrainCancelled` | node_id, admin_id, reason, timestamp | Cluster |
| `NodeLeadershipTransferred` | node_id, shard_id, old_leader, new_leader, timestamp | Cluster |
| `NodeVoterReplaced` | node_id, shard_id, replacement_node, timestamp | Cluster |
| `NodeEvicted` | node_id, admin_id, timestamp, per_shard_summary | Cluster |

All events include the admin identity (from mTLS or IAM session) and
are independent of device state transitions (I-D2).

### 6. Failure handling (F-O4)

The drain orchestrator persists progress in `DrainProgress` (stored
in the control plane Raft group). On crash:

1. On restart, scan for nodes in `Draining` state
2. Load `DrainProgress` to determine which shards have completed,
   which are in-flight, and which are pending
3. Resume from the last known state:
   - In-flight: check if the learner was promoted; if not, restart
     from step 2 (add learner) for that shard
   - Pending: process normally
4. Report resumed drain progress to operator

Because voter replacement is always `add → catch-up → promote → remove`,
and each step is observable via the Raft membership state, the
orchestrator can safely determine where it left off.

### 7. Code changes required

| File | Current state | Required change |
|------|--------------|-----------------|
| `kiseki-control/src/` | No node lifecycle | Add `node.rs`: `NodeRecord`, `NodeState`, `DrainOrchestrator` |
| `kiseki-raft/src/membership.rs` | Primitives exist | No change — orchestration calls existing `validate_membership_change` |
| `kiseki-control/src/namespace.rs` | No node-aware placement | Placement must exclude Draining/Evicted nodes |
| `kiseki-audit/src/` | No drain events | Add drain event types |
| `kiseki-server/src/runtime.rs` | No drain endpoint | Add `DrainNode`, `CancelDrain` RPC handlers |

### 8. CLI interface

```
kiseki-admin node list                    # show all nodes with state
kiseki-admin node drain <node-id>         # initiate drain
kiseki-admin node drain-cancel <node-id>  # cancel in-progress drain
kiseki-admin node status <node-id>        # detailed drain progress
```

## Consequences

### Positive
- Safe, automated node retirement with RF preservation at all states
- Operator has cancellation lever (I-N7)
- Crash-safe: progress persisted in control plane Raft
- Concurrency bounded to prevent Raft instability
- Full audit trail for compliance

### Negative
- Drain requires at least one extra node in the cluster (cannot drain
  a 3-node cluster without adding a 4th first)
- Drain can be slow for nodes with many shards (bounded by migration
  concurrency cap)

### Neutral
- No change to Raft consensus protocol
- No change to data path
- Evicted state is terminal — consistent with "cattle not pets" for
  storage nodes
