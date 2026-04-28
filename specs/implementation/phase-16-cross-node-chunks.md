# Phase 16 — Cross-Node Chunk Placement

**Status**: Draft (architect)
**Date**: 2026-04-28
**Traces**: I-C4, I-D1, I-D4, ADR-005, ADR-029
**Supersedes**: B-3 finding (per-node gateway compositions store)

## Premise

The spec already commits kiseki to multi-device chunk durability:
EC default + Replication-3 alternative (I-C4), CRUSH-like fragment
placement across distinct devices (I-D4), automatic repair from
parity or replicas (I-D1), pool-level admin policy (ADR-005). The
implementation has the algorithms (`kiseki-chunk::ec`,
`kiseki-chunk::placement`, `kiseki-chunk::pool::DurabilityStrategy`)
but stops at single-device-per-node. Each `kiseki-server` opens one
local raw block device; no inter-node chunk read or write path
exists.

Phase 16 is **integration work to bring the implementation to
its existing spec**. Not a new design; the design exists. The
gap is wiring.

## Non-goals

- Cross-site replication (I-CS3, async).
- Per-pool admin UI / Cluster CA management for pool topology.
- Migration of existing single-node deployments (single-node remains
  a valid topology — the clustered store degenerates).
- Re-encoding existing chunks when pool durability changes (I-C6
  already explicitly defers this to a `ReencodePool` RPC).
- Implementation of the EC alphabet / Reed-Solomon variants beyond
  what `kiseki-chunk::ec` already provides.

## Scope split

### Phase 16a — Infrastructure (this plan)

Wire cross-node chunk placement, peer-aware writes, peer-fetch
reads, cluster-aware GC. Pool-pluggable strategy. Ships
**Replication-3** as the only fully-tested strategy at 16a.

### Phase 16b — Per-cluster-size defaults

Admin-configurable pool topology with sane defaults per cluster
size. Wires EC 2+1 / EC 4+2 / EC 8+3 once 16a's foundations
exist. Likely a follow-up of similar magnitude.

## Architectural decisions

### D-1. New layer `ClusteredChunkStore` wraps local `ChunkOps`

Rather than push network awareness into `kiseki-chunk` (breaks
its layering — the crate is purposefully transport-agnostic),
introduce a new struct in a higher layer that:

1. Owns a local `Box<dyn ChunkOps>` (the existing per-node store).
2. Owns `Vec<PeerConnection>` to the cluster's other nodes.
3. Implements `ChunkOps` itself.

Per-fragment placement happens at this layer. Local fragments
delegate to the inner store; remote fragments route to the peer's
RPC endpoint. The inner store never learns about peers.

**Crate placement**: new `kiseki-fabric-chunk` crate, OR extend
`kiseki-gateway` (already has Raft peer awareness). Recommendation:
new crate. Keeps the gateway focused on protocol mapping and lets
both gateway + future direct-RPC clients use the clustered store.

### D-2. Transport: new internal data-fabric gRPC service

The cluster has four port-namespaces today (Raft 9300, gRPC 9100,
S3 9000, NFS 2049, DS 2052). None of them is the right fit:

- **Raft (9300)**: replicates state; not for arbitrary RPC.
- **gRPC 9100**: external-facing data plane; adding chunk fan-out
  here mixes external and internal traffic in one auth domain.
- **NFS / DS**: protocol-bound; expanding beyond RFC 8881 / 8435
  pollutes their wire-correctness story.

Decision: **add a new internal-only gRPC service** on a dedicated
port (e.g., 9400) protected by mTLS with the Cluster CA (already
the auth fabric for Raft). RPCs:

```
service FabricChunkService {
    rpc PutFragment(PutFragmentRequest) returns (PutFragmentResponse);
    rpc GetFragment(GetFragmentRequest) returns (GetFragmentResponse);
    rpc DeleteChunk(DeleteChunkRequest) returns (google.protobuf.Empty);
    rpc HasChunk(HasChunkRequest) returns (HasChunkResponse);
}
```

The protobuf definitions live in `specs/architecture/proto/kiseki/v1/fabric.proto`.

**Why a new port/service rather than reusing the data-path 9100**:
intra-cluster traffic has different SLAs (always-available,
fail-stop), different auth (mTLS only, no SigV4), and different
allowed callers (peer nodes only, never tenant clients). Mixing
them creates an attack surface and ops complexity.

### D-3. Devices = nodes initially; (node, disk) future-extensible

The placement module already takes opaque `device.id: String`
identifiers. For Phase 16a, `device.id = "node-{node_id}"`. The
cluster places one fragment per node, max. When a node grows to
multiple disks (future), the convention extends to
`"node-{node_id}-disk-{disk_id}"` without API churn.

Tradeoff: a 3-node cluster with EC 4+2 (6 fragments needed) cannot
satisfy I-D4 (no two fragments on the same device) under "device =
node". The deployment must use Replication-3 or EC 2+1 until the
topology grows. Phase 16b's defaults table reflects this.

### D-4. Refcount stays per-chunk on the leader; replication is per-fragment

The chunk store's existing refcount semantics are preserved. The
clustered store maintains the chunk's refcount in its **own**
metadata (a small redb file alongside the local block device). The
leader is the source of truth for refcount; peer fragment storage
is a derived placement.

**GC ordering**: when a chunk's refcount reaches zero, the leader's
clustered store calls `DeleteChunk` on every peer that holds a
fragment. If a peer is unreachable, the deletion is queued and
retried; the chunk is not considered fully GC'd until all peers
ack. This satisfies I-C2 (no GC while refcount > 0) under the
strictest reading.

### D-5. Failure handling

| Scenario | Phase 16a response |
|---|---|
| Peer down at write time | Fail the write with `ChunkError::PeerUnavailable`. The composition delta is not appended (preserves I-L5: composition not visible until chunks durable). |
| Peer down at read time | Try EC recovery from available fragments via `read_chunk_ec` (already implemented). Replication-3 needs only 1 fragment — succeeds if any peer is up. |
| Peer comes back | Background scrub (Phase 16a deferred to 16b) re-replicates any chunks the peer should have. For 16a: stale fragments stay stale; reads succeed via EC recovery. |
| Leader changes | The new leader already has the local fragment store (Raft has caught up). Refcount metadata moves with the leader role via Raft state — TBD whether refcount is Raft-replicated or peer-syncable. **Defer to 16b unless the simpler answer surfaces in 16a.** |

### D-6. Single-node compatibility — degenerate to local

When `cfg.raft_peers.len() == 1`, the clustered store wraps the
local store with no peers and falls through every call directly.
No protocol change. No performance hit. Existing single-node
tests / deployments remain unchanged.

## Data flow

### Write — Replication-3, 3-node cluster

```
   gateway.write(plaintext)
          │
          ▼
   AES-GCM seal envelope
          │
          ▼
   ClusteredChunkStore.write_chunk(envelope, "default-pool")
   │
   ├─ derive chunk_id (existing)
   ├─ pool.durability = Replication{ copies: 3 }
   ├─ devices = [node-1, node-2, node-3]
   ├─ placement = place_fragments(chunk_id, 3, devices) = [0, 1, 2]
   │
   ├─ fragment[0] → local (this node) → ChunkStore.write_chunk
   ├─ fragment[1] → peer node-2 → FabricChunkService.PutFragment
   └─ fragment[2] → peer node-3 → FabricChunkService.PutFragment
          │
          ▼
   wait quorum (all-3 for Replication-3) → ack to gateway
          │
          ▼
   compositions.create + emit_delta (already there — unchanged)
```

### Read — fragment fetch + EC decode

```
   gateway.read(comp_id)
          │
          ▼
   compositions.get(comp_id) → list of chunk_ids
          │
          ▼
   for each chunk_id:
       ClusteredChunkStore.read_chunk(chunk_id)
       │
       ├─ get EcMeta or RepMeta from local metadata
       ├─ fragment[0] → local → ChunkStore.read_chunk
       ├─ fragment[1] (peer) → FabricChunkService.GetFragment
       │   └─ on miss/timeout → mark as unavailable, continue
       ├─ fragment[2] (peer) → ditto
       │
       ├─ if Replication-3: any 1 fragment is sufficient → return.
       └─ if EC X+Y: need ≥X fragments → ec::decode → return.
          │
          ▼
   AES-GCM open envelope → plaintext to caller
```

### Repair — Phase 16a deferred

Phase 16a relies on EC/replication's redundancy for HA on read.
A node coming back online after a crash has stale state (chunks
written during its absence aren't on its local store). This is
acceptable for 16a because reads still succeed via the redundant
fragments on healthy peers.

Phase 16b adds a background scrub that detects fragments-this-node-
should-have-but-doesn't and rewrites them from peers.

## Module changes

| Crate | Change |
|---|---|
| `kiseki-fabric-chunk` (new) | `ClusteredChunkStore` impl of `ChunkOps`, peer connection pool, retry policy. ~1500 LOC. |
| `kiseki-proto` | Add `fabric.proto` definitions. Generated alongside existing protos. ~50 LOC of proto + generated. |
| `kiseki-server::runtime` | Replace `Box<dyn ChunkOps>` construction: when `raft_peers > 1`, wrap `PersistentChunkStore` in `ClusteredChunkStore`. ~30 LOC. |
| `kiseki-server::runtime` | Spawn a `FabricChunkService` listener on the new port (default 9400). ~50 LOC. |
| `kiseki-chunk::pool` | No change (existing `DurabilityStrategy` enum already covers the cases). |
| `kiseki-chunk::placement` | No change (already device-id-agnostic). |
| `kiseki-chunk::ec` | No change. |
| `tests/e2e` | Cross-node read after PUT (closes B-3 finding). Kill-leader-then-read scenario. |

## Test plan

### Unit tests (Rust)

- `ClusteredChunkStore` round-trip: write_chunk → read_chunk via
  in-process mocked peers.
- Peer down at write: returns `PeerUnavailable`, no partial write
  observable.
- Peer down at read with Replication-3: succeeds if ≥1 fragment
  reachable.
- Peer down at read with EC 4+2: succeeds with ≥4 fragments;
  fails cleanly with ≥3 down.
- GC across peers: refcount→0 issues `DeleteChunk` to every peer.

### BDD scenarios (`specs/features/chunk-replication.feature`)

```
@integration @cross-node
Scenario: Cross-node read after leader-only PUT
  Given a 3-node Replication-3 cluster
  When a client PUTs an object via node1's S3 listener
  Then a subsequent S3 GET via node2 returns the same bytes

@integration @cross-node
Scenario: Read survives single-node failure
  Given a 3-node Replication-3 cluster with composition X stored
  When node1 is killed
  Then node2's S3 GET on X still returns the bytes within 5 seconds

@integration @cross-node
Scenario: Write fails when peer quorum is not reachable
  Given a 3-node Replication-3 cluster
  When node2 and node3 are both unreachable
  And a client attempts a PUT via node1
  Then the PUT returns 503 with retry-after metadata
```

### e2e (`tests/e2e/test_cross_node_replication.py`)

- `test_cross_node_read_after_leader_put` — closes B-3 SKIP.
- `test_read_after_leader_kill` — strongest HA test.
- `test_perf_cross_node_overhead` — measures the latency hit of
  3-way fan-out on writes vs single-node baseline.

### Adversarial cases

- Concurrent same-chunk write from two clients (dedup race).
- PutFragment racing with DeleteChunk for the same chunk_id.
- Network partition: node1+2 vs node3 — Raft picks new leader
  in the majority; chunks written during partition.
- Slow peer: one peer takes 10s to ack a write; do we time out
  vs hang the gateway?

## Build sequence (rough)

| Step | Effort | Dependency |
|---|---|---|
| 1. proto/fabric.proto + codegen | 0.5d | none |
| 2. `kiseki-fabric-chunk` crate skeleton + ChunkOps impl | 1d | step 1 |
| 3. FabricChunkService server impl | 1d | step 1 |
| 4. Peer connection pool + retry | 0.5d | step 2 |
| 5. Wire into runtime.rs | 0.5d | steps 2,3,4 |
| 6. Unit tests | 1d | steps 2,3 |
| 7. BDD scenarios | 0.5d | step 5 |
| 8. e2e tests | 0.5d | step 5 |
| 9. Adversarial review pass | 0.5d | step 8 |
| **Total** | **~5-6d** | |

This is ≥2× my earlier "option 1 = 4-8h" estimate. The adversarial
pass found that adversarial pass mattered.

## Open questions to resolve before implementation

1. **Refcount durability**: redb on the leader vs Raft-replicated.
   Leaning redb-on-leader for 16a (simpler, leader change rare in
   3-node cluster); revisit for 16b.
2. **Bootstrap pool topology**: hardcoded "default" pool with
   Replication-3 across all peers in 16a, or admin-configurable?
   Leaning hardcoded for 16a (one less moving part), surfaced as
   admin config in 16b.
3. **Write quorum for Replication-3**: all-3 (strongest) or 2-of-3
   (faster, accepts partition tolerance). All-3 matches I-L2's
   "majority of Raft replicas" pattern but is stricter; 2-of-3
   matches typical replication semantics. **Recommend 2-of-3** —
   matches I-L2 majority-quorum semantics; the 3rd replica
   catches up via Phase 16b scrub.
4. **Encryption layer**: today AES-GCM happens above ChunkOps in
   `mem_gateway.rs`. Replicating ciphertext (not plaintext) is
   correct for I-K1 (no plaintext past the gateway boundary), but
   we need to confirm the envelope is the unit of replication and
   that `PutFragment` accepts the same envelope bytes the leader
   would write locally.

## Risks

1. **Latency floor**: writes now wait on the slowest peer ack.
   Even on a healthy LAN this adds a network RTT. Existing perf
   numbers (516 MB/s NFSv3 write) will drop. Magnitude TBD —
   need to remeasure post-16a.
2. **Refcount drift**: if the leader's redb metadata diverges from
   peer fragment storage (e.g., crashed mid-PutFragment), repair
   is non-trivial. Phase 16b's scrub addresses this; 16a leaves
   the gap.
3. **mTLS bootstrap**: peer connections need Cluster CA certs.
   If the cluster's identity story (Phase 14e) isn't fully solid,
   adding more mTLS-protected paths surfaces those gaps.
4. **GC across nodes during partition**: peer unreachable when
   refcount → 0 → deletion queued forever or eventually
   force-applied? 16a queues; needs a TTL.

## Adversarial validation (before implementation)

Before Step 1 of the build sequence, escalate to adversary mode:

- Are there failure modes (partition, leader change, peer crash
  mid-fragment-write) that the proposed control flow doesn't
  cover?
- Does refcount-on-leader create new SPOFs?
- Is the new fabric port worth the operational complexity vs
  reusing existing ports with namespaced auth?
- Should write quorum be operator-tunable per-pool (open
  question 3)?

The output of that pass either confirms the plan as-is or
reshapes the open questions before we touch code.
