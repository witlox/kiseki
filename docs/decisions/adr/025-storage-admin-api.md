# ADR-025: Storage Administration API

**Status**: Proposed.
**Date**: 2026-04-20.
**Deciders**: Architect + domain expert.

## Context

Storage administrators need to performance-tune the system similar to
Ceph (`ceph osd pool set`), VAST (management UI), or Lustre (`lctl`).
The current control plane API handles tenant lifecycle but has **no
storage admin surface** — no pool management, device management,
performance tuning, or cluster-wide observability.

**API-first principle**: All admin interactions go through gRPC APIs.
CLI (`kiseki-cli`), Web UI, and job orchestrators (Ansible, Terraform)
are wrappers around these APIs. No SSH-and-edit-config path.

## Decision

### Admin API surface (new gRPC service)

```protobuf
service StorageAdminService {
  // === Device management ===
  rpc ListDevices(ListDevicesRequest) returns (ListDevicesResponse);
  rpc GetDevice(GetDeviceRequest) returns (DeviceInfo);
  rpc AddDevice(AddDeviceRequest) returns (AddDeviceResponse);
  rpc RemoveDevice(RemoveDeviceRequest) returns (RemoveDeviceResponse);
  rpc EvacuateDevice(EvacuateDeviceRequest) returns (EvacuateDeviceResponse);
  rpc CancelEvacuation(CancelEvacuationRequest) returns (CancelEvacuationResponse);

  // === Pool management ===
  rpc ListPools(ListPoolsRequest) returns (ListPoolsResponse);
  rpc GetPool(GetPoolRequest) returns (PoolInfo);
  rpc CreatePool(CreatePoolRequest) returns (CreatePoolResponse);
  rpc SetPoolDurability(SetPoolDurabilityRequest) returns (SetPoolDurabilityResponse);
  rpc SetPoolThresholds(SetPoolThresholdsRequest) returns (SetPoolThresholdsResponse);
  rpc RebalancePool(RebalancePoolRequest) returns (RebalancePoolResponse);

  // === Performance tuning ===
  rpc GetTuningParams(GetTuningParamsRequest) returns (TuningParams);
  rpc SetTuningParams(SetTuningParamsRequest) returns (SetTuningParamsResponse);

  // === Cluster observability ===
  rpc ClusterStatus(ClusterStatusRequest) returns (ClusterStatus);
  rpc PoolStatus(PoolStatusRequest) returns (PoolStatus);
  rpc DeviceHealth(DeviceHealthRequest) returns (stream DeviceHealthEvent);
  rpc IOStats(IOStatsRequest) returns (stream IOStatsEvent);

  // === Shard management ===
  rpc ListShards(ListShardsRequest) returns (ListShardsResponse);
  rpc GetShard(GetShardRequest) returns (ShardInfo);
  rpc SplitShard(SplitShardRequest) returns (SplitShardResponse);
  rpc SetShardMaintenance(SetShardMaintenanceRequest) returns (SetShardMaintenanceResponse);

  // === Repair and scrub ===
  rpc TriggerScrub(TriggerScrubRequest) returns (TriggerScrubResponse);
  rpc RepairChunk(RepairChunkRequest) returns (RepairChunkResponse);
  rpc ListRepairs(ListRepairsRequest) returns (ListRepairsResponse);
}
```

### Tuning parameters

Storage admins tune at four levels: **cluster → pool → tenant → workload**.
Lower levels inherit from higher, can only narrow (not broaden).

#### Cluster-wide tuning

| Parameter | Default | Range | What it controls |
|-----------|---------|-------|-----------------|
| `compaction_rate_mb_s` | 100 | 10-1000 | Background compaction throughput cap |
| `gc_interval_s` | 300 | 60-3600 | How often GC scans for reclaimable chunks |
| `rebalance_rate_mb_s` | 50 | 0-500 | Background rebalance/evacuation throughput |
| `scrub_interval_h` | 168 (7d) | 24-720 | How often integrity scrub runs |
| `max_concurrent_repairs` | 4 | 1-32 | Parallel EC repair jobs |
| `stream_proc_poll_ms` | 100 | 10-1000 | View materialization poll interval |
| `inline_threshold_bytes` | 4096 | 512-65536 | Below this, data inlined in delta |
| `raft_snapshot_interval` | 10000 | 1000-100000 | Entries between Raft snapshots |

#### Per-pool tuning

| Parameter | Default | Range | What it controls |
|-----------|---------|-------|-----------------|
| `ec_data_chunks` | 4 (NVMe) / 8 (HDD) | 2-16 | EC data fragment count |
| `ec_parity_chunks` | 2 (NVMe) / 3 (HDD) | 1-8 | EC parity fragment count |
| `replication_count` | 3 | 2-5 | For replication pools (not EC) |
| `warning_threshold_pct` | per ADR-024 | 50-95 | Capacity warning level |
| `critical_threshold_pct` | per ADR-024 | 60-98 | Capacity critical level |
| `readonly_threshold_pct` | per ADR-024 | 70-99 | Read-only level |
| `target_fill_pct` | 70 (SSD) / 80 (HDD) | 50-90 | Rebalance target fill level |
| `chunk_alignment_bytes` | 4096 | 512-65536 | On-disk alignment (RDMA/NVMe) |
| `prefer_sequential_alloc` | true | bool | Allocate sequentially in pool file |

#### Per-tenant tuning (via ControlService, existing)

| Parameter | Existing API | What it controls |
|-----------|-------------|-----------------|
| `quota.capacity_bytes` | SetQuota | Tenant capacity ceiling |
| `quota.iops` | SetQuota | IOPS limit |
| `quota.metadata_ops_per_sec` | SetQuota | Metadata op rate limit |
| `dedup_policy` | CreateOrganization | Cross-tenant vs isolated dedup |
| `compliance_tags` | SetComplianceTags | Regulatory constraints |

#### Per-workload tuning (via ControlService + Advisory)

| Parameter | API | What it controls |
|-----------|-----|-----------------|
| `workload.quota` | CreateWorkload | Workload-level capacity/IOPS |
| `advisory.hints_per_sec` | Advisory ceilings | Hint submission rate |
| `advisory.prefetch_bytes_max` | Advisory ceilings | Prefetch budget |
| `advisory.profile` | Advisory profiles | Allowed hint profiles |

### Observability API

#### ClusterStatus response

```protobuf
message ClusterStatus {
  uint32 node_count = 1;
  uint32 healthy_nodes = 2;
  uint64 total_capacity_bytes = 3;
  uint64 used_bytes = 4;
  uint32 pool_count = 5;
  uint32 shard_count = 6;
  uint32 active_repairs = 7;
  uint32 evacuating_devices = 8;
  repeated PoolSummary pools = 9;
}
```

#### PoolStatus response

```protobuf
message PoolStatus {
  string pool_id = 1;
  PoolHealth health = 2;
  uint64 capacity_bytes = 3;
  uint64 used_bytes = 4;
  uint32 device_count = 5;
  uint32 healthy_devices = 6;
  uint32 chunk_count = 7;
  // Performance metrics (rolling 60s window)
  double read_iops = 8;
  double write_iops = 9;
  double read_throughput_mb_s = 10;
  double write_throughput_mb_s = 11;
  double avg_read_latency_ms = 12;
  double avg_write_latency_ms = 13;
  double p99_read_latency_ms = 14;
  double p99_write_latency_ms = 15;
}
```

#### Streaming events

```protobuf
message DeviceHealthEvent {
  DeviceId device_id = 1;
  DeviceState old_state = 2;
  DeviceState new_state = 3;
  string reason = 4;
  uint64 timestamp_ms = 5;
}

message IOStatsEvent {
  string pool_id = 1;
  double read_iops = 2;
  double write_iops = 3;
  double read_throughput_mb_s = 4;
  double write_throughput_mb_s = 5;
  uint64 timestamp_ms = 6;
}
```

### Admin personas and API mapping

| Persona | Typical actions | APIs used |
|---------|----------------|-----------|
| **Cluster admin** | Add/remove nodes, set cluster params, view health | StorageAdminService (all), ClusterStatus |
| **Storage admin** | Create pools, tune EC, set thresholds, rebalance | Pool*, SetTuningParams, PoolStatus |
| **Tenant admin** | Set quotas, compliance, retention, advisory | ControlService (existing) |
| **Workload admin** | Tune advisory, prefetch, dedup hints | Advisory (existing) + workload quota |
| **On-call/SRE** | View health, trigger repair, check alerts | ClusterStatus, DeviceHealth stream, TriggerScrub |

### CLI mapping (kiseki-cli)

```
kiseki cluster status              → ClusterStatus
kiseki pool list                   → ListPools
kiseki pool status fast-nvme       → PoolStatus
kiseki pool create --name bulk-hdd --class HddBulk --ec 8+3
kiseki pool tune fast-nvme --warning-pct 75 --target-fill 70
kiseki device list                 → ListDevices
kiseki device add /dev/nvme2n1 --pool fast-nvme
kiseki device evacuate dev-uuid    → EvacuateDevice
kiseki device health --watch       → DeviceHealth stream
kiseki tune set --compaction-rate 200 --gc-interval 120
kiseki shard list                  → ListShards
kiseki shard split shard-uuid      → SplitShard
kiseki repair scrub --pool fast-nvme
kiseki iostat --pool fast-nvme     → IOStats stream
```

### Authorization model

| API | Who can call | Auth |
|-----|-------------|------|
| StorageAdminService (all) | Cluster admin only | mTLS cert with admin OU |
| ControlService (tenant ops) | Tenant admin | mTLS cert with tenant OU |
| Advisory (workload ops) | Workload identity | mTLS cert + workflow token |
| Read-only observability | Cluster admin, SRE | mTLS cert with admin/sre OU |

Tenant admins **cannot** access StorageAdminService. They see their
own quotas and compliance tags, not pool health or device state.
This preserves the zero-trust boundary (I-T4).

## Consequences

- Full API-first admin surface — no SSH-and-edit needed
- CLI, UI, automation all use the same gRPC APIs
- Performance tuning at four levels with inheritance
- Streaming observability for real-time monitoring
- Clear authorization boundary between cluster admin and tenant admin
- Significantly expands the gRPC surface (20+ new RPCs)

## References

- ADR-024: Device management and capacity thresholds
- ADR-005: EC and chunk durability
- ADR-020: Workflow advisory (workload-level tuning)
- Ceph: `ceph osd pool set` command reference
- Lustre: `lctl set_param` tunables
- I-T4: Zero-trust infra/tenant boundary

---

## Addendum: Adversarial Review Resolutions (2026-04-20)

### C1: Per-tenant resource usage → ControlService, not StorageAdminService

Per-tenant resource usage (capacity, IOPS attribution) is exposed via
**ControlService** with tenant-admin authorization, NOT via StorageAdminService.
Cluster admin sees pool-level aggregates only. Tenant admin sees their
own usage. This preserves I-T4.

```protobuf
// In ControlService (not StorageAdminService):
rpc GetTenantUsage(GetTenantUsageRequest) returns (TenantUsage);
// Requires tenant admin cert (mTLS OU = tenant ID)
```

### C2: Per-device I/O stats added

```protobuf
rpc DeviceIOStats(DeviceIOStatsRequest) returns (stream DeviceIOStatsEvent);

message DeviceIOStatsEvent {
  string device_id = 1;
  double read_iops = 2;
  double write_iops = 3;
  double read_latency_p50_ms = 4;
  double read_latency_p99_ms = 5;
  double errors_per_sec = 6;
  uint64 timestamp_ms = 7;
}
```

### C3: Shard health observability added

```protobuf
rpc GetShardHealth(GetShardHealthRequest) returns (ShardHealthInfo);

message ShardHealthInfo {
  string shard_id = 1;
  uint64 leader_node_id = 2;
  uint32 replica_count = 3;
  uint32 reachable_count = 4;
  uint32 recent_elections = 5;
  uint64 commit_lag_entries = 6;
}
```

### C4: EC parameters immutable per pool

**New invariant I-C6**: EC parameters (data_chunks, parity_chunks) are
immutable per pool. `SetPoolDurability` applies only to NEW chunks.
Existing chunks retain their original EC configuration. Explicit
re-encoding via `ReencodePool` RPC (long-running, cancellable).

### C5: Compaction rate validation

Protobuf-level validation: `compaction_rate_mb_s ∈ [10, 1000]`.
API rejects values outside range. Audit event on every change.

### C6: Inline threshold is prospective

**New invariant I-L9**: A delta's inlined payload is immutable after
write. `inline_threshold_bytes` changes do NOT retroactively affect
existing deltas. Old and new thresholds coexist in the log.

### C7: RemoveDevice requires evacuated state

**New invariant I-D5**: `RemoveDevice` rejects if device state is not
`Removed` (post-evacuation). Precondition: `EvacuateDevice` must
complete first. Error code: `DEVICE_NOT_EVACUATED`.

### C8: Pool modifications audited to affected tenants

**New invariant I-T4c**: Cluster admin modifications to pools containing
tenant data (SetPoolDurability, EvacuateDevice) are audit-logged to
the affected tenant's audit shard. Tenant admin can review.

### C9: Tuning change audit trail

**New invariant I-A6**: All tuning parameter changes via SetTuningParams
are recorded in the cluster audit shard with parameter name, old value,
new value, timestamp, and admin identity.

### H5: SRE roles defined

| Role | Access |
|------|--------|
| `cluster-admin` | Full StorageAdminService (read + write) |
| `sre-on-call` | Read-only: List*, Get*, Status, Health streams |
| `sre-incident-response` | SRE + TriggerScrub, RepairChunk |

Enforced via mTLS certificate OU field.

### M4: DrainNode added

```protobuf
rpc DrainNode(DrainNodeRequest) returns (stream DrainNodeProgress);
```

Internally evacuates all devices on the node, then removes them.
Idempotent, safe to retry.
