# ADR-024: Device Management, Storage Tiers, and Capacity Thresholds

**Status**: Proposed.
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

Each pool tracks capacity with threshold-based behavior:

| Pool usage | State | Behavior |
|-----------|-------|----------|
| 0-80% | **Healthy** | Normal writes, background rebalance |
| 80-90% | **Warning** | Log warning, emit telemetry, reduce rebalance target |
| 90-95% | **Critical** | Reject new chunk placements to this pool, advisory backpressure |
| 95-99% | **ReadOnly** | Existing writes drain, no new writes, alert cluster admin |
| 99-100% | **Full** | Return ENOSPC to clients, all writes rejected |

**Implementation**:
```rust
pub enum PoolHealth {
    Healthy,
    Warning { used_percent: u8 },
    Critical { used_percent: u8 },
    ReadOnly { used_percent: u8 },
    Full,
}

impl AffinityPool {
    pub fn health(&self) -> PoolHealth {
        let pct = (self.used_bytes * 100) / self.capacity_bytes;
        match pct {
            0..=80 => PoolHealth::Healthy,
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
- **Critical**: Reject new placements; redirect to sibling pools if available
- **ReadOnly**: In-flight writes complete; new writes fail with retriable error
- **Full**: ENOSPC — client gets permanent error

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
                ↗ Evacuating → Removed
```

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

## Consequences

- Device diversity now first-class (HDD, SSD, NVMe, PMem)
- Capacity management is explicit with defined thresholds
- System partition (RAID-1) separated from data (JBOD)
- Device health monitoring enables proactive replacement
- Tiering is future work; static placement for MVP
- Cluster admin must provision devices and assign to pools at setup time

## References

- ADR-005: EC and chunk durability (per pool)
- ADR-022: Storage backend (redb on system partition)
- Assumption A4: ClusterStor hardware
- Assumption A8: Reactive tiering
- Failure mode F-I2: Storage node failure
- Failure mode F-I4: Disk/device failure
