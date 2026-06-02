# Capacity planning — metadata + small-tier budget

> Sizing reference for production deployments. Derived from
> ADR-024 (2026-05-31 amendment) + ADR-030 + ADR-048.

This is the planning artifact for "how much fast NVMe does each node
need for the metadata + small-tier role". The numbers are derived from
specs, not measured live — use them as planning input, not as a perf
guarantee.

## Per-file metadata footprint

| component | per file | source |
|---|---:|---|
| Delta log header (chunk-path) | ~200 B | ADR-030 §1 |
| Chunk metadata record | ~80 B | ADR-024 §"data model changes" |
| **Subtotal (no inline content)** | **~280 B** | sum |
| fjall LSM overhead | ~100-200 B | empirical, post-ADR-022 rev-2 |
| **Planning footprint** | **~500 B per file** | rounded for safety |

This is **per file, regardless of file size**. A 10 KB file pays the
same metadata overhead as a 1 GB file.

## Cluster_max_files (no inline)

`cluster_max_files = (per_node_metadata_budget × node_count) / RF / 500B`

| metadata NVMe per node | N=10 nodes (RF=3) | N=25 (RF=3) | N=50 (RF=3) | N=100 (RF=3) |
|---:|---:|---:|---:|---:|
| 50 GB | 333 M | 833 M | 1.7 B | 3.3 B |
| 100 GB | 667 M | 1.7 B | 3.3 B | 6.7 B |
| 200 GB | 1.3 B | 3.3 B | 6.7 B | 13.3 B |
| 375 GB (1× c3-lssd) | 2.5 B | 6.3 B | 12.5 B | 25 B |
| 750 GB (2× c3-lssd) | 5 B | 12.5 B | 25 B | 50 B |
| 1.5 TB (4× c3-lssd) | 10 B | 25 B | 50 B | 100 B |

**Practical lower bound**: 1 c3-standard-22-lssd local SSD (375 GB)
dedicated to metadata supports ~12.5 B files on a 50-node cluster.
That's far above typical workloads.

## Cluster_max_files (with inline content)

When inline content is also stored on the metadata NVMe (the small-tier
mode), per-file footprint grows by the inline payload size:

`per_file_with_inline = 500 B (metadata) + avg_inline_payload_size`

For 16 KB inline threshold and average inlined payload of 8 KB:
`per_file = 8 KB + 500 B ≈ 8.5 KB`

| metadata NVMe per node | N=50 nodes (RF=3), 16 KB threshold |
|---:|---:|
| 50 GB | ~100 M files (per-file = 8.5 KB) |
| 100 GB | ~200 M files |
| 200 GB | ~400 M files |
| 375 GB | ~735 M files |
| 750 GB | ~1.5 B files |
| 1.5 TB | ~3 B files |

**Inline budget shrinks the file count by ~17×** (vs. 500 B per file
without inline) when inline payloads average 8 KB. If the workload is
dominated by sub-1 KB metadata-like files (xattrs, symlinks, small
JSON), the inline budget impact is minimal.

## Sizing recommendations by workload

### Small-cluster IOPS (≤1 B files)

- 1× NVMe per node (any size ≥100 GB) in metadata role
- inline_threshold = 16 KB (default)
- Replication ceiling = 4 MB (medium tier on R-3)
- Cold tier EC-4+2 for objects > 4 MB

### Large-scale object store (10-100 B files)

- 1× NVMe per node, ≥500 GB
- inline_threshold = 4 KB (smaller, metadata-only)
- Replication ceiling = 1 MB (most objects in medium tier on R-3)
- Cold tier EC-4+2 with slab-EC compactor (ADR-048) enabled
- Slab byte budget = 64 MB (default)
- Steady-state storage overhead: ~1.5× via slab compaction

### Bulk/archival (≤1 B files, mostly large)

- 1× NVMe per node, ≥250 GB (lighter metadata footprint)
- inline_threshold = 4 KB (rare, mostly chunk-tier)
- Replication ceiling = 256 KB (most writes go straight to EC)
- Cold tier EC-8+3 for higher storage efficiency on large objects

### Mixed cluster (heterogeneous nodes)

- Each node carves whatever NVMe it has into metadata role
- The cluster's effective `cluster_max_files` = sum across nodes
- ADR-045's tier-affinity routes hot-tier workloads to NVMe-heavy
  nodes; cold-tier to bulk-storage nodes

## Boot-disk fallback (emergency mode)

If no NVMe is assigned to the metadata role at boot, the runtime falls
back to `KISEKI_DATA_DIR` on the system disk (per the warning emitted
by ADR-030 amendment).

**Hard limit**: the runtime stops accepting inline writes (and emits
ERROR-level cluster warning) when:

- System disk used > 70% of total bytes, or
- Per-node metadata estimated > 50% of system partition

This caps the boot-disk fallback at ~half the system partition's free
bytes for inline. For a 256 GB c3 boot disk that's ~80 GB of inline +
metadata budget = ~9.4 M files cluster-wide at RF=3 on 50 nodes.

The fallback is for "running a perf benchmark on a non-production
cluster" or "single-node dev mode". Production runs MUST have a
dedicated metadata NVMe and see no fallback warning.

## Reading the metric

`kiseki-admin status` (post Phase 1 of the 2026-05-31 escalation rollout)
shows the live numbers:

```
$ kiseki-admin status
Cluster:
  nodes: 6
  metadata_pool: nvme-meta  (assigned: 6/6 nodes, 1.5 TB each)
  cluster_max_files (current threshold): 50 B
  cluster_used_files: 2.4 B (4.8%)

Per-node metadata budget:
  node-1: total=1.5 TB used=120 GB (8%)  inline_threshold=16 KB
  node-2: total=1.5 TB used=125 GB (8.3%) inline_threshold=16 KB
  ...

Cluster warnings: none
```

For nodes without a metadata-role device:

```
Cluster warnings:
  - WARN node-3: metadata tier on boot disk. Run
    `kiseki-admin pool add-device metadata-pool /dev/nvme0n1`
    on node-3 to dedicate fast storage. See ADR-030.
```

## References

- ADR-024 (2026-05-31 amendment): three-tier durability by size band
- ADR-030 (2026-05-31 amendment): admin-driven metadata role, 16 KB
  default inline threshold
- ADR-048: slab-EC compactor (steady-state storage overhead)
- Escalation 2026-05-31: motivating perf measurement + plan
