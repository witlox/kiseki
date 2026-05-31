# ADR-024: Device Management, Storage Tiers, and Capacity Thresholds

**Status**: Accepted (19/19 device-management BDD scenarios pass).
**Date**: 2026-04-20.
**Deciders**: Architect + domain expert.

## Context

The current design (ADR-005) defines three NVMe device classes but
does not address:
- HDD / spinning disk tiers (common in cost-optimized HPC clusters)
- System partition vs data partition separation
- Capacity thresholds and degradation behavior
- Device health monitoring and proactive replacement
- Memory-attached storage (CXL, persistent memory)
- Mixed-tier deployments (SSD+HDD, fast-SSD+cheap-SSD)

Real HPC deployments often have:
- **System partition**: RAID-1 (or RAID-1+0) on 2 SSDs for OS + Kiseki binaries + redb
- **Data partitions**: JBOD — each NVMe/SSD/HDD is an independent pool member
- **Tiering**: Hot data on fast NVMe, warm on cheap SSD, cold on HDD

## Decision

### Device classification

Extend `DeviceClass` to cover the full storage hierarchy:

| Class | Medium | Use case | Typical capacity |
|-------|--------|----------|-----------------|
| `NvmeU2` | NVMe U.2 TLC/MLC | Metadata, hot data, Raft log | 1-8 TB |
| `NvmeQlc` | NVMe QLC | Checkpoints, warm data | 4-30 TB |
| `NvmePersistentMemory` | Intel Optane / CXL | Cache, ultra-hot metadata | 128 GB - 1 TB |
| `SsdSata` | SATA SSD | Budget fast storage | 1-8 TB |
| `HddEnterprise` | SAS/SATA HDD 10k/15k | Cold data, archive | 4-20 TB |
| `HddBulk` | SATA HDD 7.2k | Deep archive, bulk cold | 10-20 TB |
| `Custom(String)` | User-defined | Vendor-specific | Varies |

### Server disk layout

```
Server node:
├── System partition (RAID-1 on 2× SSD)
│   ├── /boot, /root, OS
│   ├── /var/lib/kiseki/redb/       ← Raft log, metadata index
│   └── /var/lib/kiseki/config/     ← Node config, certs
│
├── Data devices (JBOD, managed by Kiseki)
│   ├── /dev/nvme0n1 → pool "fast-nvme"  (device member)
│   ├── /dev/nvme1n1 → pool "fast-nvme"  (device member)
│   ├── /dev/sda     → pool "bulk-ssd"   (device member)
│   ├── /dev/sdb     → pool "cold-hdd"   (device member)
│   └── ...
│
└── Optional: CXL memory → pool "pmem" (hot cache tier)
```

**JBOD for data, RAID-1 for system.** Kiseki manages data durability
via EC/replication across JBOD members. The system partition uses
traditional RAID-1 because redb and Raft log must survive single-disk
failure without Kiseki's own repair mechanism.

### Pool capacity management

### Per-device-class capacity thresholds

Thresholds vary by device type because NVMe/SSD suffer GC-induced write
amplification at high fill levels, while HDD does not. Enterprise
arrays (VAST, Pure) can operate at 95%+ because they have global wear
leveling — JBOD does not have that luxury.

| State | NVMe/SSD | HDD | Behavior |
|-------|----------|-----|----------|
| **Healthy** | 0-75% | 0-85% | Normal writes, background rebalance |
| **Warning** | 75-85% | 85-92% | Log warning, emit telemetry |
| **Critical** | 85-92% | 92-97% | Reject new placements, advisory backpressure |
| **ReadOnly** | 92-97% | 97-99% | In-flight writes drain, no new writes |
| **Full** | 97-100% | 99-100% | ENOSPC to clients |

**Rationale**: NVMe/SSD GC pressure increases sharply above ~80% fill.
QLC is worse than TLC. The SSD Warning threshold (75%) gives the
placement engine time to redirect before the GC cliff. HDD has no
such cliff — outer-track vs inner-track difference is ~20%, not
a performance wall.

**Implementation**:
```rust
pub enum PoolHealth {
    Healthy,
    Warning { used_percent: u8 },
    Critical { used_percent: u8 },
    ReadOnly { used_percent: u8 },
    Full,
}

pub struct CapacityThresholds {
    pub warning_pct: u8,
    pub critical_pct: u8,
    pub readonly_pct: u8,
    pub full_pct: u8,
}

impl CapacityThresholds {
    pub fn for_device_class(class: &DeviceClass) -> Self {
        match class {
            DeviceClass::NvmeU2 | DeviceClass::NvmeQlc
            | DeviceClass::NvmePersistentMemory | DeviceClass::SsdSata => Self {
                warning_pct: 75,
                critical_pct: 85,
                readonly_pct: 92,
                full_pct: 97,
            },
            DeviceClass::HddEnterprise | DeviceClass::HddBulk => Self {
                warning_pct: 85,
                critical_pct: 92,
                readonly_pct: 97,
                full_pct: 99,
            },
            DeviceClass::Custom(_) => Self {
                warning_pct: 80,
                critical_pct: 90,
                readonly_pct: 95,
                full_pct: 99,
            },
        }
    }
}

impl AffinityPool {
    pub fn health(&self) -> PoolHealth {
        let pct = (self.used_bytes * 100) / self.capacity_bytes;
            81..=90 => PoolHealth::Warning { used_percent: pct as u8 },
            91..=95 => PoolHealth::Critical { used_percent: pct as u8 },
            96..=99 => PoolHealth::ReadOnly { used_percent: pct as u8 },
            _ => PoolHealth::Full,
        }
    }
}
```

**Placement engine behavior**:
- **Healthy**: Place chunks according to affinity policy
- **Warning**: Continue placing but emit telemetry; cluster admin should add capacity
- **Critical**: Reject new placements; redirect to **same device-class sibling** only
- **ReadOnly**: In-flight writes complete; new writes fail with retriable error
- **Full**: ENOSPC — client gets permanent error

**Pool redirection policy**: When a pool is Critical, the placement
engine redirects to another pool of the **same device class** only.
Never cross device-class boundaries (e.g., never NVMe → HDD).
If no same-class sibling has capacity, return ENOSPC to client.
This preserves performance SLAs and compliance tag enforcement.

### System partition

**OS-managed RAID-1** on 2× SSD. Kiseki does not manage the RAID.

Kiseki monitors system partition health:
1. On startup: check `/proc/mdstat` for RAID health
2. If degraded → log WARNING, continue operating
3. If both drives failed → log CRITICAL, refuse to start
4. Periodic check every 60 seconds

Admin is responsible for replacing failed system drives and
rebuilding the RAID. Kiseki trusts the OS for system partition
durability.

### Device health monitoring

Each device reports SMART/health metrics:

| Metric | Threshold | Action |
|--------|----------|--------|
| Temperature | >70°C | Warning; throttle if >80°C |
| Wear level (SSD) | >90% life used | Warning; proactive replacement window |
| Bad sectors (HDD) | >0 reallocated | Warning at 1; evacuate at >100 |
| Latency | >10× baseline | Mark degraded; reduce placement priority |
| Errors | Uncorrectable read | Mark suspect; verify EC/replicas for affected chunks |

**Device states**:
```
Healthy → Degraded → Failed → Removed
     ↘       ↗
   Evacuating → Removed
```

### Eviction and evacuation policy

**Key principle**: Unhealthy devices are evacuated proactively, not
waited on until failure. Full devices are write-blocked, not evicted
(data is still readable).

| Trigger | Action | Automatic? | Priority |
|---------|--------|-----------|----------|
| SMART wear >90% (SSD) | **Evacuate** — migrate chunks to other pool members | Yes (background) | Normal |
| Bad sectors >100 (HDD) | **Evacuate** — migrate before cascading failure | Yes (background) | High |
| Uncorrectable read error | **Evacuate + EC repair** for affected chunks | Yes (immediate) | Critical |
| Temperature >80°C | **Throttle** I/O, alert admin | Yes | High |
| Device unresponsive | **Mark Failed** — trigger EC repair from survivors | Yes (immediate) | Critical |
| Pool at Critical threshold | **Block writes** — redirect to sibling pools | Yes | Normal |
| Pool at ReadOnly threshold | **Drain writes** — no new data, existing completes | Yes | Normal |
| Admin-initiated | **Evacuate** — controlled migration before physical removal | Manual | Normal |

**Evacuation process**:
1. Mark device `Evacuating`
2. For each chunk on device: read fragment, write to another healthy device in pool
3. Update chunk metadata (redb) with new placement
4. When all chunks migrated: mark device `Removed`
5. Admin can physically pull the device

**Evacuation speed**: Bounded by network and destination device throughput.
At 1 GB/s NVMe write speed, a 4TB device evacuates in ~67 minutes.
EC repair (from parity) is faster since only the missing fragments
need reconstruction.

**Invariant**: A device in `Evacuating` state accepts no new writes
but serves reads for chunks not yet migrated.

### Storage backend per JBOD device

| Approach | Pros | Cons | Recommendation |
|----------|------|------|----------------|
| **Raw block** (ADR-029) | Zero FS overhead, direct I/O, aligned writes, bitmap allocator with redb journal | Custom allocator in `kiseki-block` | **Default** — recommended for production |
| **File-backed** (ADR-029) | Same `DeviceBackend` trait, works in VMs/CI without raw devices | Slight overhead from host FS | VMs and CI environments |
| **xfs** | Scales to 100M+ files, good NVMe support | Extra FS overhead, inode pressure at scale | Legacy / deprecated |

**Default**: Raw block device I/O via `kiseki-block` (`DeviceBackend`
trait with auto-detection of device characteristics). File-backed
fallback for VMs and CI. XFS is deprecated as a chunk storage backend;
existing XFS deployments can migrate via background evacuation.

### Device discovery

**Manual configuration** (MVP):
- Admin provides device list in node config (`kiseki-server.toml`)
- Each device: path, class, pool assignment

**Future: Auto-discovery**:
- Scan `/sys/block/` for NVMe/SSD/HDD devices
- Classify by transport (NVMe, SATA, SAS) and media (rotational flag)
- Present to admin for pool assignment confirmation

- **Healthy**: Normal I/O
- **Degraded**: Elevated errors or latency; reduce write priority
- **Evacuating**: Admin-initiated; migrate chunks to other devices, then remove
- **Failed**: I/O errors; trigger EC repair for all chunks
- **Removed**: Device physically absent; metadata cleaned up

### Tiering and data movement

**Static placement** (MVP): Admin assigns pools to device classes.
Chunk placement is determined at write time by the composition's
view descriptor affinity policy. No automatic migration.

**Future: Reactive tiering** (per assumption A8):
- Compositions with high read frequency auto-promote from cold → hot
- Compositions with no reads for >N days auto-demote from hot → cold
- Promotion/demotion as background job (copy chunk, update metadata, delete old)
- Bounded by pool capacity thresholds (don't overfill hot tier)

### Data model changes

```rust
pub enum DeviceClass {
    NvmeU2,
    NvmeQlc,
    NvmePersistentMemory,
    SsdSata,
    HddEnterprise,
    HddBulk,
    Custom(String),
}

pub struct DeviceInfo {
    pub id: DeviceId,
    pub class: DeviceClass,
    pub path: String,          // /dev/nvme0n1 or mount point
    pub capacity_bytes: u64,
    pub used_bytes: u64,
    pub state: DeviceState,
    pub pool_id: Option<String>,
}

pub enum DeviceState {
    Healthy,
    Degraded { reason: String },
    Evacuating { progress_percent: u8 },
    Failed { since: u64 },
    Removed,
}
```

## Amendment (2026-05-31) — Three-tier durability by size band

**Status of amendment**: Accepted.
**Trigger**: `specs/escalations/2026-05-31-tiered-storage-three-tier-and-slab-ec.md`.
**Crosses**: ADR-030 (inline path), ADR-045 (namespace tier-policy), ADR-047
(intent log as durable hot tier), new ADR-048 (slab-EC compactor).

### Motivation

The original ADR specified per-pool `DurabilityStrategy` (Replication N
or ErasureCoding M+K) and let the runtime pick a pool at write time.
Production runtime never wires the size-aware selector
([`select_pool_for_write`](../../../crates/kiseki-chunk/src/pool.rs)); every PUT
routes to a hardcoded `"default"` pool. Measurement on 2026-05-30 GCP
n=6 default profile: native put-heavy at 2.8 k op/s, ~2 % of ADR-042
§14 cluster-aggregate target. The 50× gap traces to applying the EC-4+2
strategy's 5-fragment fan-out to every PUT, including 16 KiB / 64 KiB
small-object writes where storage saving doesn't justify the per-RPC tax.

EC-4+2 is mathematically RAID 6 with 4 data drives + 2 parity, distributed
across nodes for failure-domain isolation. The Reed-Solomon code is the
same. Its only value is **storage efficiency on large objects** where the
per-PUT fan amortises over many bytes. For small/medium objects the
1+5 fan-out fixed cost dominates and replication (1+(N-1) fan) wins on
both latency and throughput.

### Decision — explicit size bands

Every write picks a pool from one of three durability strategies, by
encrypted-payload size:

| size band | durability strategy | per-PUT fan | rationale |
|---|---|---:|---|
| `0 ≤ size ≤ inline_threshold` | inline-in-Raft (no chunk store) | 1+2 (R-3 Raft intent) | latency-critical; capacity-bounded; no chunk fan |
| `inline_threshold < size ≤ replication_ceiling` | `Replication { factor: N }` | 1+(N-1) | per-PUT fan cost dominates byte cost; storage saving doesn't pay |
| `size > replication_ceiling` | `ErasureCoding { data, parity }` | 1+(data+parity−1) | RTT amortises over bytes; storage saving real |

**Defaults**:

| parameter | default | tunable via |
|---|---|---|
| `inline_threshold` | 16 KB | per-pool config, ADR-030 §3 formula |
| `inline_ceiling` (hard cap) | 64 KB | per-pool config |
| `replication_ceiling` | 4 MB | per-pool config |
| Default replicated `factor` | 3 | per-pool config |
| Default EC `data + parity` | 4 + 2 | per-pool config |

The bench-namespace small-object workload that drives the 2026-05-30
measurement falls in band 2 (replication path) under these defaults at
64 KiB. The inline path engages for ≤ 16 KB workloads (xattrs, symlinks,
small JSON, headers); the EC path engages for ≥ 4 MB workloads (bulk
data, archival, ML training shards).

### Implementation contract

The runtime MUST:

1. Maintain at least one pool per size band that the cluster's
   declared namespaces use (or a global fallback chain). A namespace
   without a `tier_policy` inherits the cluster default chain
   `[inline, replicated, ec]` resolved against the highest-class
   pools available.
2. Route each PUT to a pool by calling `select_pool_for_write(pools,
   size, namespace.tier_policy)`. The hardcoded `"default"` string
   lookup is **deprecated** and the gateway hot path stops emitting
   it after this amendment lands.
3. Surface the active size-band → pool routing in
   `kiseki-admin namespace describe <ns>` so operators see exactly
   which pool will receive a write of a given size.

### Read-path implication

Reads are unchanged: the composition's `chunk_refs` carry pool +
chunk_id (or pool + slab_id + offset post-ADR-048). The gateway's
read path consults the chunk's source pool to know whether to read
inline (`small/objects.redb`), via replication ladder, or via EC
reconstruction.

### Migration

This is **pre-production**. No deployed clusters; no migration owed.
Existing test fixtures that hardcode `pool: "default"` are updated in
the Phase 3 wire-up.

### What this does NOT change

- The pool model itself (`AffinityPool`, `DurabilityStrategy`,
  `DeviceClass`) is unchanged — already supports R-N and EC variants.
- Per-class capacity thresholds (Healthy / Warning / Critical) are
  unchanged.
- EC-4+2 stays as the default for the cold tier (band 3 above
  `replication_ceiling`).
- ADR-047's intent log durability semantics are unchanged; the
  inline band rides on top of the existing per-shard Raft.

## Consequences

- Device diversity now first-class (HDD, SSD, NVMe, PMem)
- Capacity management is explicit with defined thresholds
- System partition (RAID-1) separated from data (JBOD)
- Device health monitoring enables proactive replacement
- Tiering is future work; static placement for MVP
- Cluster admin must provision devices and assign to pools at setup time
- **(2026-05-31 amendment)** Three durability strategies routed by size
  band. Default chain delivers inline / replicated / EC for the three
  workload regimes. Per-PUT EC fan-out tax limited to objects ≥
  `replication_ceiling` (default 4 MB).

## References

- ADR-005: EC and chunk durability (per pool)
- ADR-022: Storage backend (redb on system partition)
- ADR-030: Inline path for the small-object band
- ADR-045: Namespace tier-policy declares per-size-band pool choice
- ADR-047: Intent log durability — the substrate the inline band rides on
- ADR-048: Slab-EC compactor — amortises EC fan-out across many writes
- Assumption A4: ClusterStor hardware
- Assumption A8: Reactive tiering
- Failure mode F-I2: Storage node failure
- Failure mode F-I4: Disk/device failure
- Escalation 2026-05-31: tiered storage + slab-EC
