# Capacity Planning

Kiseki separates metadata and data onto different storage tiers. Proper
sizing of both tiers is critical for stable operation at scale.

---

## Storage tiers

Each storage node has two distinct storage tiers:

### System disk (metadata tier)

The system partition hosts:

- **Raft log** (`raft/log.redb`): Bounded by snapshot interval.
  Grows with write rate, compacted periodically.
- **Key epochs** (`keys/epochs.redb`): Tiny (<10 MB). One entry per
  key epoch.
- **Chunk metadata** (`chunks/meta.redb`): Scales linearly with file
  count. Approximately 80 bytes per file.
- **Inline content** (`small/objects.redb`): Variable. Controlled by
  the dynamic inline threshold (ADR-030).

**Requirements**:

- NVMe or SSD strongly recommended. HDD system disks trigger a boot
  warning because Raft fsync latency will be 5-10ms per commit.
- RAID-1 on 2x SSD for redundancy (the system disk is not protected
  by Kiseki's EC; it uses traditional RAID).
- Size based on expected file count and inline content.

### Data devices (data tier)

Data devices are JBOD-managed by Kiseki. They store chunk ciphertext
as extents on raw block devices (ADR-029).

**Requirements**:

- NVMe, SSD, or HDD depending on the pool's device class.
- Multiple devices per node for EC placement (I-D4: no two EC fragments
  on the same device).
- JBOD (no RAID): Kiseki manages durability via EC or replication.

---

## Metadata capacity sizing

### Per-file metadata footprint

| Component | Per file | Notes |
|-----------|----------|-------|
| Delta log entry | ~200 bytes | Raft log entry with header fields |
| Chunk metadata | ~80 bytes | Extent index entry in `chunks/meta.redb` |
| **Subtotal (no inline)** | **~280 bytes** | Fixed per file |
| Inline content | 0 to 64 KB | Only if file is below inline threshold |

### Capacity examples

**10 billion files, 50-node cluster, RF=3, no inline:**

| Component | Cluster total | Per node |
|-----------|--------------|----------|
| Delta log (metadata only) | ~2 TB | ~120 GB |
| Chunk metadata index | ~0.8 TB | ~48 GB |
| **Total metadata** | **~2.8 TB** | **~168 GB** |

At 168 GB per node, 256 GB NVMe system disks are tight. Larger system
disks (512 GB or 1 TB) provide comfortable headroom.

**10 billion files, 50-node cluster, RF=3, with inline (4 KB threshold):**

| Component | Cluster total | Per node |
|-----------|--------------|----------|
| Metadata (as above) | ~2.8 TB | ~168 GB |
| Inline content (10% of files < 4 KB, avg 2 KB) | ~2 TB | ~120 GB |
| **Total** | **~4.8 TB** | **~288 GB** |

This exceeds 256 GB system disks. The dynamic inline threshold
(ADR-030) prevents this by automatically reducing the threshold when
system disk usage approaches the soft limit.

### Capacity monitoring

The system automatically monitors metadata disk usage and adjusts:

| Usage level | Response |
|-------------|----------|
| Below `KISEKI_META_SOFT_LIMIT_PCT` (50%) | Normal operation |
| Above soft limit | Inline threshold reduced |
| Above `KISEKI_META_HARD_LIMIT_PCT` (75%) | Threshold forced to floor (128 B), alert emitted |

Alerts use out-of-band gRPC health reports (not Raft) so that a
full-disk node can signal without writing Raft entries (I-SF2).

---

## Dynamic inline threshold (ADR-030)

The inline threshold is computed per-shard as the minimum affordable
threshold across all Raft voters:

```
available = min(node.small_file_budget_bytes for node in shard.voters)
projected_files = shard.file_count_estimate
raw_threshold = available / max(projected_files, 1)
shard_threshold = clamp(raw_threshold, INLINE_FLOOR, INLINE_CEILING)
```

Where:

| Parameter | Value |
|-----------|-------|
| `INLINE_FLOOR` | 128 bytes (hard lower bound) |
| `INLINE_CEILING` | 64 KB (system-wide maximum) |
| `KISEKI_META_SOFT_LIMIT_PCT` | 50% (default) |
| `KISEKI_META_HARD_LIMIT_PCT` | 75% (default) |

### Threshold behavior

- **Decrease**: Automatic and safe. New files use the chunk path.
  Existing inline data is not retroactively migrated (I-L9).
- **Increase**: Requires cluster admin decision. May trigger optional
  background migration of small chunked files back to inline.
- **Emergency**: If any voter reports hard-limit breach, the leader
  commits a threshold reduction via Raft (2/3 majority; the full-disk
  node's vote is not required).

### Raft throughput guard (I-SF7)

The effective inline threshold is further clamped by a per-shard Raft
log throughput budget (`KISEKI_RAFT_INLINE_MBPS`, default 10 MB/s).
If the shard's inline write rate exceeds this budget, the threshold
temporarily drops to the floor until the rate subsides. This prevents
inline data from starving metadata-only Raft operations during write
storms.

---

## Pool capacity thresholds

Data-tier capacity is managed per pool. Thresholds vary by device class
to account for SSD/NVMe GC pressure at high fill levels (ADR-024):

| State | NVMe/SSD | HDD | Behavior |
|-------|----------|-----|----------|
| Healthy | 0-75% | 0-85% | Normal writes, background rebalance |
| Warning | 75-85% | 85-92% | Log warning, emit telemetry |
| Critical | 85-92% | 92-97% | Reject new placements, advisory backpressure |
| ReadOnly | 92-97% | 97-99% | In-flight writes drain, no new writes |
| Full | 97-100% | 99-100% | ENOSPC to clients |

### Why NVMe/SSD thresholds are lower

NVMe and SSD devices experience write amplification from garbage
collection at high fill levels. Above ~80% fill, GC pressure increases
sharply, causing:

- Increased write latency (10-100x during GC storms).
- Reduced effective write bandwidth.
- Accelerated wear.

Enterprise storage arrays (VAST, Pure) operate at 95%+ because they
have global wear leveling across all flash in the system. JBOD devices
do not have this capability, so Kiseki's thresholds are more
conservative.

---

## Growth estimation

### File count growth

Monitor `kiseki_shard_delta_count` to track delta (file) accumulation:

```bash
# Current delta count per shard
curl -s http://node1:9090/metrics | grep kiseki_shard_delta_count
```

Use the rate of delta count increase to project when the metadata tier
will reach capacity.

### Data volume growth

Monitor pool capacity metrics:

```bash
# Current pool utilization
curl -s http://node1:9090/metrics | grep kiseki_pool_capacity
```

### Projection formula

```
days_until_full = (capacity_total - capacity_used) / daily_write_rate
```

For metadata:

```
metadata_per_file = 280 bytes (no inline) or 280 + avg_inline_size (with inline)
days_until_full = (system_disk_capacity * soft_limit_pct - current_used) /
                  (new_files_per_day * metadata_per_file * replication_factor)
```

---

## Sizing recommendations

### Small deployment (development/testing)

| Component | Recommendation |
|-----------|---------------|
| Nodes | 3 (minimum for Raft) |
| System disk | 256 GB NVMe each (RAID-1 on 2x SSD) |
| Data devices | 2x 1 TB NVMe per node |
| Key manager | Co-located with storage nodes (internal KMS) |
| File count | Up to 100 million |

### Medium deployment (departmental HPC)

| Component | Recommendation |
|-----------|---------------|
| Nodes | 5-10 |
| System disk | 512 GB NVMe each (RAID-1) |
| Data devices | 4-8 NVMe per node (2-8 TB each) |
| Key manager | 3 dedicated nodes |
| File count | Up to 1 billion |

### Large deployment (institutional HPC/AI)

| Component | Recommendation |
|-----------|---------------|
| Nodes | 50-200 |
| System disk | 1 TB NVMe each (RAID-1) |
| Data devices | 8-24 devices per node, mixed tiers (NVMe + SSD + HDD) |
| Key manager | 5 dedicated nodes |
| File count | Up to 10 billion |
| Total capacity | 100 PB+ |

### Rules of thumb

- **System disk**: Size at 2x the expected metadata footprint for
  comfortable headroom. Include inline content estimates.
- **Data devices**: At least `ec_data_chunks + ec_parity_chunks`
  devices per pool (for EC placement across distinct devices, I-D4).
- **Network**: CXI or InfiniBand for clusters where storage bandwidth
  is critical. TCP is acceptable for cold-tier pools.
- **Memory**: At least 64 GB per storage node for Raft state, chunk
  metadata caching, and stream processor buffers.

---

## Capacity alerts

### Configuring alerts

Use Prometheus alerting rules (see
[Monitoring](../admin/monitoring.md)) to detect capacity issues before
they become critical:

```yaml
- alert: KisekiSystemDiskWarning
  expr: >
    node_filesystem_avail_bytes{mountpoint="/var/lib/kiseki"} /
    node_filesystem_size_bytes{mountpoint="/var/lib/kiseki"} < 0.50
  for: 10m
  labels:
    severity: warning
  annotations:
    summary: "System disk above 50% on {{ $labels.instance }}"

- alert: KisekiSystemDiskCritical
  expr: >
    node_filesystem_avail_bytes{mountpoint="/var/lib/kiseki"} /
    node_filesystem_size_bytes{mountpoint="/var/lib/kiseki"} < 0.25
  for: 5m
  labels:
    severity: critical
  annotations:
    summary: "System disk above 75% on {{ $labels.instance }}"
```

### When to add capacity

- **System disk above 50% (soft limit)**: Plan for capacity expansion.
  Inline threshold will start decreasing.
- **System disk above 75% (hard limit)**: Urgent. Inline threshold is
  at floor. Add nodes or upgrade system disks.
- **Pool above Warning threshold**: Monitor growth. Plan for device
  additions.
- **Pool above Critical threshold**: Writes are being rejected. Add
  devices immediately or evacuate data to another pool.
