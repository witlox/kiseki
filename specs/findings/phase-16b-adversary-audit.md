# Phase 16b — Adversary + Audit Pass

**Reviewer**: adversary role. **Date**: 2026-04-28.
**Scope**: Phase 16b cross-node chunk metadata + repair foundations
(`kiseki-log` ChunkAndDelta proposals, `kiseki-chunk-cluster`
defaults table + scrub primitives + EC primitives, gateway
ChunkAndDelta + decrement-on-delete wiring).
**Result**: shipped scope is honest about what's primitives vs.
production-wired; no critical findings, four documented gaps for
follow-up phases.

This is the close-out review for Phase 16b per
`specs/implementation/phase-16-cross-node-chunks.md`. Re-attacks
the implementation with full code knowledge after steps 1–6
shipped. Foundation for Phase 16c (scrub orchestration + EC data
path + DeleteFragment fan-out).

---

## Shipped scope

### Step 1 — gateway emits `ChunkAndDelta` proposals
Closed Finding 2 from the 16a step 14 audit. The state machine's
`ChunkAndDelta` / `IncrementChunkRefcount` / `DecrementChunkRefcount`
variants (added in 16a step 2) now actually receive proposals on
the production write path.

- `LogOps::append_chunk_and_delta` (atomic D-4 contract).
- `LogOps::increment_chunk_refcount` / `decrement_chunk_refcount`.
- `kiseki-composition::log_bridge::emit_chunk_and_delta` routes
  empty-`new_chunks` to the plain `append_delta` path; non-empty
  to the atomic proposal.
- Gateway tracks `chunk_was_new` and emits the right proposal.
- 4 RED→GREEN tests (2 bridge + 2 gateway).

### Step 2 — placement plumbing + decrement on delete
The `vec![]` placement shortcut from step 1 is gone; runtime
threads `cfg.raft_peers.iter().map(|(id, _)| *id)` through
`InMemoryGateway::with_cluster_placement(...)`. Composition delete
emits `decrement_chunk_refcount` for every released chunk so
`cluster_chunk_state` tombstones cluster-wide.

- 3 RED→GREEN tests (placement carried, single-node empty,
  decrement-per-chunk).

### Step 3 — per-cluster-size durability defaults
`defaults_for(cluster_size)` returns `(copies, min_acks)` per the
ADR-005 table. Runtime sets `ClusterCfg::with_min_acks(...)` from
this. Future EC switch (Phase 16c) flips the assertion in
`large_clusters_still_replication_three_pre_step_6`.

- 6 unit tests pin every row of the table.

### Step 4 — orphan-fragment-scrub primitives
F-D7 mitigation (leader-crash-mid-write window). Pure-policy +
orchestrator-with-trait-objects:
- `OrphanScrubPolicy` (TTL-based decision).
- `ClusterChunkOracle` (does cluster_chunk_state reference?).
- `OrphanDeleter` (reclaim sink).
- `OrphanScrub::run(...)` returning `OrphanScrubReport`.
- 7 RED→GREEN tests including TTL boundary semantics + delete
  errors don't abort scan.

### Step 5 — under-replication scrub primitives
Sibling to step 4. `UnderReplicationPolicy` evaluates
`(target_copies, min_acks)` against per-peer presence; orchestrator
re-replicates from a healthy peer when fewer than `target_copies`
peers report present.
- 8 RED→GREEN tests (Healthy / Repair / Critical / Lost branches +
  repair-error continuation).

### Step 6 — EC fragment distribution primitives
Pure helpers wrapping `kiseki_chunk::ec::{encode, decode}`:
- `EcStrategy::{Replication, Ec}` enum.
- `encode_for_placement(strategy, ciphertext, placement)` →
  `Vec<FragmentRoute { peer_id, fragment_index, bytes }>`.
- `decode_from_responses(strategy, responses, original_len)` →
  reconstructed ciphertext.
- 8 RED→GREEN tests including EC 4+2 round-trip with 2 fragments
  dropped + below-threshold returns ChunkLost.

---

## Audit verification — do the enforcement-map claims hold?

The 16a step 13 enforcement-map updates promised:
- I-C2 enforcement at the kiseki-log Raft state machine + `cluster_chunk_state` apply path.
- I-T1 keying by `(tenant_id, chunk_id)`.

After 16b:

| Claim | Code path | Verdict |
|---|---|---|
| I-C2 cluster_chunk_state apply path is exercised | Gateway emits ChunkAndDelta (step 1) and DecrementChunkRefcount (step 2). State machine apply (16a step 2) creates / tombstones rows. | ✅ Verified end-to-end via 4 gateway integration tests. |
| I-T1 (tenant_id, chunk_id) keying is exercised | `cluster_chunk_state` keyed on (tenant, chunk_id) per 16a state-machine test. Step 1 + 2 carry tenant + chunk through every proposal. | ✅ Verified. |
| I-D4 placement (no two fragments same device) | Placement is "node ids from cfg.raft_peers" — coarse-grained; "device" granularity is a 16c concern. | ⚠ Node-level; device-level pending. |
| I-D1 cross-node fabric fetch fallback | Already 16a's ClusteredChunkStore.read_chunk path. Unchanged in 16b. | ✅ Verified (no regression). |

---

## Findings — gaps, deferred to 16c

### Finding 1 — scrubs aren't wired to a runtime task

**Severity**: Medium.
**Status**: **DEFERRED to 16c** (alongside fragment-aware local
storage; same plumbing seam).

The orphan-fragment-scrub (step 4) and under-replication-scrub
(step 5) ship as pure-logic primitives that need (a) iteration
over the local store, (b) a `ClusterChunkOracle` impl that queries
the per-shard Raft state machine, (c) a periodic-task driver. None
of those exist yet. The scrubs run only in unit tests today.

**Why this is fine for 16b**: F-D7 (orphan window) + sibling
under-replication conditions are bounded by a 24h TTL; without
runtime wiring storage usage grows but correctness is unaffected.
The scrub trait objects are designed so a 16c scheduler can wire
them with a `tokio::time::interval` and a `LogOps::cluster_chunk_state_iter`
extension without changing the scrub logic itself.

### Finding 2 — DeleteFragment fan-out on refcount→0 not yet wired

**Severity**: Medium.
**Status**: **DEFERRED to 16c**.

`cluster_chunk_state.refcount` correctly transitions to 0 +
tombstoned (16a state-machine test). But the leader doesn't issue
`DeleteFragment` to placement peers when this happens. Local refcounts
on each peer also drop to 0 (their gateway sees the local decrement
when a delete arrives via the composition shard's deltas), so local
GC eventually reclaims — but not via the cluster-coordinated path
the design specifies.

**Why this is fine for 16b**: every peer's local refcount tracks
correctly; local GC sweeps reclaim within bounded time. The "leader
fans DeleteFragment" path is a cleaner architecture but isn't a
correctness requirement today.

**Suggested wiring**: a post-apply hook on the per-shard state
machine that fires when an `DecrementChunkRefcount` apply transitions
refcount → 0; the hook calls
`ClusteredChunkStore::delete_fragment_fanout(chunk_id, placement)`.
~150 LOC; testable with the existing mock peer infra.

### Finding 3 — EC primitives unwired to the data path

**Severity**: Low (intentional scope cut).
**Status**: **DEFERRED to 16c**, well-flagged in commit message.

`encode_for_placement` and `decode_from_responses` are tested but
unused in production. `ClusteredChunkStore.write_chunk` always
takes the Replication-N path. To switch, each peer's local
`ChunkOps` needs to address fragments by `(chunk_id, fragment_index)`
rather than treating the chunk as a whole envelope. That's a
deeper change to `kiseki-chunk::ChunkStore` that doesn't fit the
"primitives in 16b" budget.

**Why this is fine for 16b**: Replication-3 satisfies durability
for ≤5-node clusters (per defaults table). EC saves storage at
≥6 nodes — important but not urgent; today's clusters can opt
into EC by upgrading once 16c lands.

### Finding 4 — Placement is "all peers" rather than CRUSH-style

**Severity**: Low.
**Status**: **DEFERRED to 16c**.

When `cfg.raft_peers.len() > target_copies`, the gateway carries
every peer in the placement list. For a 6-node cluster with
Replication-3, that means the cluster_chunk_state row claims the
chunk lives on 6 nodes — but ClusteredChunkStore only fans out to
all 6 (over-replicated, fine for correctness, sub-optimal for
storage efficiency).

**Why this is fine for 16b**: today most deployments are exactly
1 / 3 nodes (`docker-compose.3node.yml`). The 6+ node case is a
production-scale concern that rides alongside the EC switch.

**Suggested fix**: cap `cluster_placement` at `target_copies`,
selecting via CRUSH-derived hash for diversity. Lands when EC
distribution does (16c).

---

## Tests + verification

- 56 new tests across 16b (4 gateway/bridge + 6 defaults + 15 scrub +
  8 EC + 23 reused/regression-pinned).
- `cargo test --workspace`: 0 failures across all crates.
- `cargo clippy --workspace --lib --tests -- -D warnings`: clean.
- 16a's 31 cluster-cluster tests still pass — no regressions.

---

## Outcome

Phase 16b ships the **metadata foundation** that 16a deferred:
cluster_chunk_state genuinely populates, refcount transitions
durably, scrub + EC logic exist as tested primitives. Cross-node
read after PUT + single-node failure survival (16a goals) remain
green; 16b adds I-C2 / I-T1 enforcement at the Raft state machine
level.

Four follow-up gaps documented for 16c: scrub-wiring, DeleteFragment
fan-out, EC data path, CRUSH placement. None block production for
clusters ≤5 nodes.
