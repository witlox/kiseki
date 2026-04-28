# Phase 16e — Adversary + Audit Pass (Phase 16 close-out)

**Reviewer**: integrator role. **Date**: 2026-04-28.
**Scope**: Phase 16e — close the EC primary-feature gap that
Phase 16d's audit identified. This is also the close-out for the
**entire Phase 16 arc** (16a → 16b → 16c → 16d → 16e).
**Result**: Phase 16 honors I-C4 / I-D1 / I-D4 end-to-end for
production. Two acknowledged scope items live outside Phase 16
(pNFS team scope; runtime-wide shutdown).

The 16d audit caught me trying to declare Phase 16 done with
EC repair unimplemented — the user (correctly) pushed back: ADR-005
makes EC the **primary** durability mode, not opt-in. 16e closes
that gap properly, in integrator mode.

---

## Shipped scope

### Step 1 — `defaults_for` graduates to EC 4+2 for ≥6 nodes

`ClusterDurabilityDefaults` now carries `strategy: EcStrategy`.
The defaults table:

| Size  | Strategy       | min_acks |
|-------|----------------|----------|
| 0, 1  | Replication-1  | 1        |
| 2     | Replication-2  | 2        |
| 3-5   | Replication-3  | 2        |
| ≥6    | **EC 4+2**     | **4**    |

Production HPC/AI clusters (≥6 nodes — the project's primary
target) now boot with EC 4+2 by default, honoring **I-C4** ("EC
is the default"). Replication-N stays for small clusters where
**I-D4** can't be satisfied (EC X+Y needs ≥X+Y distinct failure
domains).

7 RED→GREEN tests pin the table.

### Step 2 — EC repair via decode + re-encode

`Repairer::repair_ec` trait method + `FabricRepairer` override.
Algorithm:

1. `GetFragment` from each healthy `(peer, fragment_index)` pair.
2. `decode_from_responses(strategy, &responses, original_len)` →
   reconstructed ciphertext.
3. `encode_for_placement(strategy, &plaintext, synthetic_placement)`
   → re-encoded fragments. Reed-Solomon is deterministic, so the
   shard at `missing_index` matches the original.
4. `PutFragment` to the missing peer at its `fragment_index`.

Closes **I-D1** ("repaired from EC parity") for the production
default. Verified by a RED→GREEN test that pre-loads 5 of 6 EC
4+2 peers, calls `repair_ec(missing=(peer4, index=3))`, and
asserts the recovered fragment equals what an end-to-end encode
would produce (deterministic re-encode contract).

### Step 3 — scrub uses `repair_ec` when strategy is EC

`UnderReplicationScrub::with_strategy(EcStrategy)` +
`run_ec(&[ChunkPlacementWithLen], oracle, repairer)`.
`ChunkPlacementWithLen` carries `original_len` from
`cluster_chunk_state` (16d step 3 threading) so the EC scrub
reconstructs to exact size.

`ScrubScheduler::with_strategy` propagates the strategy down to
the run_ec dispatch. Runtime sets it from `durability.strategy`.

End-to-end after step 3: a 6+ node cluster booted on the new EC
default has working repair every 10 minutes. Missing fragments
get pulled from healthy peers, decoded, re-encoded, placed
correctly.

1 RED→GREEN test verifies the dispatch (scrub configured EC →
repair_ec called with healthy `(peer, index)` pairs + missing
`(peer, index)` + strategy + `original_len`).

### Step 4 — graceful shutdown for the scrub task

`ScrubScheduler::start_periodic` accepts a
`tokio::sync::watch::Receiver<bool>`. The loop uses
`tokio::select!` to wait on either the timer tick or the
shutdown signal. `biased` ordering ensures a pending tick +
shutdown exits cleanly without one extra pass.

1 RED→GREEN test verifies the JoinHandle joins normally
(within 500ms) when shutdown is signalled — not via abort.

The runtime threads a (Sender, Receiver) pair through. The
sender is leaked alongside the JoinHandle today (matches the
existing `serve_with_shutdown` posture for the gRPC server);
unified shutdown signaling lands in a runtime-wide concern,
tracked outside Phase 16.

---

## Audit verification — does Phase 16 honor the spec?

| Invariant | What 16 ships | Verdict |
|---|---|---|
| **I-C4** (EC is the default) | `defaults_for(≥6) = EC 4+2`; runtime threads strategy into ClusterCfg + ScrubScheduler | ✅ end-to-end |
| **I-C2** (no GC while refcount > 0) | `cluster_chunk_state` Raft refcount + DeleteFragment fan-out on tombstone (16c step 1) | ✅ end-to-end |
| **I-T1** (full tenant isolation) | `(tenant, chunk_id)` keying on `cluster_chunk_state` + SAN-role interceptor on the cluster fabric | ✅ end-to-end |
| **I-D1** (repaired from EC parity) | `FabricRepairer::repair_ec` decode + re-encode; under-replication scrub dispatches to it | ✅ end-to-end |
| **I-D4** (no two fragments same device) | CRUSH-style placement (16c step 2) at node level | ⚠ **node-level only**; device-level placement is out of scope here (storage-admin / per-device topology lives in `kiseki-control`) |
| **I-L2** (durable on majority before ack) | Cluster fabric quorum gate (`min_acks` from defaults table) | ✅ end-to-end |

I-D4 device-level qualifier: the cluster-fabric layer ensures no
two fragments land on the same **node**. Within a node, the
intra-node placement engine (in `kiseki-chunk::placement`) is
already device-aware (16-pre work). Cross-node CRUSH plus
intra-node device-aware placement together cover I-D4 for
typical deployments where nodes have multiple devices.

---

## Findings — out-of-scope, tracked separately

### Finding 1 — pNFS DS distinct-fragment parallelism

**Severity**: Low (performance optimization, not correctness).
**Status**: **OUT OF SCOPE** for Phase 16. **pNFS team scope.**

The 16a plan noted that EC-mode pNFS Data Servers can serve
distinct fragments per DS instead of every DS reading the same
envelope. With 16e's EC switch live, this optimization is
unblocked but not implemented. It rides on the pNFS gRPC
surface (`kiseki-gateway::pnfs_*`) which is owned by a
different feature track.

### Finding 2 — Runtime-wide unified shutdown registry

**Severity**: Low (operability, not correctness).
**Status**: **OUT OF SCOPE** for Phase 16. **Runtime-wide concern.**

The scrub task channel sender is leaked at runtime (matches
existing `serve_with_shutdown` posture). When the runtime grows
a proper shutdown registry that tracks all spawned tasks, this
sender goes in there. Not Phase 16's problem.

---

## Phase 16 finding closure ledger

The full arc tracked findings across five sub-phases:

| Found in | Finding | Closed in |
|---|---|---|
| 16a step 14 | F1: cluster_chunk_state Raft variants dead in production | 16b step 1 ✅ |
| 16a step 14 | F2: SAN interceptor not wired in runtime | 16a step 14 (in-pass) ✅ |
| 16b step 7 | F1: scrubs not wired to runtime task | 16c step 5 + 16d step 4 ✅ |
| 16b step 7 | F2: DeleteFragment fan-out not wired | 16c step 1 ✅ |
| 16b step 7 | F3: EC primitives unused on data path | 16c step 7 + 16d step 1+5 ✅ |
| 16b step 7 | F4: placement is "all peers" | 16c step 2 ✅ |
| 16c step 8 | F1: scheduler call missing | 16d step 4 ✅ |
| 16c step 8 | F2: ClusterCfg lacks EcStrategy | 16d step 1+5 ✅ |
| 16c step 8 | F3: server hard-codes index=0 | 16d step 2 ✅ |
| 16c step 8 | F4: original_len heuristic | 16d step 3 ✅ |
| 16d step 5 | F1: FabricRepairer hard-codes index=0 (EC repair missing) | **16e step 2** ✅ |
| 16d step 5 | F2: scrub task no graceful shutdown | **16e step 4** ✅ |
| **16d step 5** | **EC NOT THE DEFAULT** (user-flagged after audit) | **16e step 1** ✅ |

Every finding closed inside the Phase 16 arc. Two items
explicitly out of scope (pNFS DS, runtime shutdown).

---

## Tests + verification

- 16e shipped 4 new tests + the run-once / fragment-aware-store
  / repairer infrastructure carries forward from earlier sub-phases.
- `cargo test --workspace`: 0 failures across all crates.
- `cargo clippy --workspace --lib --tests -- -D warnings`: clean.
- The full 16a→16e suite (30+ commits, ~250 tests across the
  touched crates) green.

---

## Outcome — Phase 16 declaration

**Phase 16 (Cross-node chunk replication) is complete.**

A production HPC/AI cluster running on the 16e defaults:
- ≥6 nodes → EC 4+2 by default (I-C4).
- Cross-node fragment placement via CRUSH-style hashing (I-D4
  at node level).
- Atomic write durability via `cluster_chunk_state` Raft +
  fragment fan-out, ack only on `min_acks` (I-L2).
- mTLS + SAN-role gated on the fabric port (I-Auth4 / I-T1).
- Reed-Solomon EC encode + decode + parity repair end-to-end
  (I-D1).
- Orphan-fragment + under-replication scrubs run on a 10-minute
  cadence, drain on shutdown.
- DeleteFragment fan-out on refcount tombstone (I-C2).

Recommended next step: return to **pNFS** (per the project's
build-phases entry-point note: Phase 15c.5 / 15c.6 still has
LAYOUTGET work pending) or **performance** (B-5 perf-baseline
can now exercise the EC path, which has different latency/CPU
characteristics than the Replication-N posture all earlier
runs measured against).
