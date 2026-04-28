# Phase 16 — Cross-Node Chunk Placement

**Status**: Draft (architect, revision 2 — post-adversary)
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
its existing spec**.

## Non-goals

- Cross-site replication (I-CS3, async).
- Cluster CA / pool topology UI (admin uses config / env for 16a).
- Migration of existing single-node deployments (single-node
  remains a valid topology — clustered store degenerates).
- Re-encoding existing chunks when pool durability changes (I-C6
  defers this to a `ReencodePool` RPC).
- Beyond-spec EC alphabet variants.
- Auto-rebalance on cluster growth (deferred to 16b scrub).

## Scope split

### Phase 16a — Infrastructure (this plan)

Wire cross-node chunk placement, peer-aware writes, peer-fetch
reads, cluster-aware GC, **Raft-replicated chunk metadata**.
Pool-pluggable strategy. Ships **Replication-3** as the only
fully-tested strategy at 16a.

### Phase 16b — Defaults table + repair scrub

Per-cluster-size defaults (EC 2+1 / EC 4+2 / EC 8+3), background
repair scrub, auto-rebalance on cluster growth. Similar magnitude
to 16a.

## Architectural decisions

### D-1. New layer `ClusteredChunkStore` wraps local `ChunkOps`

A new struct in a higher layer that owns:
1. A local `Box<dyn ChunkOps>` (the existing per-node store).
2. `Vec<PeerConnection>` to the cluster's other nodes.
3. A handle to the **Raft-replicated chunk metadata state machine**
   (D-4) for refcount + placement queries.

Per-fragment placement happens at this layer. Local fragments
delegate to the inner store; remote fragments route through the
fabric gRPC. The inner `ChunkOps` impl never learns about peers.

**Crate placement**: new `kiseki-fabric-chunk` crate. Keeps the
gateway focused on protocol mapping; lets future direct-RPC
clients reuse the clustered store. Depends on `kiseki-chunk`,
`kiseki-proto`, `kiseki-raft`.

### D-2. Transport: namespaced gRPC on existing port 9100

**Revised post-adversary.** Earlier draft proposed port 9400.
Adversary correctly noted this multiplies the operational surface
(firewall rules, cert SAN entries, healthchecks, monitoring) for
no security benefit beyond what a namespaced authz on the existing
data-path port delivers.

Decision: **add `FabricChunkService` to the existing port 9100
gRPC server** with a tonic `Interceptor` that:
1. Requires mTLS with a Cluster-CA-signed peer cert.
2. Asserts the peer cert's SAN includes a `kiseki-fabric/` URI
   (tenant clients have `kiseki-tenant/<org-id>`; cluster nodes
   have `kiseki-fabric/<node-id>`).
3. Rejects with `Unauthenticated` otherwise.

Tenant clients with valid certs can call data-path RPCs but are
rejected by the fabric interceptor on FabricChunkService. Same
TLS fabric, same port; different authz.

The protobuf service:

```protobuf
// specs/architecture/proto/kiseki/v1/fabric.proto
service FabricChunkService {
    rpc PutFragment(PutFragmentRequest) returns (PutFragmentResponse);
    rpc GetFragment(GetFragmentRequest) returns (GetFragmentResponse);
    rpc DeleteChunk(DeleteChunkRequest) returns (google.protobuf.Empty);
    rpc HasChunk(HasChunkRequest) returns (HasChunkResponse);
}
```

### D-3. Devices = nodes initially; (node, disk) future-extensible

Unchanged from rev 1. `device.id = "node-{node_id}"` for 16a;
extends to `"node-{node_id}-disk-{disk_id}"` without API churn.
A 3-node cluster cannot satisfy I-D4 under EC 4+2 (forces 2
fragments per node) — Phase 16b's defaults table reflects this.

### D-4. Refcount + placement metadata via Raft state machine

**Revised post-adversary.** Earlier draft proposed redb-on-leader.
Adversary correctly identified this breaks HA on leader change:
the new leader has zero refcount data, GC blocks, disk leaks until
the old leader returns.

Decision: **chunk metadata is Raft-replicated state** alongside
the existing log entries. Each chunk has a `ChunkMetaEntry` keyed
by `chunk_id`:

```rust
struct ChunkMetaEntry {
    refcount: u64,
    placement: Vec<NodeId>,        // which peers hold a fragment
    pool: String,                  // affinity pool name
    durability: DurabilityStrategy,
    retention_holds: Vec<String>,  // I-C2b
}
```

Updates flow through Raft proposals: increment_refcount,
decrement_refcount, add_placement, etc. Reads are local-applied
state (fast, no quorum).

Implementation: extend `kiseki-log::RaftShardStore` with a
`chunk_meta` table in the existing redb file (the underlying
Raft state machine). One `MetaProposal` enum carries
increment/decrement/place/etc. Apply path mutates the table.

**Performance implication**: writes pay one extra Raft round-trip
(refcount increment) on top of composition delta + fragment
fan-out. The increment can ride the same Raft batch as the
composition delta to amortize.

### D-5. Failure handling — write quorum 2-of-3, read fan-out tolerates miss

**Revised post-adversary.** Earlier draft contradicted itself
(plan body said all-3, open question 3 said 2-of-3). Resolved:
**2-of-3 for Replication-3**. Matches I-L2's majority semantics.
The 3rd replica catches up via 16b's repair scrub.

| Scenario | 16a response |
|---|---|
| Peer down at write, ≥2 peers up | Write succeeds with 2-of-3 ack. Pending replication to the down peer is queued (16b scrub) or expires per ADR-035 node lifecycle. |
| Peer down at write, only 1 peer up | Write fails with `ChunkError::QuorumLost`. Composition delta NOT appended (preserves I-L5). Returned to caller as NFS4ERR_DELAY / S3 503 with retry-after. |
| Peer down at read, ≥1 fragment reachable | Read succeeds. Local fragment if available; else fabric fetch from any healthy peer. (Replication-3 needs 1; EC X+Y needs ≥X.) |
| Peer down at read, no fragments reachable | Read fails with NFS4ERR_DELAY (transient — kernel retries). Distinct from NFS4ERR_IO (data loss). |
| Leader change | Refcount + placement come from Raft (D-4) — new leader has full state. Inflight writes on the old leader: client retries via S3/NFS retry semantics. |
| Slow peer (write fan-out hangs) | Per-peer write timeout = 5s. Timed-out fragment counts as "down" for quorum. |

### D-6. Single-node compatibility — degenerate to local

When `cfg.raft_peers.len() == 1`, ClusteredChunkStore wraps the
local store with no peers. Every call is local. No protocol
change. No performance hit. Existing single-node tests unchanged.

**Cluster-grew-from-1-to-3 case**: documented as admin-action.
Old chunks stay on the original node until the admin runs
`kiseki-control replicate-pool` (16b). Reads of old chunks during
this period work only when the original node is up. **16a does
not auto-migrate.**

### D-7. Async API: parallel `AsyncChunkOps` trait

**New, post-adversary.** `ChunkOps` is sync; cross-node calls are
async (gRPC). Earlier draft handwaved this; adversary noted the
deadlock risk if `block_on` is called from a tokio worker.

Decision: introduce a parallel async trait, leaving `ChunkOps`
unchanged.

```rust
#[async_trait]
pub trait AsyncChunkOps: Send + Sync {
    async fn write_chunk(&self, env: Envelope, pool: &str)
        -> Result<bool, ChunkError>;
    async fn read_chunk(&self, id: &ChunkId)
        -> Result<Envelope, ChunkError>;
    // ... matching the sync trait
}

// Blanket impl: every ChunkOps + Send is also AsyncChunkOps
// (running the sync method on spawn_blocking).
impl<T: ChunkOps + Send + 'static> AsyncChunkOps for SyncBridge<T> { ... }
```

`ClusteredChunkStore` implements `AsyncChunkOps` directly (no
sync version). Existing local stores continue to implement
`ChunkOps`; the runtime wraps them in `SyncBridge` when wiring
into the gateway.

Sync NFS dispatch already uses `block_gateway` which goes through
a dedicated runtime — extends naturally. S3 path is already async.

### D-8. pNFS DS coexistence — clustered store under each DS

**New, post-adversary.** Adversary correctly noted Phase 15
ships a pNFS DS on port 2052 that reads from the local chunk
store. Switching the local store for ClusteredChunkStore changes
DS semantics: each DS now has every chunk (Replication-3) or
fragment-set-needed-for-recovery (EC).

Decision: **the pNFS DS reads from the local ClusteredChunkStore
just like the gateway does.** The kernel's pNFS view of "3
DSes with parallel fragments" remains intact at the protocol
layer. The reality underneath is "every DS can serve every
chunk because clustered store gives it local access" — strictly
better than the spec's expectation.

Phase 15c.5 (kernel-issues-LAYOUTGET, byte-correct Flex Files
body) becomes *easier* to land post-16a, not harder: every node
genuinely can serve any chunk the layout points at, so the body
correctness work isn't fighting against missing data.

EC modes (Phase 16b) make pNFS parallelism more meaningful (each
DS holds a distinct fragment set), but that's 16b scope.

### D-9. Key epoch propagation — Raft-synchronized

**New, post-adversary.** The system DEK key store
(`kiseki-keymanager::OpenRaftKeyStore`) is already Raft-
replicated. When a leader writes a chunk under epoch K, the
epoch K key entry is Raft-committed *before* the chunk write
emits its log entry (current invariant). Followers receive
both via Raft → the epoch is available when the chunk arrives.

Replication lag implication: a follower receiving a fabric
`PutFragment` for a chunk encrypted under a not-yet-applied
epoch returns `NotFound`/`Unavailable`; the leader treats this
as transient and retries (or fails the write under D-5).

Read-side: a peer fetched via `GetFragment` whose key store
hasn't applied the epoch returns `NotFound`. The clustered
store retries on another peer or returns NFS4ERR_DELAY (kernel
retries). This is consistent with RFC 8881 §15.1's transient-
unavailability semantics.

### D-10. Cross-stream ordering — write fragments before delta

**New, post-adversary.** Two replication paths arrive at peers
in undefined order: composition deltas via Raft, chunk fragments
via fabric. Adversary flagged the race where a follower receives
a delta before the chunks it references.

Decision: **write order on the leader is fragments first
(2-of-3 ack), composition delta second.** Adversary's analysis:
under this order, a follower receiving the delta has *probably*
already received its own fragment via fabric. If not (the peer
that didn't ack within 5s), the read path fans out to peers
that did ack — which exist by 2-of-3 quorum.

So the cross-stream ordering issue is moot **if** the read path
always falls back to fabric fetch on local miss. D-1 already
specifies this. Test plan adds a scenario for the slow-peer
fragment-arrives-after-delta race (was a blocker, downgraded
to "explicit test").

### D-11. Pool config from day 1

**New, post-adversary.** Adversary correctly noted ADR-005's
"admin-configurable" can't be honored if the bootstrap pool is
hardcoded. Even at 16a we expose a config surface:

```toml
# kiseki-server config (or equivalent env var)
[chunk_pools.default]
strategy = "replication"
copies = 3
```

Env: `KISEKI_CHUNK_POOL_DEFAULT_STRATEGY=replication-3`.
Defaults to Replication-3 when unset. Multiple pools land in 16b.

### D-12. GC bound on peer-down via ADR-035 node lifecycle

**New, post-adversary.** Pending deletes for a permanently-down
peer are bounded by ADR-035's node lifecycle: when a node enters
`Evicted` state (admin or auto-evicted after drain failure),
**all pending fabric deletes targeting that node are dropped
from the queue and logged as orphan-fragment**. Disk reclamation
at the node level happens via I-D2/I-D5 device replacement.

Soft TTL for in-flight peer-down (without `Evicted` state): 24h.
After 24h, log a warning and drop. Operator-tunable per pool.

## Data flow

### Write — Replication-3, 3-node cluster

```
   gateway.write(plaintext)
          │
          ▼
   AES-GCM seal envelope
          │
          ▼
   ClusteredChunkStore.write_chunk(envelope, "default")
   │
   ├─ derive chunk_id (existing)
   ├─ pool.durability = Replication{ copies: 3 }
   ├─ devices = [node-1, node-2, node-3]
   ├─ placement = place_fragments(chunk_id, 3, devices)
   │
   ├─ fragment[0] → local → ChunkStore.write_chunk
   ├─ fragment[1] → peer node-2 → fabric.PutFragment (5s timeout)
   └─ fragment[2] → peer node-3 → fabric.PutFragment (5s timeout)
          │
          ▼  wait for 2-of-3 ack (D-5)
          │
          ▼
   Raft propose: ChunkMetaEntry{refcount=1, placement=[acked nodes]}
          │
          ▼  Raft commit
          │
          ▼  ack to gateway
          │
          ▼
   compositions.create + emit_delta (existing path; unchanged)
```

### Read — fabric-aware fallback

```
   gateway.read(comp_id)
          │
          ▼
   compositions.get(comp_id) → list of chunk_ids
          │
          ▼  for each chunk_id:
   ClusteredChunkStore.read_chunk(chunk_id)
   │
   ├─ Raft local-applied: get ChunkMetaEntry
   ├─ if local fragment available: return ChunkStore.read_chunk
   ├─ else for each peer in placement:
   │      fabric.GetFragment(chunk_id, fragment_idx)  (3s timeout)
   │      on success: return
   │      on miss/timeout: continue to next peer
   │
   ├─ Replication-3: any 1 fragment is sufficient
   ├─ EC X+Y: ≥X fragments → ec::decode → return
   └─ none reachable: NFS4ERR_DELAY (kernel retries)
          │
          ▼
   AES-GCM open envelope → plaintext to caller
```

## Module changes

| Crate | Change |
|---|---|
| `kiseki-fabric-chunk` (new) | `ClusteredChunkStore` impl of `AsyncChunkOps`, peer connection pool, retry policy. ~1500 LOC. |
| `kiseki-proto` | Add `fabric.proto` definitions. |
| `kiseki-chunk` | New `AsyncChunkOps` trait + `SyncBridge` adapter. Unchanged otherwise. |
| `kiseki-log` (Raft state machine) | Extend with `chunk_meta` table + `MetaProposal` variants. |
| `kiseki-keymanager` | No change (already Raft-replicated). |
| `kiseki-server::runtime` | When `raft_peers > 1`: wrap chunk store in ClusteredChunkStore; register FabricChunkService on existing port-9100 gRPC. ~80 LOC. |
| `kiseki-gateway::s3_server` / `nfs_server` | Plumb the async chunk store through. Mostly type changes. |
| `tests/e2e` | Cross-node read after PUT (closes B-3). Kill-leader-then-read. Slow-peer ordering. |

## Test plan

### Unit tests (Rust)

- ClusteredChunkStore round-trip via in-process mocked peers.
- Peer down at write, ≥2 peers up: 2-of-3 quorum succeeds.
- Peer down at write, ≤1 peers up: returns `QuorumLost`.
- Peer down at read with Replication-3: succeeds via fabric.
- Peer down at read with EC X+Y: succeeds with ≥X fragments.
- GC across peers: refcount→0 → DeleteChunk to all in placement.
- Peer permanently down + node enters Evicted: pending deletes drop.
- Slow peer (5s+ ack) treated as down for quorum.
- Raft-replicated refcount: leader change preserves refcount state.
- Cross-stream ordering: composition delta before fragment arrives;
  read path's fabric fetch resolves the missing local fragment.

### BDD scenarios — `specs/features/chunk-replication.feature`

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
Scenario: Write requires 2-of-3 quorum
  Given a 3-node Replication-3 cluster
  When node2 and node3 are both unreachable
  And a client attempts a PUT via node1
  Then the PUT returns 503 with retry-after metadata

@integration @cross-node @ordering
Scenario: Composition delta arrives before fragment
  Given a 3-node Replication-3 cluster with a slow node3
  When a PUT is issued and the composition delta replicates faster
       than the fragment
  Then a read on node3 still returns the bytes (fabric fetch)

@integration @cross-node @leader-change
Scenario: Refcount preserved across leader change
  Given a 3-node Replication-3 cluster with composition X
       (refcount=1)
  When the leader is killed and a new leader is elected
  Then `kiseki-control inspect-chunk X` reports refcount=1
       on the new leader
```

### e2e (`tests/e2e/test_cross_node_replication.py`)

- `test_cross_node_read_after_leader_put` — closes B-3 SKIP.
- `test_read_after_leader_kill` — strongest HA test.
- `test_write_quorum_lost_returns_503` — partition test.
- `test_perf_cross_node_overhead` — measures latency hit
  vs single-node baseline (Phase 15 perf numbers).

### Adversarial cases

- Concurrent same-chunk write (dedup race): both succeed,
  refcount reaches 2.
- PutFragment racing DeleteChunk for the same chunk_id:
  delete wins iff refcount=0 at the apply moment.
- Network partition node1+2 vs node3: Raft picks new leader in
  majority; chunks written during partition.
- Slow peer 10s ack: treated as down at the 5s timeout.
- Key epoch lag: peer's keymanager hasn't applied epoch K when
  fragment arrives → returns `Unavailable` → leader retries on
  another peer.

## Build sequence

| Step | Effort | Dependency |
|---|---|---|
| 1. proto/fabric.proto + codegen | 0.5d | none |
| 2. Raft state machine extension (chunk_meta + MetaProposal) | 1.5d | none |
| 3. AsyncChunkOps + SyncBridge in `kiseki-chunk` | 0.5d | none |
| 4. `kiseki-fabric-chunk` crate skeleton + ClusteredChunkStore impl | 1.5d | steps 1, 2, 3 |
| 5. FabricChunkService server + interceptor | 1d | step 1 |
| 6. Peer connection pool + retry policy | 0.5d | step 5 |
| 7. Wire into runtime.rs (clustered store + service) | 0.5d | steps 4, 5, 6 |
| 8. Unit tests | 1.5d | steps 4, 5 |
| 9. BDD scenarios | 1d | step 7 |
| 10. e2e tests | 1d | step 7 |
| 11. Prometheus metrics for fabric ops + peer-down | 0.5d | step 7 |
| 12. mTLS SAN updates + cert gen tooling | 0.5d | none (parallel) |
| 13. Docs: protocol-compliance.md + api-contracts.md + ADR for fabric service | 0.5d | step 7 |
| 14. Adversarial review pass (round 2) | 0.5d | step 13 |
| **Total** | **~10d** | |

Two extra days vs revision 1 cover the Raft state machine work
(D-4) + observability + docs that adversary correctly identified
as missing.

## Open questions resolved post-adversary

| Q | Rev 1 answer | Rev 2 answer |
|---|---|---|
| Refcount durability | redb-on-leader | **Raft state machine** (D-4) |
| Bootstrap pool topology | hardcoded | **config / env var from day 1** (D-11) |
| Write quorum for Replication-3 | open | **2-of-3** (D-5) |
| Encryption epoch propagation | open | **Raft-synced; lag = NFS4ERR_DELAY** (D-9) |
| Cross-stream ordering | not addressed | **fragments before delta + fabric fallback** (D-10) |
| pNFS DS interaction | not addressed | **DS reads through ClusteredChunkStore** (D-8) |
| Sync vs async ChunkOps | not addressed | **parallel AsyncChunkOps trait** (D-7) |
| New port vs namespaced | port 9400 | **namespaced on existing 9100** (D-2) |
| GC on permanently-down peer | indefinite queue | **ADR-035 lifecycle integration + 24h TTL** (D-12) |
| Cluster-grew migration | scrub deferred | **admin-action; auto in 16b** (D-6) |

## Risks (revised)

1. **Latency floor**: writes wait on the slowest peer ack (up
   to 5s timeout). Existing perf numbers (516 MB/s NFSv3 write)
   will drop. Magnitude TBD; new test_perf_cross_node_overhead
   measures.
2. **Raft state machine memory footprint**: chunk_meta entries
   are ~80 bytes × N chunks. At 10M chunks → 800 MB Raft state.
   ADR-029 already plans for this scale; chunk_meta fits the
   existing budget.
3. **Cluster-CA bootstrap**: requires the SAN convention
   (kiseki-fabric/<node-id>) to be in cert generation tooling
   (gen-tls-certs.sh from B-1). Update needed.
4. **Refcount Raft proposal rate**: under heavy write load, every
   chunk write = one Raft proposal. If chunk write rate exceeds
   Raft commit rate, Raft becomes a bottleneck. Mitigations:
   batch increments per composition delta; existing log already
   batches.

## Next step

This plan resolves the four blockers and five concerns from the
rev-1 adversary pass. Go to a *short* second adversary pass to
validate the resolutions — looking specifically at:

- Does D-4's Raft state machine introduce new failure modes?
- Does D-2's namespaced authz on port 9100 actually achieve the
  security boundary (compare to separate port more rigorously)?
- Does D-5's 2-of-3 quorum + D-10's "write fragments before delta"
  combine cleanly under all peer-failure permutations?

If the second adversary pass clears, proceed to implementation
build sequence step 1.
