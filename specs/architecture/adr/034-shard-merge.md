# ADR-034: Shard Merge Mechanism

**Status**: Accepted
**Date**: 2026-04-25
**Deciders**: Architect + domain expert
**Adversarial review**: 2026-04-25 (10 findings across ADR-033/034/035: 3H 6M 1L, all resolved)
**Context**: ADR-033 (persistent namespace shard map), ADR-026 (Raft topology),
I-L13, I-L14, F-O6 (merge/split race)

## Problem

After splits (I-L6, I-L11) increase the shard count, sustained
under-utilization wastes Raft-group resources. Each shard consumes:
- One Raft group (3 voters × heartbeat + election overhead)
- Memory for the state machine and log buffer
- An entry in the namespace shard map

Without merge, shard count is monotonically increasing. A namespace
that had a traffic spike (triggering splits) and then returned to
baseline carries dead shards indefinitely.

## Decision

### 1. Merge eligibility (I-L13)

Two shards are merge-eligible when ALL of the following hold:

1. **Adjacent**: their `hashed_key` ranges are contiguous
   (`shard_a.range_end == shard_b.range_start`)
2. **Sustained underutilization**: combined utilization on every
   dimension (delta count, byte size, write throughput) has been below
   the merge threshold for the merge interval
3. **Ratio floor safe**: the post-merge shard count still satisfies
   I-L11 (`(shard_count - 1) / active_node_count >= ratio_floor`)
4. **No in-flight operations**: neither shard is currently Splitting
   or Merging (F-O6 ordering rule)

| Parameter | Default | Configurable by |
|-----------|---------|-----------------|
| `merge_threshold_pct` | 25% of split ceiling per dimension | Cluster admin (cluster-wide); tenant admin (per-namespace, within admin envelope) |
| `merge_interval` | 24 h | Cluster admin only |

### 2. Merge protocol: copy-then-cutover

**Decision**: copy-then-cutover (not leader-coordinated live cutover).

Rationale: copy-then-cutover is simpler and does not require
cross-shard coordination during the data path. The cost is temporary
doubled storage for the merged range during the copy phase, which is
acceptable because merge targets are by definition under-utilized
(combined < 25% of capacity).

#### Steps

```
Phase 1: Prepare
  1. Mark both input shards as state=Merging in the namespace shard map
     (atomic update through control plane Raft)
  2. Choose the merge target: the shard whose leader is on the
     least-loaded node (I-L12); if equal, lower ShardId wins
  3. Create a new shard "merged" with range [a.start, b.end)
     via control plane Raft, state=Merging

Phase 2: Copy
  4. Read all committed deltas from both input shards
  5. Write them to the merged shard in hashed_key order
     (interleave by hashed_key; within same hashed_key, preserve
     per-shard sequence order — this satisfies I-L14)
  6. Record a high-water-mark (HWM) HLC timestamp at copy start
  7. After initial copy, replay any deltas committed to input shards
     since HWM (tail-chase until gap < configurable threshold,
     default 100 deltas or 1 second)

Phase 3: Cutover
  8. Briefly pause writes to both input shards (< 50ms budget):
     - Set input shards to read-only via maintenance mode (I-O6)
     - Copy remaining deltas (the tail from step 7)
     - Update namespace shard map atomically:
       remove input entries, add merged entry with state=Healthy
     - Resume writes (now routed to merged shard)
  9. Emit ShardMerged event with input shard IDs, merged shard ID,
     merged range, and merge HLC timestamp
  10. Retire input shards (mark for Raft group teardown after a
      grace period — default 5 minutes — to drain in-flight reads)

Phase 4: Cleanup
  11. After grace period: tear down input Raft groups
  12. Delete input shard data from redb
```

#### Write availability during merge

- During Phase 2 (copy): writes continue to input shards normally
  (consistent with I-O1 / A-O1)
- During Phase 3 (cutover pause): writes are rejected with retriable
  error for < 50ms. This is the only unavailability window.
- After Phase 3: writes go to the merged shard

The cutover pause is bounded. If the tail-chase in step 7 keeps the
gap small (< 100 deltas), the pause in step 8 copies at most ~100
deltas, which at Raft consensus latency of ~250µs/delta takes ~25ms.

#### Convergence timeout and abort (ADV-034-2)

The tail-chase in step 7 has a convergence timeout (default 60
seconds). If the gap does not close below the threshold within the
timeout (e.g., because write rate exceeds copy rate), the merge is
aborted:

1. Tear down the in-progress merged shard
2. Mark input shards as state=Healthy (remove Merging flag)
3. Record `MergeAborted` event with reason `convergence_timeout`
4. The merge candidate scanner will re-evaluate eligibility on its
   next scan (5 minutes later); if write rate has subsided, the merge
   may be re-attempted

Similarly, the cutover pause in step 8 has a hard budget of 50ms.
If the remaining tail after entering read-only exceeds 200 deltas,
the cutover is aborted: read-write is restored on input shards, the
merged shard is torn down, and a `MergeAborted` event is recorded.
This prevents unbounded write unavailability.

### 3. Total order preservation (I-L14)

The merged shard's delta sequence is a consistent extension of both
inputs. This is achieved by:

1. Deltas are ordered by `hashed_key` in the merged shard
2. Within the same `hashed_key`, the original per-shard sequence
   order is preserved (deltas from shard A before deltas from shard B
   if A's sequence was committed first by HLC)
3. **HLC tie-break (ADV-034-6)**: if two deltas from different input
   shards have the same `hashed_key` AND the same HLC value (possible
   for truly concurrent writes across independent Raft groups), the
   delta from the lower `ShardId` is ordered first. This produces a
   deterministic total order that is arbitrary for concurrent writes
   but consistent and reproducible — which is all I-L14 requires.
4. The `ShardMerged` event records the merge HLC timestamp so
   consumers can distinguish pre-merge from post-merge sequences

Readers that were consuming from the input shards must switch to the
merged shard after the merge event. The stream processor detects
`ShardMerged` events via the control plane's event stream and
re-subscribes to the merged shard.

### 4. Candidate scanner

A periodic background task in `kiseki-control` (default every 5 minutes):

1. For each namespace, sort shards by range_start
2. For each pair of adjacent shards, check merge eligibility (§1)
3. If eligible, enqueue a merge operation
4. At most one merge per namespace at a time (serialized to avoid
   cascading merges)

### 5. Merge/split race resolution (F-O6)

Shard state machine:

```
Healthy → Splitting (on split trigger)
Healthy → Merging   (on merge trigger)
Splitting → Healthy (on split complete)
Merging → Healthy   (on merge complete, for the merged shard)
Merging → Retiring  (on merge complete, for the input shards)
Retiring → removed  (after grace period)
```

A shard in `Splitting` or `Merging` rejects the other operation:
- Split request on a `Merging` shard → `ShardBusy: merge in progress`
- Merge request on a `Splitting` shard → `ShardBusy: split in progress`
- Merge request on a `Merging` shard → `ShardBusy: merge in progress`

The losing operation is re-evaluated after the winning operation
completes (the resulting topology may make it unnecessary).

### 6. Code changes required

| File | Current state | Required change |
|------|--------------|-----------------|
| `kiseki-log/src/auto_split.rs` | Split only; no merge | Add merge orchestrator (copy-then-cutover) |
| `kiseki-log/src/shard.rs` | `ShardState: Healthy, Splitting` | Add `Merging`, `Retiring` states |
| `kiseki-control/src/namespace.rs` | No merge path | Add merge candidate scanner, merge enqueue |
| `kiseki-gateway/src/mem_gateway.rs` | No shard map invalidation | Handle `ShardMerged` event, refresh routing cache |
| `kiseki-view/src/stream_processor.rs` | Subscribes to single shard | Handle `ShardMerged` event, re-subscribe to merged shard |
| `specs/architecture/api-contracts.md` | No merge operations | Add `MergeShard`, `ShardMerged` event |

## Consequences

### Positive
- Shard count is no longer monotonically increasing
- Reclaims Raft-group overhead from under-utilized shards
- Copy-then-cutover is simple: no cross-shard consensus needed
- Bounded write unavailability (< 50ms)

### Negative
- Temporary 2× storage during copy phase (acceptable: merged shards
  are by definition under-utilized)
- 50ms write pause during cutover (retriable errors)
- Adds `Merging`/`Retiring` states to shard lifecycle

### Neutral
- No change to the Raft consensus protocol
- No change to chunk storage or encryption
- Merge interval (24h) prevents thrash from transient dips (A-N6)
