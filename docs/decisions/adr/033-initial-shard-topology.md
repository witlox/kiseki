# ADR-033: Initial Shard Topology, Ratio-Floor Splits, and Persistent Namespace Shard Map

**Status**: Accepted
**Date**: 2026-04-25
**Deciders**: Architect + domain expert
**Adversarial review**: 2026-04-25 (10 findings across ADR-033/034/035: 3H 6M 1L, all resolved)
**Context**: ADR-026 (Raft topology), ADR-030 (small-file placement), I-L10, I-L11, I-L12, I-L15, A-N1..A-N5

## Problem

Kiseki's namespace-to-shard mapping is hardcoded: one shard per namespace,
stored in-process memory only (`NamespaceStore` is `RwLock<HashMap>`), and
the gateway routes all writes to `ShardId::from_u128(1)`. This creates:

1. **Single-leader bottleneck**: all writes for a namespace serialize
   through one Raft leader on one node.
2. **No data distribution**: multi-node clusters cannot use all nodes
   for a namespace's write traffic.
3. **No persistence**: shard map is lost on process restart; no source
   of truth for routing.
4. **Dead code**: `auto_split::execute_split` exists but is never
   called, and the split plan assigns the new shard's leader to the
   old leader (`info.leader.unwrap_or(NodeId(1))`).

### Scale target

At cluster sizes of 3–100 nodes, a namespace should use multiple shards
from creation so writes distribute across nodes from day one, not only
after reactive splits fire.

## Decision

### 1. Initial shard topology (I-L10)

When a namespace is created, compute:

```
initial_shards = max(min(multiplier × node_count, shard_cap), shard_floor)
```

| Parameter | Default | Configurable by |
|-----------|---------|-----------------|
| `multiplier` | 3 | Cluster admin (cluster-wide); tenant admin (per-namespace, within admin envelope) |
| `shard_cap` | 64 | Cluster admin only |
| `shard_floor` | 3 | Cluster admin only |
| Per-tenant min/max envelope | none | Cluster admin sets per-tenant bounds; tenant admin overrides within them |

The control plane creates `initial_shards` Raft groups with disjoint
`hashed_key` ranges covering the full 256-bit key space. Ranges are
computed by uniform subdivision:

```
range_size = (2^256 - 1) / initial_shards   (integer division)
shard[i].range = [i * range_size, (i+1) * range_size)
shard[last].range_end = [0xFF; 32]           (absorb remainder)
```

#### Atomic namespace creation (ADV-033-1)

Creating N Raft groups is N distributed operations that can partially
fail. Namespace creation uses a two-phase protocol:

```
Phase 1: Create Raft groups
  1. Mark namespace as state=Creating in the control plane Raft group
  2. Create all N Raft groups in parallel, wait for each to reach quorum
  3. If any group fails to form within timeout (default 30s):
     - Tear down all successfully created groups
     - Remove the Creating namespace entry
     - Return error: "namespace creation failed: shard N did not reach quorum"
  4. Concurrent CreateNamespace for the same namespace is rejected
     while state=Creating

Phase 2: Commit shard map
  5. Write all N ShardRange entries to the namespace shard map atomically
  6. Transition namespace state: Creating → Active
  7. Namespace is now routable
```

The `Creating` state prevents partial-key-range coverage. No writes
are routed to the namespace until all shards are healthy and the map
is committed.

### 2. Leader placement policy (I-L12)

At shard creation, split, and merge, the initial leader is placed on
the node currently hosting the fewest leaders *for that namespace*, with
deterministic tie-break on lowest `NodeId`.

**No post-hoc balance invariant.** Drift between placement events is
permitted. Operators may trigger explicit rebalance via a future
`RebalanceNamespace` RPC (out of scope for this ADR).

**Placement eligibility (ADV-033-8)**: the I-L12 placement engine
considers only nodes in `{Active, Degraded}` state. `Degraded` nodes
are eligible because they are still serving traffic. `Failed`,
`Draining`, and `Evicted` nodes are excluded. `Degraded` nodes are
also eligible as drain replacement targets (ADV-035-10).

### 3. Ratio-floor auto-split trigger (I-L11)

A background evaluator in `kiseki-control` monitors the shards-per-node
ratio for every namespace on every topology change event:

- Node added (Active count increases)
- Node drained/evicted (Active count decreases — may reduce shards,
  but ratio improves because denominator shrinks)
- Namespace created (new namespace with fresh shards)

**Trigger condition:** for any namespace where
`shard_count / active_node_count < ratio_floor` (default 1.5),
auto-split fires for the largest shard(s) in that namespace until
`shard_count >= ceil(ratio_floor × active_node_count)`.

The ratio-floor trigger reuses the same split mechanism as the I-L6
per-shard ceiling trigger. Both produce a `SplitPlan` that:
1. Computes the midpoint of the shard's key range
2. Creates a new Raft group for the upper half
3. Redistributes deltas by key range
4. Updates the namespace shard map atomically

**Ordering with I-L6**: either trigger independently suffices. If both
fire simultaneously for the same shard, only one split executes (the
shard is locked during split); the other re-evaluates after completion.

**Rate limiting (ADV-033-7)**: ratio-floor splits are bounded to
`max(1, active_node_count / 5)` concurrent splits cluster-wide.
Remaining splits are queued. This prevents bulk node-add events
(e.g., scaling from 3 to 50 nodes) from overwhelming the control
plane Raft group with shard map updates. The `shard_cap` takes
precedence over the ratio formula: if `ceil(ratio_floor × node_count)
> shard_cap`, the namespace stops at `shard_cap` shards and the
sub-floor ratio is accepted as a known limitation until the cap is
raised.

### 4. Persistent namespace shard map (I-L15)

The namespace-shard mapping MUST be stored in the control plane's Raft
group — never in process memory only.

#### Data model

```
NamespaceShardMap {
    namespace_id: String,
    tenant_id: OrgId,
    version: u64,                    // monotonically increasing on every mutation
    shards: Vec<ShardRange>,
    created_at: HLC,
    updated_at: HLC,
}

ShardRange {
    shard_id: ShardId,
    range_start: [u8; 32],
    range_end: [u8; 32],
    leader_node: NodeId,             // best-effort; may be stale
    state: ShardState,               // Healthy | Splitting | Merging | Retiring
}
```

#### Mutations

All mutations go through the control plane Raft group (single writer):

| Operation | Trigger | Atomicity |
|-----------|---------|-----------|
| `CreateNamespace` | Tenant admin | Creates N `ShardRange` entries + Raft groups |
| `SplitShard` | I-L6 ceiling or I-L11 ratio floor | Replaces 1 entry with 2 entries (original shrinks, new entry added) |
| `MergeShard` | I-L13 utilization (ADR-034) | Replaces 2 adjacent entries with 1 entry |
| `UpdateLeader` | Election, drain, rebalance | Updates `leader_node` on one entry |

All mutations increment `version`. Consumers (gateway, native client)
cache the map and refresh on version mismatch or cache-miss.

#### Distribution: pull-on-cache-miss

Gateways and native clients cache the `NamespaceShardMap` with a version
number. On a routing decision:

1. Hash the composition key → `hashed_key`
2. Look up `hashed_key` in the cached shard ranges
3. If cache miss (key not in any range) or write returns `KeyOutOfRange`:
   fetch the latest map from the control plane via `GetNamespaceShardMap` RPC
4. Update local cache

**Why pull, not push/watch:** pull-on-cache-miss is the simplest correct
mechanism. It adds one RPC on cold start and on topology changes (splits,
merges). A gRPC watch stream is a viable optimization for high-churn
workloads but adds complexity (stream lifecycle, reconnection, ordering
guarantees) without improving correctness. If monitoring shows excessive
cache-miss RPCs (>1/s sustained per gateway), add a watch stream as a
follow-on ADR.

**Authorization (ADV-033-9)**: `GetNamespaceShardMap` validates the
caller's mTLS identity against the tenant owning the namespace.
Gateways and clients serving tenant A cannot query shard maps for
tenant B. This prevents cross-tenant topology information leakage
(shard count, leader nodes, key ranges). Consistent with I-T1
(full tenant isolation).

### 5. Gateway routing

The gateway replaces the hardcoded `ShardId::from_u128(1)` with a
routing function:

```
fn route_to_shard(ns_map: &NamespaceShardMap, hashed_key: &[u8; 32]) -> ShardId
```

This is a binary search over the sorted `ShardRange` list. O(log N)
where N ≤ 64.

### 5a. Shard-side range validation (ADV-033-3)

`AppendDelta` MUST reject deltas whose `hashed_key` is outside the
shard's `[range_start, range_end)` with error `KeyOutOfRange`. This
is the enforcement point that makes pull-on-cache-miss safe: a stale
gateway cache sends a delta to the wrong shard → the shard rejects
it → the gateway refreshes its shard map → retries to the correct
shard. Without this validation, stale caches silently misplace data.

```
fn validate_key_range(shard: &ShardInfo, hashed_key: &[u8; 32]) -> Result<(), LogError> {
    if hashed_key < &shard.range_start || hashed_key >= &shard.range_end {
        Err(LogError::KeyOutOfRange { shard_id: shard.shard_id })
    } else {
        Ok(())
    }
}
```

This check is added to `AppendDelta` in `kiseki-log` (all store
implementations: `MemShardStore`, `PersistentShardStore`,
`RaftShardStore`).

### 6. Shard key hashing

Composition keys are hashed to `[u8; 32]` using SHA-256 of the
composition's `(namespace_id, key)` tuple. This is the same
`hashed_key` already present in `DeltaHeader`. No new hashing is
introduced — the existing `hashed_key` is used for range routing.

### 7. Code changes required

| File | Current state | Required change |
|------|--------------|-----------------|
| `kiseki-control/src/namespace.rs:34` | In-memory `RwLock<HashMap>` | Replace with Raft-backed `NamespaceShardMapStore` |
| `kiseki-control/src/namespace.rs:64-67` | Single shard creation | Create `initial_shards` Raft groups with uniform key ranges |
| `kiseki-log/src/auto_split.rs:107` | `initial_node: leader` (inherits from old shard) | Use I-L12 placement: fewest leaders for namespace, tie-break on `NodeId` |
| `kiseki-gateway/src/mem_gateway.rs:221` | Hardcoded `ShardId::from_u128(1)` | Route via `NamespaceShardMap` lookup |
| `kiseki-client/src/discovery.rs:18-35` | `ShardEndpoint` defined but unused | Wire `ShardEndpoint` into discovery response; populate from `NamespaceShardMap` |
| `kiseki-control` | No RPC for shard map | Add `GetNamespaceShardMap` RPC to `ControlService` |

### 8. Multi-Raft heartbeat batching and the 64-shard cap

The `shard_cap` of 64 exists because `kiseki-raft` does not batch
heartbeats (each Raft group sends its own heartbeats independently).
Per ADR-026, at 30 groups/node the heartbeat traffic is negligible
(78 KB/s on a 200 Gbps fabric). At 64 groups on a 3-node cluster
(all groups on all nodes), heartbeat overhead is still <0.001% of
fabric bandwidth.

**Decision:** keep the cap at 64 for now. When Multi-Raft heartbeat
batching (ADR-026 Strategy C) lands, the cap can be raised via
`shard_cap` configuration. No spec or invariant change needed — the
cap is a tuning parameter, not a correctness boundary.

## Consequences

### Positive
- Day-one write distribution across all cluster nodes
- Persistent routing: survives process restart
- Gateway routing is O(log N), N ≤ 64
- Compatible with existing `hashed_key` in `DeltaHeader`
- Pull-on-cache-miss is simple and correct

### Negative
- Initial namespace creation is heavier: N Raft groups instead of 1
- Control plane Raft group is on the write path for topology changes
  (not on data path — topology changes are rare)
- 64-shard cap limits maximum parallelism per namespace until
  heartbeat batching lands

### Neutral
- No change to data path performance (routing is local cache lookup)
- No change to Raft consensus protocol
- No change to chunk storage or encryption
