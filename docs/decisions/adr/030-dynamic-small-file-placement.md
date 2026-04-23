# ADR-030: Dynamic Small-File Placement and Metadata Capacity Management

**Status**: Accepted
**Date**: 2026-04-22
**Deciders**: Architect + domain expert
**Adversarial review**: 2026-04-22 (6 findings: 1C 2H 2M 1L, all resolved)
**Context**: ADR-024 (device management), ADR-029 (raw block allocator),
I-L9 (inline threshold), I-C5 (capacity thresholds), I-C8 (bitmap ground truth)

## Problem

At scale (10B+ files, 100PB+), the metadata tier (redb on system NVMe)
becomes a sizing bottleneck. The per-file metadata footprint (~280 bytes)
is unavoidable, but small-file content inlined into deltas causes the
metadata tier to scale with *data volume*, not just file count.

Current state:
- `inline_threshold_bytes` is specified (I-L9) but not implemented
- No dynamic adjustment mechanism exists
- No awareness of system disk capacity or media type
- No workload-driven shard placement across heterogeneous nodes

### Capacity example

10B files, 100PB total, 50-node cluster, RF=3, 256GB NVMe root disks:

| Component | Per file | Cluster total | Per node |
|-----------|---------|--------------|----------|
| Delta log (no inline) | ~200 B | ~2 TB | ~120 GB |
| Chunk metadata | ~80 B | ~0.8 TB | ~48 GB |
| **Subtotal (metadata only)** | **~280 B** | **~2.8 TB** | **~168 GB** |
| Small-file content (if inlined) | variable | 3-200 TB | **blows budget** |

Metadata alone consumes 168 GB/node at 50 nodes. Adding inline content
makes 256 GB root disks insufficient.

## Decision

### 1. System disk auto-detection and budget calculation

At server boot, detect the system partition's capacity and media type.
Compute a metadata budget with configurable soft and hard limits.

```
KISEKI_DATA_DIR → stat() → total_bytes, fs_type
/sys/block/{dev}/queue/rotational → 0 = SSD/NVMe, 1 = HDD
/sys/block/{dev}/device/model → device identification
```

**Defaults** (configurable via env or config file):

| Parameter | Default | Description |
|-----------|---------|-------------|
| `KISEKI_META_SOFT_LIMIT_PCT` | 50% | Normal operating ceiling |
| `KISEKI_META_HARD_LIMIT_PCT` | 75% | Absolute maximum, triggers emergency |
| `KISEKI_META_INLINE_FLOOR` | 128 B | Hard lower bound for inline (metadata-like payloads only) |

**Warning**: If the system disk is rotational (HDD), emit a persistent
warning at boot and in health reports:

```
WARNING: system disk is rotational (HDD). Raft fsync latency will
be 5-10ms per commit. Production deployments require NVMe or SSD
for the metadata partition. See ADR-030.
```

**Reported to cluster** (via gRPC health reports, not Raft — see
SF-ADV-4 resolution):

```rust
struct NodeMetadataCapacity {
    total_bytes: u64,
    used_bytes: u64,
    soft_limit_bytes: u64,
    hard_limit_bytes: u64,
    media_type: MediaType,  // Nvme, Ssd, Hdd
    small_file_budget_bytes: u64,  // derived: soft_limit - reserved - metadata
}
```

### 2. Two-tier redb layout on system disk

Separate metadata (Raft log, chunk index) from small-file content:

```
KISEKI_DATA_DIR/
├── raft/log.redb            ← Raft log entries (bounded by snapshot policy)
├── keys/epochs.redb         ← Key epoch metadata (tiny, <10 MB)
├── chunks/meta.redb         ← Chunk extent index (scales with file count)
└── small/objects.redb       ← Small-file encrypted content (capacity-managed)
```

The first three are **structural metadata** — required regardless of
inline threshold. The fourth (`small/objects.redb`) is **data-tier
extension** — its size is controlled by the inline threshold.

This separation enables:
- Independent monitoring of each tier's growth
- Emergency response: disable inline (threshold → floor) without
  touching structural metadata
- Backup/restore of structural metadata without bulk data

**GC contract** (SF-ADV-6): When `truncate_log` or `compact_shard`
removes a delta that references an inline object, the corresponding
`small/objects.redb` entry is also deleted. The GC path must cover
both stores — orphan entries in `small/objects.redb` are a capacity
leak. The `chunk_id` key is shared between `small/objects.redb` and
the block device extent mapping, so deletion is keyed identically.

### 3. Per-shard dynamic inline threshold

The inline threshold determines whether a file's encrypted content is
stored in `small/objects.redb` (metadata tier) or as a chunk extent on
a raw block device (data tier).

**Threshold is per-shard**, not per-node, because all Raft replicas of
a shard must agree on whether content is inline or chunked (state
machine determinism).

**Computation**: The shard leader computes the threshold from the
minimum small-file budget across all nodes hosting that shard:

```
available = min(node.small_file_budget_bytes for node in shard.voters)
projected_files = shard.file_count_estimate (from delta count heuristic)
raw_threshold = available / max(projected_files, 1)
shard_threshold = clamp(raw_threshold, INLINE_FLOOR, INLINE_CEILING)
```

Where `INLINE_CEILING` is a system-wide maximum (e.g., 64 KB) to
prevent pathological cases.

**Raft log throughput guard** (SF-ADV-1): The threshold is further
clamped by a per-shard Raft log throughput budget
(`KISEKI_RAFT_INLINE_MBPS`, default 10 MB/s). If the shard's inline
write rate (measured over a sliding 10-second window) would exceed
this budget at the current threshold, the effective threshold is
temporarily reduced to floor until the rate drops. This prevents
inline data from starving metadata-only Raft operations (large-file
chunk_ref deltas, maintenance commands, watermark advances) during
write storms.

```
effective_threshold = if shard.inline_write_rate_mbps > RAFT_INLINE_MBPS:
    INLINE_FLOOR
else:
    shard_threshold
```

**Threshold adjustment rules** (I-L9 compatibility):

- Threshold can **decrease** dynamically (safe — new files use chunks)
- Threshold changes are **prospective only** — existing inline data is
  not retroactively migrated
- Threshold **increase** requires cluster admin decision and may trigger
  background migration of small chunked files back to inline (optional,
  maintenance-mode operation)
- Threshold is stored in `ShardConfig` and replicated via Raft

**Read latency note** (SF-ADV-3): After a threshold decrease, existing
inline files remain in `small/objects.redb` (fast, NVMe reads) while
new files of the same size go to block device extents (potentially
slower, especially on HDD). This bimodal latency for same-sized files
is expected behavior. Administrators can normalize it via the
maintenance-mode migration path (move old inline content to chunks),
but this is optional and not automatic.

**Emergency override** (SF-ADV-4): Capacity alerts use **out-of-band
gRPC health reports**, not Raft. Each node periodically reports its
`NodeMetadataCapacity` to the shard leader (or control plane) via the
data-path gRPC channel. If any voter reports hard-limit breach, the
leader commits a threshold reduction via Raft. This works because:
- The full-disk node doesn't need to write Raft entries for the signal
- The leader commits the threshold change with 2/3 majority (the
  full-disk node's vote is not required)
- The full-disk node receives the committed threshold change via Raft
  replication (read-only, no disk write needed until next apply)

### 4. Small-file data path

**Inline content flows through Raft** (SF-ADV-2): Inline content is
carried as payload in the Raft log entry (`LogCommand::AppendDelta`
with `payload` field). The state machine's `apply()` method offloads
the payload to `small/objects.redb` on apply, keyed by `chunk_id`.
The in-memory state machine retains only the delta header (no payload).

This ensures:
- **Snapshot correctness**: `build_snapshot()` reads inline content
  from `small/objects.redb`, includes it in the serialized snapshot.
  `install_snapshot()` writes it back. Learners and restarted nodes
  receive all inline content via snapshot transfer.
- **State machine determinism**: all replicas apply the same log
  entries and write to their local `small/objects.redb` identically.
- **Memory efficiency**: inline payloads are not held in memory after
  apply — only the redb reference remains.

Below threshold (inline path):
```
client write → gateway encrypt → delta with payload →
  Raft client_write (payload in log entry) →
  replicated to voters →
  state machine apply() → offload payload to small/objects.redb →
  in-memory state: header only (no payload)
```

Above threshold (chunk path, unchanged):
```
client write → gateway encrypt → chunk alloc on DeviceBackend →
  extent write (O_DIRECT) → delta with chunk_ref (no payload) →
  Raft client_write → replicated (metadata only)
```

**Read path**: `ChunkOps::get()` checks `small/objects.redb` first
(keyed by chunk_id). If not found, reads from block device extent.
This is transparent to callers.

### 5. Workload-driven shard placement (heterogeneous clusters)

When the cluster has mixed node types (HDD + SSD), the control plane
can migrate shards to better-suited nodes using Raft membership changes.

**Placement levers** (ordered by preference, topology-dependent):

| Lever | When to use | Mechanism |
|-------|------------|-----------|
| Lower inline threshold | Always available | ShardConfig update via Raft |
| Split shard | Shard exceeds I-L6 ceiling | Standard shard split |
| Migrate to larger-NVMe node | Heterogeneous cluster, metadata pressure | Raft add_learner → promote → demote |
| Migrate to SSD node | Heterogeneous, small-file-heavy shard | Raft add_learner → promote → demote |

**Decision tree** (control plane policy):

```
IF shard.metadata_pressure > soft_limit:
  IF can_lower_threshold(shard):
    lower_threshold(shard)               # cheapest, always try first
  ELSE IF shard.exceeds_split_ceiling:
    split_shard(shard)                   # distributes load
  ELSE IF cluster.has_better_node(shard):
    migrate_shard(shard, better_node)    # needs heterogeneous cluster
  ELSE:
    alert("metadata tier at capacity, no placement options available")
```

In a homogeneous cluster, only the first two levers exist. The policy
prunes itself based on what's available.

**Shard migration via Raft**:

Migration is not a special operation — it's a Raft membership change:

1. `raft.add_learner(target_node)` — target receives log/snapshot
2. Wait for learner to catch up (snapshot transfer, then log replay)
3. `raft.change_membership(new_voter_set)` — promote target, demote source
4. Old node removed from voter set, its data eventually GC'd

Properties:
- **Zero downtime**: reads/writes continue during migration
- **Zero data loss**: old node stays in membership until new node is caught up
- **Reversible**: if migration fails, learner is removed, no state change

### 6. Placement change rate limiting

Placement changes (shard migration, learner add/remove) consume
snapshot transfer bandwidth. In HPC environments, workload profiles
shift at job boundaries (hours to days), not continuously.

**Exponential backoff per shard**:

| Observation window | After N-th change |
|--------------------|-------------------|
| 2 hours | 1st (initial observation, minimum floor) |
| 2 hours | 2nd (backoff resets never go below 2h) |
| 4 hours | 3rd |
| 8 hours | 4th |
| ... | doubles each time |
| 24 hours | cap (maximum interval) |

**Reset** (SF-ADV-5): The backoff resets to the **minimum floor of
2 hours**, not to a shorter interval. Even when the shard's workload
profile changes significantly (e.g., small-file ratio crosses a
threshold boundary), the shard cannot be migrated more than once per
2 hours. This prevents oscillating workloads from causing continuous
snapshot transfers. The 2-hour floor is chosen because:
- HPC job boundaries are typically hours apart
- A snapshot transfer of a large shard takes minutes, and the target
  node needs time to stabilize before being evaluated again
- The floor applies per-shard, so different shards can migrate
  concurrently within the cluster-wide rate limit

**Per-cluster rate limit**: at most `max(1, num_nodes / 10)` concurrent
shard migrations cluster-wide, to bound snapshot transfer bandwidth.

### 7. SSD nodes as read accelerators (Raft learners)

For read-heavy small-file workloads, SSD nodes can serve as non-voting
Raft learners:

- Learners receive the full Raft log (including small-file content)
- Learners do NOT participate in elections or commit quorum
- Learners serve read requests (state machine is up-to-date)
- Add/remove learners without disturbing the voter set

Use case: a shard has RF=3 on HDD voters (for capacity) plus 1-2 SSD
learners (for read IOPS). The SSD learners handle small-file reads,
HDD voters handle bulk writes.

**Correction after suboptimal placement**: Initial shard placement
does not need to be optimal. The control plane observes shard metrics
(small-file ratio, read IOPS, p99 latency) and corrects placement
via Raft membership changes. Adding an SSD learner, promoting it to
voter, and demoting an HDD voter is a zero-downtime, zero-data-loss
operation. The cost is one snapshot transfer per migrated shard —
bounded by the rate limiting in §6.

**Promotion path**: if workload shifts permanently, a learner can be
promoted to voter (and an HDD voter demoted) via standard membership
change.

## Consequences

### Positive

- Metadata tier sizing becomes self-managing
- Small files handled efficiently without manual tuning
- Mixed HDD/SSD clusters used optimally
- Placement corrections have zero downtime and zero data loss
- I-L9 compatibility preserved (prospective-only threshold changes)
- Snapshot transfer includes inline content (SF-ADV-2 resolved)

### Negative

- Per-shard threshold adds complexity to `ShardConfig`
- `ChunkOps::get()` now checks two stores (redb + block device)
- Snapshot transfer is the bottleneck for migration speed
- Threshold computation requires cluster-wide metadata aggregation
- Inline writes under high load may be temporarily demoted to chunk
  path (throughput guard), causing brief latency increase for small
  files

### Neutral

- Threshold floor (128 B) means truly tiny files are always inline
- Homogeneous clusters get simpler behavior (fewer levers)
- Migration mechanism is just Raft membership changes — no new protocol
- Bimodal read latency after threshold decrease is expected (SF-ADV-3)

## Adversarial findings (resolved)

| ID | Severity | Finding | Resolution |
|----|----------|---------|------------|
| SF-ADV-1 | High | Raft log throughput saturation from inline writes | Per-shard throughput budget (§3), temporarily lowers threshold to floor under load |
| SF-ADV-2 | Critical | Inline content missing from Raft snapshots | Inline content flows through Raft log; state machine offloads to redb on apply; snapshot reads from redb (§4) |
| SF-ADV-3 | Medium | Bimodal read latency after threshold decrease | Documented as expected; optional admin migration path to normalize (§3) |
| SF-ADV-4 | High | Emergency override fails if full-disk node can't write Raft entries | Capacity reporting via out-of-band gRPC, not Raft; leader commits with 2/3 majority (§3) |
| SF-ADV-5 | Low | Backoff reset allows frequent migrations from oscillating workloads | Minimum 2-hour floor that never resets below (§6) |
| SF-ADV-6 | Medium | No GC path for small/objects.redb | GC contract: truncate_log and compact_shard delete corresponding redb entries (§2) |

## Invariant impact

| Invariant | Impact |
|-----------|--------|
| I-L9 | Extended: threshold is now per-shard and dynamic, but still prospective-only. Increase requires admin action. |
| I-C5 | Unchanged: capacity thresholds on data devices unaffected. |
| I-C8 | Unchanged: bitmap remains ground truth for block device allocations. |
| I-K3 | Unchanged: inline content is still encrypted with system DEK, wrapped with tenant KEK. |

## New invariants

| ID | Invariant |
|----|-----------|
| I-SF1 | The inline threshold for a shard is the minimum affordable threshold across all nodes hosting that shard's voter set. Threshold stored in ShardConfig, replicated via Raft. |
| I-SF2 | System disk metadata usage must not exceed `hard_limit_pct` of system partition capacity. Exceeding soft limit triggers threshold reduction; exceeding hard limit forces threshold to floor and emits alert. Alert uses out-of-band gRPC, not Raft. |
| I-SF3 | Shard migration via Raft membership change must not proceed until the target node has fully caught up (learner state matches leader's committed index). |
| I-SF4 | Placement change rate per shard follows exponential backoff (2h floor, 24h cap). Backoff resets never go below 2h floor. Cluster-wide concurrent migrations bounded by `max(1, num_nodes / 10)`. |
| I-SF5 | Inline content is carried in Raft log entries and offloaded to `small/objects.redb` on state machine apply. Snapshots include inline content read from redb. No inline content is held in the in-memory state machine after apply. |
| I-SF6 | GC (truncate_log, compact_shard) must delete corresponding entries from `small/objects.redb` when removing deltas that reference inline objects. Orphan redb entries are a capacity leak. |
| I-SF7 | Per-shard Raft inline throughput must not exceed `KISEKI_RAFT_INLINE_MBPS` (default 10 MB/s). When exceeded, effective inline threshold drops to floor until rate subsides. |

## Spec references

- `specs/invariants.md` — I-L9, I-C5, I-C8, I-K3
- `specs/architecture/adr/024-device-management-and-capacity.md` — device classes, server disk layout
- `specs/architecture/adr/029-raw-block-device-allocator.md` — DeviceBackend trait, extent allocation
- `specs/architecture/adr/026-raft-topology.md` — Raft membership, multi-Raft pattern
- `specs/implementation/phase-7-9-assessment.md` — open design question on small files
