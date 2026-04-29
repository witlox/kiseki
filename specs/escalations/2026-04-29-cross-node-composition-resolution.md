# Escalation: Cross-node composition resolution missing

**Type**: Architecture Conflict
**From**: implementer
**To**: architect
**Date**: 2026-04-29
**Phase**: 16a follow-up

## What I need

A Raft-replicated (or stream-processor-hydrated) `CompositionStore` so a
PUT on node-1 produces a composition that node-2 and node-3 can resolve
on subsequent GET / NFS lookup operations.

## What's blocking

Two of the four scenarios in `tests/e2e/test_cross_node_replication.py` —
`test_cross_node_read_after_leader_put` and
`test_read_survives_single_node_failure` — fail with `404 NoSuchKey` on
the cross-node GET. Investigation:

- `kiseki_chunk_cluster::ClusteredChunkStore` correctly fans out chunk
  bytes via fabric (validated by `kiseki_fabric_ops_total > 0` and the
  `test_fabric_metrics_present_after_cross_node_write` scenario passing)
- After the chunk is replicated, node-2's chunk store does have the
  envelope. Node-2's `read_chunk` would return it.
- But the S3 GET path on node-2 first goes through
  `mem_gateway.rs:407` →
  `compositions.lock().get(req.composition_id)` — this is a local
  in-memory `HashMap`. `CompositionStore::get` has no log fallback and
  the `compositions` map is not Raft-replicated. So node-2 returns
  `CompositionNotFound` long before it would touch the chunk store.
- The Raft log carries an `AppendDelta` with `payload = comp_id` (16
  bytes UUID) + `chunk_refs` + `hashed_key` + `tenant_id`, but **not**
  `bytes_written` (the user-visible size on `Composition`).
- The view stream-processor (`kiseki_view::TrackedStreamProcessor`) is
  spawned on every node and consumes the same delta log to update the
  view store, but there is no equivalent for the composition store.

## Two ways forward

**A) Hydrate composition store from delta log on each node.** Spawn a
sibling of `TrackedStreamProcessor` that reads `AppendDelta` /
`AppendChunkAndDelta` records and calls `CompositionStore::create_at`
on each follower with the delta's `(tenant_id, namespace_id, comp_id,
chunk_refs, size)`. Required prerequisites:
1. Extend the delta payload to carry `size` (or a side-channel index of
   size keyed by comp_id). Currently `payload = comp_id` only.
2. Add `CompositionStore::create_at(comp_id, namespace_id, chunks, size)`
   so followers can install a composition at the leader-assigned ID
   without the local UUID generator.
3. Spawn the hydration loop in `runtime.rs` next to the existing view
   stream processor.

**B) Replicate `CompositionStore` directly via Raft.** Make composition
state a state-machine type alongside the existing `cluster_chunk_state`.
Heavier change but symmetric with how chunks are tracked. Single source
of truth, no hydration race.

Option A is closer to the current architecture (the view path is the
template). Option B is closer to ADR-026's "Raft replicates delta
metadata" framing where compositions are derived from deltas.

## Impact

`@cross_node` BDD scenarios in `phase16-cross-node-chunks.feature`
(future) and the two failing e2e tests are blocked. Single-node and
chunk-fan-out paths are unaffected.

## Can I continue?

Yes — for the Phase-16 release I have applied the chunk-path fix
(disable inline write path in multi-node clusters so all writes go
through `ClusteredChunkStore`). The two composition-blocked tests are
marked `@pytest.mark.xfail` with a reference to this file. They flip
from xfail to pass automatically once option A or B lands.
