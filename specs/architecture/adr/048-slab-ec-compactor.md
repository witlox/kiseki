# ADR-048: Slab-EC compactor — amortise EC fan-out across writes

**Status**: Proposed.
**Date**: 2026-05-31.
**Deciders**: Architect + domain expert.
**Adversarial review**: pending; required before implementation (Phase 5
of the 2026-05-31 escalation rollout).
**Context**: ADR-024 (2026-05-31 amendment: three-tier durability), ADR-026
(per-shard Raft), ADR-029 (raw-block device allocator), ADR-045 (tiered
namespaces), ADR-047 (decoupled write-ack — intent log as durable hot tier),
I-CS1 (no-loss floor), I-L5 (chunks-before-metadata visibility), F-D7
(orphan-fragment scrub).

---

## Problem

ADR-024's amended three-tier durability strategy says: small files go
inline, medium files go replicated, large files go EC. The medium tier's
per-PUT fan-out is 1+2 (R-3) vs. EC's 1+5, so latency is lower — but
storage overhead jumps from EC-4+2's 1.5× to R-3's 3×.

At medium-tier scale (PB+ of medium-sized objects), the 3× vs. 1.5×
storage cost matters operationally. The naive options:

1. **Keep R-3 on medium tier**: 2× storage cost vs. EC. At 1 PB
   medium-tier, ~500 TB of extra physical bytes the cluster has to
   provision.
2. **Use EC-4+2 on medium tier**: regresses to the per-PUT 1+5 fan
   tax the three-tier amendment was specifically designed to avoid.

Both are bad. The medium tier needs *EC storage efficiency on the
backing store* without *EC fan-out on the per-PUT write path*.

### Root cause of the dilemma

EC and per-PUT writes are mismatched in granularity:

- **EC operates at stripe granularity** — Reed-Solomon needs N data
  fragments aligned before it can compute K parity fragments. Each
  fragment is a fraction (1/N) of the stripe; the stripe is the
  natural EC unit.
- **PUTs arrive at object granularity** — one object per PUT, sized by
  the workload (16 KB – few MB for the medium tier).

When EC is applied per-PUT, the "stripe" is the object: it's split into
N data fragments, K parity fragments computed over them, fragments
distributed across N+K nodes. The per-PUT fan-out cost is 1+(N+K−1) RPCs
no matter how small the object.

When EC is applied per **slab** (a fixed-size unit aggregating many
PUTs), the stripe is the slab: many objects packed into one EC unit,
encoded once, distributed once. Per-slab fan-out cost amortises across
all the PUTs in the slab.

### Prior art

- **Facebook f4**: cold-tier blob store; slabs of fixed size (~64 GB
  each) aggregate many warm objects, RS-encoded on slab seal. Live
  for ~years; well-validated pattern.
- **Ceph BlueStore EC pools**: typically combined with a replicated
  cache tier; the cache absorbs writes, the BlueStore EC pool
  receives objects via background migration.
- **HDFS Erasure Coding**: hot-then-cold migration; writes land
  replicated, background compactor migrates aged blocks into EC
  groups.

All three put a replicated (or replicated-equivalent) hot tier in
front of an EC cold tier, with background compaction handling the
migration. Kiseki's intent log (ADR-047) is the natural hot tier;
this ADR specifies the cold tier and the compactor.

---

## Decision

**Hot tier = per-shard Raft intent log + composition store + small_store**
(ADR-030, ADR-047 substrate, unchanged). Writes ack on the
intent durable on `min_acks` voters. The intent carries a chunk_id
reference; the actual chunk bytes are written to one of:

- `small/objects.redb` for inline-band writes (ADR-030),
- the chunk fabric for replicated- and EC-band writes (ADR-024).

**Cold tier = slab-EC**. The committer (ADR-047 §5) extends its async
apply step:

- For chunks that originated in a **replicated pool** with an
  `EC migration policy` set, the committer enqueues the chunk for
  slab compaction.
- For chunks that originated directly in an **EC pool**, the chunks
  already land EC-encoded (unchanged from today, suitable for the
  large-tier band).

**Slab structure**:

```
slab_id (UUIDv4)
├── slab_header (32 B: version, format, data_count, parity_count, byte_size)
├── slab_extent_table (N × 32 B): per-chunk { chunk_id, offset, length, ref_count }
├── slab_data            (concatenated chunks, sorted by chunk_id)
└── slab_parity (K × ceil(slab_byte_size / data_count) bytes RS-encoded)
```

Fragments distributed across the EC placement set per `pick_placement`.
Each fragment is `(slab_byte_size + parity_bytes) / (data_count + parity_count)`
bytes. The K parity fragments are RS-computed over the data fragments
exactly as today's EC-4+2 chunk encoding works — slab is just a bigger
stripe with multiple chunks packed in.

**`ChunkRef` extends**:

```rust
enum ChunkRefLocation {
    /// Live in hot tier; not yet migrated. Read goes through
    /// `local.read_chunk(chunk_id)` or the inline path.
    Hot { pool_name: String },
    /// Migrated into a slab; read goes through slab fragment
    /// reconstruction + extent extraction.
    Cold {
        pool_name: String,
        slab_id: SlabId,
        offset_in_slab: u64,
        length: u64,
    },
}
```

A composition's `chunk_refs` carry `ChunkRef::Location` per chunk. The
gateway read path branches on the location; writes always land in the
hot tier first, the compactor flips chunks to `Cold` after migration.

### Compactor task

Runs as a per-shard background task on the shard leader (or a single
elected node per shard, doesn't have to be the Raft leader). For each
slab-eligible pool:

```
loop:
  1. Read intent log + composition store, find chunks pending migration:
     { chunk_id, pool: ChunkRefLocation::Hot { pool } | pool.requires_migration }
  2. Sort by (pool, chunk_id) — slabs group same-pool chunks.
  3. Accumulate chunks into a candidate slab until either:
     - slab byte budget reached (default: 64 MB), or
     - max chunk count reached (default: 1024), or
     - candidate-age timeout reached (default: 30s — keep cold-tier
       freshness bounded so reads benefit promptly).
  4. Encode slab:
     a. Allocate slab_id.
     b. Concatenate chunk bytes into data buffer.
     c. RS-encode parity fragments.
     d. pick_placement → distribute fragments.
  5. Fan slab fragments to placement nodes (existing fabric).
  6. On quorum durable:
     a. Update composition store's chunk_refs from Hot to Cold via
        Raft delta (atomic per-shard).
     b. Decrement hot-tier refcount on the migrated chunks; release
        chunk-store extents when refcount drops to zero.
  7. Resume at (1).
```

The compactor is **eventually consistent** with respect to reads:
between a chunk being acked to the client and its `ChunkRef` being
flipped to `Cold`, reads see the chunk via the hot-tier path. After
the flip, reads see it via the cold-tier slab path. Both yield the
same bytes (cold-tier reconstruction returns the exact chunk that was
written).

### Hot-tier eviction policy

Hot-tier capacity is bounded; it cannot grow without bound or it
becomes a metadata-tier problem (ADR-030). Eviction rules:

1. A chunk is evictable from the hot tier ONLY when its `ChunkRef`
   has been flipped to `Cold` AND the cold-tier slab is durable on
   `min_acks` (slab placement quorum).
2. Eviction is **opportunistic**, not scheduled: triggered by
   capacity pressure (per-pool used_bytes > soft_limit) or by
   Raft log compaction passing the chunk's birth-seq.
3. The intent log truncates evicted chunks' payload references the
   same way it truncates any committed delta: snapshot-driven.
4. If the compactor falls behind (sustained write rate > compactor
   throughput), the hot tier fills; the runtime asserts backpressure
   on the gateway (per-pool `WriteSurface::is_async_ack_eligible`
   returns false until the compactor catches up).

### Read path

```
gateway.read_chunk(composition_id, chunk_offset, length):
  comp = composition_store.get(composition_id)
  refs = comp.chunk_refs
  for each ref in refs covering [chunk_offset, chunk_offset+length):
    match ref.location {
      Hot { pool_name } =>
        chunks.read_chunk(ref.chunk_id, pool_name)
      Cold { pool_name, slab_id, offset_in_slab, length } =>
        slab = slab_store.get(slab_id) or read_chunk_ec_from_fabric(slab_id)
        slab.extract_chunk(offset_in_slab, length)
    }
```

The slab `extract_chunk` works by:

1. Reading the slab's data fragments via the existing
   `read_chunk_ec` reconstruction path (already implemented for the
   large-tier band's per-PUT EC).
2. Indexing into the reconstructed slab buffer at `offset_in_slab`
   for `length` bytes.

The slab fragment reads dominate read latency for cold-tier chunks
(EC reconstruction is non-trivial). To keep cold-tier reads fast,
the gateway maintains an LRU cache of recently-extracted slabs in
memory.

### Slab GC

When a chunk's refcount drops to zero (delete + scrub), the slab
extent table entry's refcount is decremented. When *every* chunk
in a slab has refcount=0, the whole slab is garbage-collected:
fragments deleted from placement nodes, slab_id released.

When some-but-not-all chunks in a slab are deleted, the slab is
*fragmented*: it stores unreferenced bytes. The compactor's
maintenance pass rewrites highly-fragmented slabs (> 50 % of bytes
unreferenced) into fresh slabs containing only live chunks, then
GCs the old slab. This is f4's "rebalance" pass.

### Backpressure

When the compactor cannot keep up with hot-tier write rate:

- Per-pool hot-tier `used_bytes > soft_limit` → emit
  `kiseki_compactor_backlog_seconds` metric + audit event.
- `used_bytes > hard_limit` → gateway asserts
  `WriteSurface::async_ack_eligible = false` for the affected pool;
  writes block until compactor catches up.
- A persistent backlog (> 1 minute at hard limit) → escalate to
  cluster_warning ERROR, visible in admin status.

### Invariants

New invariants added by this ADR (ID space continues from ADR-030):

| ID | Statement |
|---|---|
| I-SE1 | A composition's `chunk_refs[i].location` flips from `Hot` to `Cold` atomically via Raft delta, AFTER the cold-tier slab is durable on `min_acks` placement nodes (preserves I-L5). |
| I-SE2 | The compactor MUST NOT migrate a chunk to a slab before the chunk's `min_acks`-durable confirmation on the hot tier (preserves I-CS1). |
| I-SE3 | A slab fragment is deleted from a placement node ONLY when the slab's per-chunk refcount table shows every chunk in the slab is unreferenced. Partial deletions rewrite the slab; they do not delete fragments. |
| I-SE4 | Hot-tier eviction of a chunk is permitted ONLY when its `ChunkRef::location == Cold` AND the slab quorum has confirmed durability. |
| I-SE5 | Slab fragment placement uses the same `pick_placement` rendezvous hash as per-PUT EC (deterministic, stateless, no separate slab placement state to corrupt). |
| I-SE6 | Compactor backlog metric reflects per-pool oldest pending-migration chunk's age. Sustained `> 60 s` at hard-limit triggers backpressure on the gateway's `is_async_ack_eligible` for that pool. |

### Performance accounting

Per-PUT cost (medium tier, slab-EC pool):

- Hot tier write: Raft intent fan (1+(replication_factor−1)) = 1+2 with R-3 → ~2 ms.
- Cold tier migration: batched, off the write critical path. Amortises
  EC fan-out across N chunks per slab. At 64 MB slabs and 64 KiB
  chunks: 1024 chunks per slab; per-chunk EC fan cost = 1 slab fan / 1024 chunks.

Per-PUT critical path = hot-tier intent fan only. Identical to ADR-024's
medium-tier (R-3) band's latency floor; better storage overhead
(1.5× EC) than R-3's 3× once compaction catches up.

### Storage overhead, steady state

```
overhead = (hot_tier_replication_factor × hot_tier_dwell_fraction)
         + (1.5 × ec_dwell_fraction)
```

For R-3 hot tier with 1 min compaction backlog at 10 k op/s:

```
hot_tier_residency = 10k × 60s × 64KB ≈ 36 GB  (per shard)
cold_tier_residency = (cluster_total − 36 GB) × 1.5
≈ EC storage overhead in steady state, plus ~1 GB / shard hot-tier overhead
```

For PB-scale clusters the hot-tier residency is negligible vs. the cold
tier. Steady-state cluster storage overhead ≈ 1.5× (EC overhead).

---

## Alternatives considered

### A. Per-PUT EC on the medium tier (status quo without this ADR)

5-fragment fan per PUT, no slab aggregation. Storage overhead 1.5×.
Per-PUT critical path ≈ 10 ms (5 EC RPCs in parallel).

Rejected: 50× perf gap vs. ADR-042 §14 target, measured 2026-05-30.

### B. R-3 medium tier without cold tier

ADR-024's amendment without ADR-048. Storage overhead 3×.
Per-PUT critical path ≈ 4 ms (R-3 fan).

Rejected: storage cost prohibitive at PB scale (extra 1.5×PB physical
bytes vs. EC's 1.5×).

### C. Per-slab EC with the slab seal on the write path

Buffer writes in a per-shard slab buffer; when slab is full, EC-encode
and ack the buffered writes together. Defers ack until slab seals;
latency = `slab_byte_size / write_rate` worst case (could be seconds).

Rejected: unbounded ack latency for low-throughput pools; complicates
the surface-specific ack semantics in ADR-047 §F-3.

### D. Replicate writes; EC reads via online encoder

Like Ceph BlueStore's optimisation: writes hit replication; reads
either go to replicated copies (fast) or to an online EC encoder
that synthesises parity on demand.

Rejected: reads of recent writes touch the replicated copies fine,
but storage overhead never drops below 3×; the EC encoder doesn't
free the replicas.

### E. Hot-tier `EC-2+1` (smaller stripe)

EC-2+1 has fewer fragments (3 vs 6 for 4+2). Could be used on the
hot tier directly without the slab aggregation. Fan-out 1+2 (same as
R-3). Storage overhead 1.5× (vs. 3× R-3).

Tempting but: 2+1 only survives 1 node failure (not 2). The
durability matrix downgrade is not acceptable for the hot tier — every
write would have a higher catastrophic-loss probability than today's
R-3 intent log. Could be revisited per-pool for clusters that
explicitly accept reduced durability.

---

## Implementation phasing (within Phase 5 of the 2026-05-31 escalation)

| sub-phase | what |
|---|---|
| 5.1 | Slab format types + serialization + tests (`crates/kiseki-chunk-cluster/src/slab.rs`) |
| 5.2 | `ChunkRefLocation` enum + ChunkRef changes; protobuf updates for delta payload |
| 5.3 | Compactor task (per-shard background, configurable trigger thresholds) |
| 5.4 | Slab fan-out via existing `pick_placement` + `write_chunk_ec` |
| 5.5 | Atomic `Hot → Cold` chunk_ref flip via Raft delta from the committer |
| 5.6 | Read path: branch on `ChunkRef::location`; slab extract + LRU cache |
| 5.7 | Hot-tier eviction triggered by chunk-ref flip + per-pool capacity pressure |
| 5.8 | Slab GC + fragmented-slab rewrite |
| 5.9 | Compactor backpressure → gateway `is_async_ack_eligible` |
| 5.10 | Metrics: `kiseki_compactor_lag_seconds`, `kiseki_compactor_slabs_per_sec`, `kiseki_slab_extract_seconds` histogram, per-pool hot-tier `used_bytes` |
| 5.11 | BDD scenarios: round-trip via compactor, partial-slab delete + rewrite, compactor backlog triggers backpressure, slab fragment failure → EC reconstruct |

---

## Risks (for adversary gate-1)

1. **Slab corruption on partial migration**: if the compactor crashes
   between fragment fan-out and Raft chunk-ref flip, the slab may
   exist on placement nodes with no composition pointing to it. F-D7
   (orphan-fragment scrub) addresses this; slab-level orphan tracking
   reuses the same mechanism with a slab_id-aware sweep.
2. **Read latency cliff on cold-tier cache miss**: cold-tier reads
   pay EC reconstruction (~ms) + slab extraction (~µs). For
   read-heavy medium-tier workloads, the LRU cache (5.6) must be
   sized for the working set. Open question: cache size as
   percentage of node RAM (1 %? 5 %?), needs measurement.
3. **Compactor backlog during burst**: gateway backpressure (5.9)
   converts "writes block" → "writes refused" once the hot tier
   fills past hard-limit. Surface-specific semantics (S3 backpressure
   vs. POSIX EAGAIN) need explicit specification — open question.
4. **Slab rewrite amplification**: highly-fragmented slabs rewrite
   the entire slab (~64 MB) to keep a few KB of live data. The
   rewrite-threshold (50 %) is a heuristic; measurement under typical
   deletion patterns will tune it.
5. **Mixed hot+cold tier consistency under leader change**:
   `ChunkRef::location` is per-composition Raft state; leader changes
   replicate the flip via the existing intent log. A follower that
   missed the flip will see stale `Hot` references until it catches
   up; reads still succeed because the hot-tier path remains valid
   until eviction.

---

## Rollout

Pre-production. Land Phases 0–4 first (the three-tier routing without
slab compaction). At that point the cluster operates with R-3 medium
tier (no slab-EC), measurable on GCP. Phase 5 adds the compactor;
medium-tier storage overhead drops from 3× to 1.5× over time without
affecting the write critical path.

Per-pool opt-in: only pools configured with
`compaction_strategy: SlabEc { ... }` engage the compactor. Pools
without it stay R-3 forever (admin's call for clusters that prefer
operational simplicity over storage efficiency).

## Consequences

### Positive

- Medium-tier storage overhead drops from R-3's 3× to EC's 1.5×
  steady state, without per-PUT EC fan tax.
- Write critical path bounded by hot-tier R-3 (~2 ms) regardless of
  pool's eventual durability strategy.
- Compactor work is throughput-shaped (background, batched) — much
  easier to size and schedule than latency-shaped per-PUT EC.
- Slab pattern matches well-validated prior art (f4, Ceph, HDFS).

### Negative

- Read path has more branches (hot vs cold); cold-tier reads pay
  slab extraction overhead.
- Compactor is a new background system with its own failure modes
  (backlog, partial-migration, fragmentation).
- More metadata: per-slab extent table, per-chunk
  `ChunkRefLocation`, per-slab refcount and fragmentation tracking.

### Neutral

- Existing per-PUT EC code path (used for the large tier) is reused
  for slab fan-out; no new transport code.
- Capacity-planning models (ADR-024 amendment) account for the
  hot-tier residency explicitly.

---

## Spec references

- ADR-024 (2026-05-31 amendment) — three-tier durability defaults; medium-tier R-3 with slab-EC migration option.
- ADR-026 — per-shard Raft; compactor runs per shard.
- ADR-029 — raw-block extent allocator; slab fragments use the same `DeviceBackend`.
- ADR-030 — small/objects.redb hot-tier inline storage; structurally similar to the slab compactor but for sub-threshold writes.
- ADR-045 — namespace tier-policy can declare per-size-band pool with slab-EC opt-in.
- ADR-047 — intent log durability; the substrate this ADR builds on.
- Escalation 2026-05-31: tiered storage + slab-EC (`specs/escalations/2026-05-31-tiered-storage-three-tier-and-slab-ec.md`).
- I-CS1, I-L5, F-D7 — preserved.
