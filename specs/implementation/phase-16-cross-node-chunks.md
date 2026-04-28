# Phase 16 — Cross-Node Chunk Placement

**Status**: Draft (architect, revision 3 — post-adversary round 2)
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

**Trade-off note (post-adversary round 2)**: this is *weaker
defense-in-depth* than separate port + firewall. Both fabric
and tenant certs are signed by the same Cluster CA, so the
interceptor's SAN check is the *only* defense between a
compromised tenant cert and full chunk-store access. With
separate-port + firewall, an attacker would also need to
breach the network boundary. Accepted for 16a because
operational impact (1 fewer port) is judged greater than the
security delta in a closed cluster network. Re-evaluate when
exposing the cluster network outside a tenant's VPC.

**Authentication ≠ authorization (acknowledged 16b gap)**:
the interceptor verifies the peer cert's identity (`kiseki-fabric/
<node-id>` SAN). It does *not* check whether that node is currently
expected to be a peer (e.g., not in `Evicted` state per ADR-035).
A compromised cert from an evicted node retains access until
expiry. 16a accepts this; 16b adds CRL-style cluster-state
checks or short-lived certs.

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

**Revised post-adversary round 1.** Earlier draft proposed
redb-on-leader. Adversary correctly identified this breaks HA on
leader change: the new leader has zero refcount data, GC blocks,
disk leaks until the old leader returns.

Decision: **chunk metadata is Raft-replicated state** alongside
the existing log entries. Each chunk has a `ChunkMetaEntry` keyed
by **`(tenant_id, chunk_id)`** (not just `chunk_id`):

```rust
// Key: (tenant_id, chunk_id) — see "tenant isolation" below.
struct ChunkMetaEntry {
    refcount: u64,
    placement: Vec<NodeId>,        // which peers hold a fragment
    pool: String,                  // affinity pool name
    durability: DurabilityStrategy,
    retention_holds: Vec<String>,  // I-C2b
}
```

**Tenant isolation (post-adversary round 2)**: keying by
`(tenant_id, chunk_id)` rather than `chunk_id` alone prevents an
I-T1 leak. Under `CrossTenant` dedup policy a single chunk_id can
be referenced from multiple tenants; without per-tenant keying, a
read of `chunk_meta[chunk_id].refcount` reveals the count across
tenants. With per-tenant keying, each tenant's refcount is
independent — same plaintext yields the same chunk_id (deduped at
the chunk store) but bills/audits/lifecycles per-tenant. The
chunk store's own dedup remains content-addressed; only the
metadata layer is partitioned.

Updates flow through Raft proposals: increment_refcount,
decrement_refcount, add_placement, etc. Reads are local-applied
state (fast, no quorum).

Implementation: extend `kiseki-log::RaftShardStore` with a
`chunk_meta` table in the existing redb file (the underlying
Raft state machine). One `MetaProposal` enum carries
increment/decrement/place/etc. Apply path mutates the table.

**Atomicity with composition delta (post-adversary round 2 —
Q1.F + Q3.E fix)**: a chunk write's `MetaProposal::Create
{ refcount: 1, placement, ... }` AND the corresponding
`DeltaAppend { composition delta }` MUST be carried in a
**single Raft proposal** — `CombinedProposal { meta, delta }` —
applied atomically by the state machine. Without this, leader
crash between the two leaves either:

- Orphan fragments + no composition (meta committed, delta lost)
- Composition referencing nonexistent chunk_meta (delta committed,
  meta lost) — breaks I-L5

The combined proposal is the unit of client ack: the gateway only
returns success after the proposal is Raft-committed. **Move the
client-facing `OK` to AFTER Raft commit, not after fragment-fan-
out ack.** This restores I-L2's "durable on Raft majority before
ack" invariant for the chunk path.

Compaction: chunk_meta entries with `refcount > 0` are held in
the Raft snapshot. Entries with `refcount == 0` after GC are
tombstoned and pruned at the next compaction (handled by the
existing kiseki-log compaction path, which already prunes
tombstoned deltas).

**Performance implication**: writes pay one Raft round-trip
covering both meta and delta (vs the previous "composition delta
only" round-trip). Single-trip, batchable with concurrent writes.

### D-5. Failure handling — write quorum 2-of-3, read fan-out tolerates miss

**Revised post-adversary round 1.** Earlier draft contradicted
itself (plan body said all-3, open question 3 said 2-of-3).
Resolved: **2-of-3 for Replication-3**. Matches I-L2's majority
semantics. The 3rd replica catches up via 16b's repair scrub.

**Revised post-adversary round 2** to reflect the D-4 atomic
proposal: the client ack moves to *after* Raft commit of the
combined proposal, not after fragment-fan-out ack. This closes
the leader-crash-after-ack-before-Raft case (Q3.E).

| Scenario | 16a response |
|---|---|
| Peer down at write, ≥2 peers up | Fragment fan-out succeeds 2-of-3 → CombinedProposal proposed → Raft commits → client ack. Pending replication to the down peer queued (16b scrub) or expires per ADR-035 node lifecycle. |
| Peer down at write, only 1 peer up | Fragment fan-out fails (1-of-3). No CombinedProposal proposed. Client gets `ChunkError::QuorumLost` → NFS4ERR_DELAY / S3 503 with retry-after. |
| Leader crashes after fragment fan-out, before CombinedProposal commits | Fragments orphaned on 2-of-3 peers. New leader has no chunk_meta or composition delta — neither was committed. The orphan fragments are reclaimed by the orphan-fragment scrub (24h TTL, see Risks). Client sees the failure via S3/NFS retry path (no ack was issued). |
| Leader crashes between CombinedProposal Raft commit and client ack | Raft commit means majority of replicas have the proposal; new leader has the chunk_meta + composition delta. Reads succeed. Client retries the write (idempotent under content-addressed dedup — same chunk_id, refcount unchanged). |
| Peer down at read, ≥1 fragment reachable | Read succeeds. Local fragment if available; else fabric fetch from any healthy peer. (Replication-3 needs 1; EC X+Y needs ≥X.) |
| Peer down at read, no fragments reachable | Read fails with NFS4ERR_DELAY (transient — kernel retries). Distinct from NFS4ERR_IO (data loss). |
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

### D-10. Cross-stream ordering — fragments before atomic proposal

**Revised post-adversary round 2 to reflect D-4's atomic
proposal.** Two replication paths arrive at peers in undefined
order: composition delta + chunk_meta via Raft (now combined into
a single proposal per D-4), and chunk fragments via fabric.

Decision: **leader write sequence is**:
1. Fan out fragments via fabric. Wait for 2-of-3 ack (5s timeout).
2. Submit `CombinedProposal { meta, delta }` to Raft.
3. Wait for Raft commit (majority ack).
4. Return success to the caller.

After step 3, every Raft majority-replica has both the chunk_meta
AND the composition delta atomically. After step 1, at least 2 of
3 peers have the fragment locally. The peer that didn't ack the
fabric fan-out (if any) might apply the Raft proposal before
receiving its fragment via 16b's repair scrub — a read on that
peer falls back to fabric fetch from the 2 peers that did ack
(D-1 + D-5 read path). Test plan still includes the slow-peer
ordering scenario as an explicit test.

The atomic proposal also makes the I-L5 invariant ("composition
not visible until referenced chunks durable") trivially enforced
on followers: applying the proposal means the chunk_meta entry
exists with `placement` listing the peers that did ack, so the
read path knows exactly which peers to fabric-fetch from.

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
   gateway.write(plaintext, tenant_id)
          │
          ▼
   AES-GCM seal envelope
          │
          ▼
   ClusteredChunkStore.write_chunk(envelope, tenant_id, "default")
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
          ▼  fragments durable on a majority of peers
          │
          ▼
   Raft propose: CombinedProposal {                            ← D-4 atomic
       meta: ChunkMetaEntry {
           key: (tenant_id, chunk_id),                          ← D-4 tenant key
           refcount: 1,
           placement: [acked nodes],
           ...
       },
       delta: composition_delta,
   }
          │
          ▼  Raft commit (majority of replicas have BOTH
          │              meta and delta atomically)
          │
          ▼  ack to gateway              ← I-L2 ack-after-commit
          │
          ▼
   gateway returns success to client
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
| **Refcount keying** | unspecified | **`(tenant_id, chunk_id)` for I-T1** (D-4, round 2) |
| **Meta + delta atomicity** | unspecified | **single `CombinedProposal`; client ack after Raft commit** (D-4 + D-10, round 2) |
| Bootstrap pool topology | hardcoded | **config / env var from day 1** (D-11) |
| Write quorum for Replication-3 | open | **2-of-3** (D-5) |
| Encryption epoch propagation | open | **Raft-synced; lag = NFS4ERR_DELAY** (D-9) |
| Cross-stream ordering | not addressed | **fragments before atomic proposal + fabric fallback** (D-10) |
| pNFS DS interaction | not addressed | **DS reads through ClusteredChunkStore** (D-8) |
| Sync vs async ChunkOps | not addressed | **parallel AsyncChunkOps trait** (D-7) |
| New port vs namespaced | port 9400 | **namespaced on existing 9100, defense-in-depth note** (D-2, round 2) |
| GC on permanently-down peer | indefinite queue | **ADR-035 lifecycle integration + 24h TTL** (D-12) |
| Cluster-grew migration | scrub deferred | **admin-action; auto in 16b** (D-6) |
| **Orphan fragments on aborted writes** | unspecified | **24h TTL + Prometheus metric** (Risk #5, round 2) |
| **chunk_meta compaction** | unspecified | **tombstone-then-prune in existing log compaction** (D-4, round 2) |
| **Cert revocation / authn vs authz** | unspecified | **16b gap; documented** (D-2, round 2) |

## Risks (revised round 2)

1. **Latency floor**: writes wait on the slowest peer ack (up
   to 5s timeout) + Raft commit. Existing perf numbers (516 MB/s
   NFSv3 write) will drop. Magnitude TBD; new
   `test_perf_cross_node_overhead` measures.
2. **Raft state machine memory footprint**: chunk_meta entries
   are ~80 bytes × N chunks (with `(tenant_id, chunk_id)` keying,
   the dedup overhead per tenant adds ~16 bytes). At 10M chunks
   single-tenant → 800 MB Raft state; ADR-029 budget holds.
3. **Cluster-CA bootstrap**: SAN convention
   (`kiseki-fabric/<node-id>`) must land in cert generation
   tooling (`gen-tls-certs.sh` from B-1). Cert rotation /
   revocation flow is a 16b gap (acknowledged in D-2).
4. **Raft proposal rate under heavy write load**: every chunk
   write = one `CombinedProposal`. Raft batches proposals at
   commit time, but bursty workloads can stack. Mitigation:
   the gateway's existing per-tenant rate limit + ADR-021
   advisory backpressure surface — already wired.
5. **Orphan fragments on aborted writes (Q1.A)**: if the
   leader crashes between fragment fan-out and `CombinedProposal`
   commit, the fragments-on-2-of-3-peers have no Raft metadata
   referencing them. They become orphans. Bounded recovery: a
   24-hour orphan-fragment TTL — every fragment carries a
   `created_ms` timestamp; the local chunk store sweeps fragments
   older than 24h with no `chunk_meta` entry referring to them.
   Documented operational metric: `kiseki_orphan_fragments_total`
   (Prometheus counter); alert on rising trend. Phase 16b's
   repair scrub additionally reconciles cross-peer.
6. **Defense-in-depth trade-off (D-2)**: namespaced authz on
   port 9100 is one bug away from full data-plane compromise.
   Documented as accepted-for-16a; revisit when exposing the
   cluster network beyond a tenant's VPC.

## Next step

This plan resolves the four blockers and five concerns from
adversary round 1, and the four atomicity / isolation /
trade-off / orphan-bound findings from adversary round 2.

Round-2 adversary findings resolved inline:

| Finding | Resolution |
|---|---|
| Q1.A — orphan fragments (disk leak) | Risk #5 + 24h orphan TTL + Prometheus metric |
| Q1.B — chunk_meta compaction unspecified | D-4: tombstone-then-prune in existing kiseki-log compaction path |
| Q1.C — tenant isolation refcount leak | D-4: `(tenant_id, chunk_id)` key |
| Q1.F + Q3.E — atomicity gap | D-4: `CombinedProposal { meta, delta }` single Raft proposal; client ack after Raft commit |
| Q2.A — namespaced authz weaker than firewall | D-2: explicit trade-off note |
| Q2.D — auth ≠ authz, no revocation | D-2: 16b gap, acknowledged |

No round-3 adversary needed unless implementation surfaces a
genuinely new question. Proceed to build sequence step 1.
