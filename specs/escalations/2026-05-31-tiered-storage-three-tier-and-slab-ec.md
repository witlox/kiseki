# Escalation: Tiered storage — three-tier durability + slab-EC compactor

**Status**: Open — drives ADR-024 / ADR-030 / ADR-045 amendments and new ADR-048.
**Date**: 2026-05-31
**Raised by**: Architect, after GCP n=6 perf measurement (2026-05-30) and structural review.
**Crosses**: ADR-024 (device management), ADR-030 (small-file placement),
ADR-045 (tiered namespaces), ADR-047 (decoupled write-ack), ADR-026 (Raft topology),
infra/gcp/scripts/setup-raw-storage.sh.

---

## Symptom (motivating measurement)

On the 2026-05-30 GCP `default` profile run (6 × c3-standard-22-lssd,
europe-west1, EC-4+2, 18 shards, parallel-fan PUT path landed):

| shape | aggregate | p50 | p99 | errors |
|---|---:|---:|---:|---:|
| native put-heavy (3 clients × c=16 × 60s) | **2,791 op/s** | 10.5 ms | 71 ms | 0.11% |
| native get-heavy | 31,914 op/s | 530 µs | ~7 ms | 0 |

ADR-042 §14 cluster-aggregate target for n=6 PUT ≈ 140 k op/s.
Measured: 2.8 k = **~2 %** of target. The gap is 50×.

Per-PUT phase decomposition on the leader (after the parallel-fan
landed, so chunk_write and raft_commit are guaranteed concurrent):

| phase | µs / op | what it is |
|---|---:|---|
| `chunk_write` (EC fragment fan-out to 5 peers) | 9,426 | 5 × tonic `put_fragment` RPCs in parallel |
| `raft_commit` (intent fan to 2 followers) | 9,426 | 1+2 raft fan, same wall-clock as chunk_write |
| `composition_record` (wraps raft_commit + comp_create) | 10,344 | 1 fjall composition record |
| `pif.local_put` (leader's own intent fjall write) | 3,114 | one local intent record |
| `gw.comp_create` (composition store local write) | 908 | one local composition record |

Critical path = max(chunk_write, raft_commit) ≈ **9.4 ms/op**, of which
~9 ms is the EC 5-fragment fan. Tail p99 71 ms is openraft batched
commit waiting on slowest follower under sustained load.

---

## Root cause (the structural finding)

Two compounded structural mistakes, both in code paths the architecture
already specifies but the implementation never wired:

### 1. EC-4+2 is misapplied to every PUT regardless of object size

EC-4+2 = RAID 6 with 4 data drives, distributed across nodes. Math is
identical: 4 data + 2 parity = 6 fragments, survives 2 failures, 1.5×
storage overhead. Reed-Solomon, same as the RAID 6 implementation.

EC's only value proposition is **storage efficiency** at the cost of
**per-write coordination overhead** (5 RPC fan-out per PUT, 5× the
critical-path RTT vs. R-2 replication).

| object size | per-byte EC RPC cost | EC justification |
|---:|---:|---|
| 16 KB | ~2.4 µs/B | none — pure RTT tax, storage saving is 8 KB |
| 1 MB | ~0.04 µs/B | marginal — break-even with byte transfer |
| 100 MB | negligible | clear — storage saving justifies the fixed-cost RPC fan |

Every well-known distributed storage system that uses EC **never applies
it per-PUT to small objects**:

- **Facebook f4** — slab-based aggregation, EC over slabs not per-object
- **Ceph BlueStore EC pools** — typically tiered with replicated cache
- **MinIO** — replicates small, EC large (configurable threshold)
- **HDFS Erasure Coding** — hot-then-cold migration into EC

Kiseki does none of these. EC-4+2 is the default for every PUT to every
pool. The pool model (ADR-024 `AffinityPool` with `DurabilityStrategy`)
exists and *supports* per-pool R-3 / R-2 / EC strategies. The gateway
hot path **never consults it**: production goes to a hardcoded `"default"`
pool regardless of object size.

### 2. Small-file inline path (ADR-030) exists in spec, dormant in code

ADR-030 fully specifies inline-payload-in-Raft-delta for small files:

- 4 KB default threshold (`inline_threshold_bytes`)
- 64 KB ceiling (`inline_ceiling_bytes`)
- Per-shard dynamic threshold formula (§3) from budgets reported by all
  voters
- Per-shard Raft throughput guard (`KISEKI_RAFT_INLINE_MBPS`)
- Two-tier redb layout on system disk

Implementation status:

| component | spec | implementation |
|---|---|---|
| `inline_threshold_bytes` field | ✓ | ✓ (hardcoded default 4 KB) |
| Per-shard threshold formula | ✓ | ✗ (function not written) |
| `KISEKI_META_SOFT_LIMIT_PCT` / `KISEKI_META_HARD_LIMIT_PCT` env | ✓ | ✗ (only BDD test fixtures reference) |
| Media-type detection at boot | ✓ | ⚠ partial (`warn_if_rotational` exists, doesn't drive anything) |
| `NodeMetadataCapacity` reporting | ✓ | ✗ (struct only in BDD acceptance steps) |
| Small-tier on dedicated NVMe device | ✓ | ✗ (small_store lives on boot disk per setup-raw-storage.sh) |
| Throughput guard | ✓ | ✗ |
| Emergency threshold reduction | ✓ | ✗ |

The 64 KiB bench is 16× over the 4 KB default threshold → every PUT
takes the chunk path → every PUT pays the EC 5-fragment fan tax.

Both pieces — the size-band routing AND the inline path — are
substrate that already exists in the data model and ADRs, but the
gateway never reads from them. The 50× perf gap is not a performance
bug. It's an implementation that took a hardcoded slow path for
every workload.

---

## What "should" happen

The architecturally-consistent shape, given the existing ADRs:

```
client PUT
  ↓
gateway derives chunk_id + size
  ↓
gateway routes by size via select_pool_for_write(pools, size, ns.tier_policy):
  size ≤ inline_threshold        → inline pool (Raft-only, payload in delta)
  inline_threshold < size ≤ rep_ceiling → replicated pool (R-3 or R-2)
  size > rep_ceiling              → EC pool (EC-4+2, large objects)
```

**Inline path** (≤16 KB default): payload rides in the Raft delta;
state machine apply offloads to `small/objects.redb`. No chunk store,
no fabric fan. Bound by per-shard Raft commit throughput, governed by
`KISEKI_RAFT_INLINE_MBPS` (default 10 MB/s).

**Replicated path** (medium): chunks fan to N-1 peers via the chunk
fabric, with `DurabilityStrategy::Replication { factor: N }`. Per-PUT
fan is 1+(N-1) vs EC's 1+5. R-3 (1+2 fan) drops critical path from
~9 ms (EC) to ~4 ms (replication) on the same fabric.

**EC path** (large objects): unchanged from today. Per-PUT 5-fragment
fan is justified by the storage saving when amortised over MBs of
payload.

For **medium-sized objects on EC pools**, the slab-EC compactor (new
ADR-048) batches N adjacent chunks per shard into a single EC stripe,
moving the 5-fragment fan from per-PUT to per-batch. Per-PUT fan drops
to R-3 levels; storage efficiency stays at EC levels. f4-style slab
pattern on top of ADR-047's intent log.

---

## Default device role policy (revised)

The original ADR-030 §2 implies metadata + small/objects.redb live on
the system disk. `infra/gcp/scripts/setup-raw-storage.sh` line 39
explicitly states `mkdir -p ${meta_dir}/{raft,keys,small,chunks}` *on
the boot disk* and hands every NVMe to `KISEKI_RAW_DEVICES`.

This works at small scale (≤1 B files cluster-wide, where 256 GB boot
disk is sufficient for ~280 B × N_files / RF × 1/N_nodes). It does not
work at scale (10 B files = 168 GB/node metadata at 50 nodes; 100 B
files = 1.7 TB/node).

**Revised default**: every NVMe still defaults to chunk-pool role at
boot — no auto-magic. The runtime emits a loud, persistent warning
when no device is in the metadata role and `KISEKI_DATA_DIR` lives on
the boot disk. Admin promotes one (or more) NVMe to the metadata role
via `kiseki-admin pool add-device metadata-pool <device>`, which:

1. Drains the device from its current pool.
2. `mkfs.ext4` the device.
3. Mounts at `${KISEKI_DATA_DIR}/<device-uuid>/`.
4. Migrates `${KISEKI_DATA_DIR}` symlink to the new mount.
5. Restarts fjall on the new mount, replays from the boot-disk WAL,
   confirms convergence, removes the boot-disk WAL.

This is **operator-driven** by design. Auto-carving a device at boot
without admin opt-in would surprise admins who provisioned their
NVMe budget for chunks. The warning + capacity reporting tells them
when it's a problem; they decide when to act.

---

## Three-tier durability + slab-EC: what changes per ADR

| ADR | change | shape |
|---|---|---|
| ADR-024 | amendment §"three-tier durability by size band" | size → durability strategy mapping; default 16 KB inline ceiling / 4 MB replication ceiling / >4 MB EC |
| ADR-030 | amendment §"admin-driven metadata role" | revoke auto-carve; warning + `kiseki-admin pool add-device metadata-pool` |
| ADR-045 | amendment cross-ref to ADR-024 §"three-tier" | namespace tier-policy can declare per-size-band pool; default inherits ADR-024 |
| ADR-047 | unchanged (substrate is correct) | reference from ADR-048 for hot-tier durability semantics |
| **ADR-048 (new)** | **slab-EC compactor** | committer batches N applied intents into one EC slab; chunk_refs point at `slab_id:offset`; hot-tier eviction once cold-tier durable |

---

## Implementation phasing

Phase 0: this escalation + ADR amendments + ADR-048.
Phase 1: `kiseki-admin` operational primitives (device role, pool moves, namespace tier-policy).
Phase 2: runtime media-type detection + `NodeMetadataCapacity` reporting + per-pool capacity accounting.
Phase 3: gateway write-path tier routing — replace the `"default"` string lookup with `select_pool_for_write`.
Phase 4: dynamic per-shard threshold formula + throughput guard + per-pool capacity thresholds (Healthy/Warning/Critical).
Phase 5: slab-EC compactor in the committer.

Phases 1–3 deliver the inline path + three-tier routing live, A/B-measurable
on the next GCP perf run. Phases 4–5 make it cluster-aware, self-tuning,
and storage-efficient on the medium tier without per-PUT EC tax.

---

## Validation plan

After Phase 3 (smallest measurable point):

- Re-run GCP n=6 native put-heavy with bench namespace on a `replicated`
  pool (`DurabilityStrategy::Replication { factor: 3 }`). Expected:
  PUT critical path ~4 ms (vs ~9.4 ms EC). Aggregate ~5–7 k op/s vs
  current 2.8 k.
- Re-run with bench namespace on `inline` pool (`inline_threshold_bytes`
  raised to 16 KB or 64 KB). For 64 KiB objects this exercises the
  inline-eligible path. Expected: PUT critical path ~2 ms. Aggregate
  bounded by `KISEKI_RAFT_INLINE_MBPS` × shard_count / object_size.

After Phase 5 (slab-EC compactor): same numbers as Phase 3 replication
pool on the write path; storage overhead drops from R-3's 3× to EC's
1.5× via background compaction.

---

## Open questions resolved by this work

- **Why does GCP n=6 PUT sit at 2 % of ADR-042 §14 target?** Because
  every PUT routes through EC fan-out, including objects too small to
  benefit from EC's storage saving. Three-tier routing fixes this.
- **What does the 64 KB inline ceiling buy us?** Latency optimization
  for small files (16 KB inline saves 8 KB Raft frame and 9 ms RTT vs
  the chunk path). Not throughput — per-shard Raft commit budget is the
  binding constraint above ~16 KB.
- **Why is small_store on the boot disk?** Because nothing in the
  current implementation knows it shouldn't be. Admin-driven device
  role assignment with first-boot warning fixes this without auto-magic.
- **What replaces "EC for everything"?** Three durability strategies
  routed by size; slab-EC for the medium tier amortises EC's per-PUT
  fan cost across many writes via background compaction.

---

## References

- ADR-024 §"per-pool durability strategy" — pool model already supports R-N / EC variants.
- ADR-030 §3 — per-shard dynamic threshold formula already specified.
- ADR-045 §D5 — tiering decision policy already lists size-based routing.
- ADR-047 — intent log as durable hot tier; substrate for slab-EC compactor.
- 2026-05-30 GCP perf snapshot: `specs/performance/2026-05-30-gcp-pr142-baseline.md`
  (to be created in cross-cutting Phase X).
- I-L5, I-CS1 (no-loss floor) — preserved throughout.
