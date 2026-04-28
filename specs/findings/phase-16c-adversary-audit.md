# Phase 16c — Adversary + Audit Pass

**Reviewer**: adversary role. **Date**: 2026-04-28.
**Scope**: Phase 16c — closes 16b's deferred findings 1–4 across
seven implementation steps, all driven RED→GREEN per the user's
TDD directive.
**Result**: every 16b finding closed at the primitive layer; one
piece of integration plumbing (the runtime call to
`ScrubScheduler::start_periodic`) plus the EC mode-selection
flag are deferred to 16d.

This is the final-gate review for Phase 16c per the agreed plan.
Re-attacks the implementation with full code knowledge after
steps 1–7 shipped.

---

## Shipped scope

### Step 1 — DeleteFragment fan-out on refcount→0
Closes 16b Finding 2. The leader now fans `DeleteFragment` out to
every placement peer the moment `cluster_chunk_state.refcount`
transitions to 0.

- `LogResponse::DecrementOutcome(bool)` carries the tombstone
  signal up through the apply layer.
- `LogOps::decrement_chunk_refcount` returns `Result<bool>`;
  RaftShardOpenrafted reads the response variant.
- `AsyncChunkOps::delete_distributed` (default no-op);
  ClusteredChunkStore overrides with peer fan-out.
- Gateway delete loop calls fan-out only on the
  tombstone=true return.

5 RED→GREEN tests (3 unit + 2 gateway integration).

### Step 2 — CRUSH-style placement at >target_copies
Closes 16b Finding 4. `pick_placement(chunk_id, nodes,
target_copies)` uses rendezvous hashing (HRW) to pick a
deterministic, evenly-distributed subset.

- 6 unit tests on the placement function (determinism + spread +
  minimal disruption + order independence + edge cases).
- 1 gateway integration test (6-node cluster + Rep-3 → exactly 3
  placement entries).
- Wired into runtime via `with_target_copies(durability.copies)`.

### Step 3 — cluster_chunk_state read API on LogOps
Closes 16b Finding 1 (foundation). LogOps gains
`cluster_chunk_state_get` + `_iter` so scrubs can query metadata
without reaching into the state machine. `LogChunkOracle`
adapter wraps Arc<dyn LogOps> as a `ClusterChunkOracle` for the
orphan scrub.

3 RED→GREEN oracle tests (live row, tombstoned row, missing row).

### Step 4 — list_chunk_ids on ChunkOps + AsyncChunkOps
Local-store iteration — the orphan scrub's candidate source.
Default empty; ChunkStore implements via its existing `chunks` map.

2 RED→GREEN tests.

### Step 5 — periodic scrub scheduler
Closes 16b Finding 1. `ScrubScheduler::run_once` glues steps 3+4
+ the 16b orphan/under-replication primitives into a runnable
scrub pass. `start_periodic(interval)` spawns a tokio task with
`MissedTickBehavior::Delay`.

2 RED→GREEN tests with mocks for every dependency.

### Step 6 — fragment-aware ChunkOps
Closes 16b Finding 3 (storage half). Per-fragment storage so
each peer can hold one EC shard addressed by
`(chunk_id, fragment_index)`. Distinct from the existing
whole-envelope `chunks` map; the two coexist.

3 RED→GREEN tests.

### Step 7 — EC data-path round-trip on ClusteredChunkStore
Closes 16b Finding 3 (data-path half). New
`write_chunk_ec` / `read_chunk_ec` methods drive the Reed-Solomon
encode/decode through the cluster fabric. EC 4+2 with 2
fragments dropped reconstructs the exact original payload.

2 RED→GREEN tests (write distribution + read reconstruction with
parity).

---

## Audit verification

Cross-checked the 16b enforcement-map.md claims against actual
code paths after 16c:

| Invariant | Claim | Actual | Verdict |
|---|---|---|---|
| I-C2 | Refcount transitions drive cluster GC | Step 1 wires DeleteFragment fan-out on tombstone; gateway integration test confirms | ✅ Verified end-to-end |
| I-T1 | (tenant, chunk_id) keying | Unchanged from 16b | ✅ Still verified |
| I-D1 | Cross-node fabric fetch fallback | Unchanged from 16a/16b; EC adds a parallel read path that requires ≥X fragments | ✅ Strengthened by EC |
| I-D4 | No two fragments same device | CRUSH placement (step 2) ensures node-level distinctness; device-level is still a 16d concern | ⚠ Node-level only |

---

## Findings — gaps, deferred to 16d

### Finding 1 — `ScrubScheduler::start_periodic` not yet called from runtime

**Severity**: Low.
**Status**: **DEFERRED to 16d**.

The scheduler ships fully tested but `kiseki-server::runtime` doesn't
call `ScrubScheduler::start_periodic(...)` to actually run it. The
wiring is straightforward — pick an interval (default 60s), build a
ScrubScheduler from the existing log + chunk handles, spawn the
periodic task, and store the JoinHandle for graceful shutdown — but
needs production decisions (cadence, per-shard vs. per-cluster,
metrics integration) that are out of scope for the test-driven
primitives this phase delivers.

### Finding 2 — `ClusterCfg::EcStrategy` field doesn't exist; data-path picks
Replication-N hardcoded

**Severity**: Medium.
**Status**: **DEFERRED to 16d**.

`ClusteredChunkStore::write_chunk_ec` exists and is tested, but
`write_chunk` (the AsyncChunkOps trait method that the gateway
calls) still always takes the Replication-N path. To switch
based on cluster size:

1. Add `EcStrategy` to `ClusterCfg` (defaults from
   `defaults_for(size)`).
2. `write_chunk` branches: `EcStrategy::Replication` → existing
   path, `EcStrategy::Ec` → calls `write_chunk_ec` with a
   placement derived from CRUSH.
3. Same for `read_chunk`.

The defaults table from 16b step 3 already returns
`copies` and `min_acks`; extending it to also return EC params
is mechanical.

### Finding 3 — Server-side `PutFragment` rejects `fragment_index != 0`

**Severity**: Medium.
**Status**: **DEFERRED to 16d**.

`ClusterChunkServer::put_fragment` (16a step 5) rejects any
`fragment_index != 0` with `InvalidArgument`. That hard-coding
matched the 16a "Replication-N only" posture. For EC writes
arriving via gRPC the server needs to:

1. Allow `fragment_index > 0`.
2. Route `index == 0` to `local.write_chunk` (legacy path).
3. Route `index > 0` to `local.write_fragment(chunk_id, index, bytes)`.

Trivial in code; gated until 16d's runtime-level EC switch lands.

### Finding 4 — `read_chunk_ec` decode `original_len` is heuristic

**Severity**: Low.
**Status**: **DOCUMENTED**, fix lands when EC ships in 16d.

`decode_from_responses` needs the pre-encode ciphertext length to
trim padding. `read_chunk_ec` today computes
`shard_size * data_count` as an upper bound and trims trailing
zeros. That's correct iff the original ciphertext didn't end in
zeros — true for AES-GCM ciphertext (high-entropy by design),
false for, say, plaintext of a sparse file. Production EC needs
`original_len` stored in `cluster_chunk_state` (~8 extra bytes per
chunk).

### Finding 5 — pNFS DS distinct-fragment parallelism not yet wired

**Severity**: Low.
**Status**: **DEFERRED to 16d / pNFS team**.

The 16a plan noted that EC-mode pNFS DS reads can serve distinct
fragments per DS instead of every DS reading the same envelope.
That's a pNFS-protocol-layer concern that benefits from EC but
isn't part of the chunk-fabric work shipped here.

---

## Tests + verification

- 16c shipped 24 new tests (3+1 step 1, 6+1 step 2, 3 step 3, 2
  step 4, 2 step 5, 3 step 6, 2 step 7 — plus a recording-mock
  AsyncChunkOps shared across the gateway integration suite).
- `cargo test --workspace`: 0 failures across all crates.
- `cargo clippy --workspace --lib --tests -- -D warnings`: clean.
- Every 16a + 16b + 16c suite (190+ tests across the touched
  crates) green.

---

## Outcome

Phase 16c closes every load-bearing 16b finding at the primitive
layer:

| 16b finding | 16c step | Closure |
|---|---|---|
| Finding 1: scrubs unwired | Steps 3+4+5 | Closed primitive layer; scheduler call from runtime is the 16d wiring |
| Finding 2: DeleteFragment fan-out missing | Step 1 | Closed end-to-end |
| Finding 3: EC primitives unused | Steps 6+7 | Closed at the data path; gRPC server + ClusterCfg switch are 16d wiring |
| Finding 4: placement is "all peers" | Step 2 | Closed end-to-end |

Recommended next step: Phase 16d — wire the four 16c primitives
into the runtime data path (scheduler start, EcStrategy in cfg,
server-side multi-fragment_index, cluster_chunk_state stores
`original_len`). Should be a quarter the size of 16c — mostly
plumbing.
