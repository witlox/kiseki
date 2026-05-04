# gRPC Services

Kiseki exposes several gRPC services across two network ports. Data-path
services run on port 9100. The advisory service runs on a separate
listener at port 9101 (isolated runtime, ADR-021).

---

## LogService

**Port**: 9100 (data fabric)
**Provider**: `kiseki-log` (via `kiseki-server`)
**Consumers**: Composition, View stream processors, Gateway, Client

| RPC | Type | Description |
|---|---|---|
| `AppendDelta` | Unary | Append a delta to a shard. Returns the assigned sequence number. Commits via Raft majority before ack (I-L2). |
| `ReadDeltas` | Server streaming | Read a range of deltas from a shard. Used by view stream processors for materialization. |
| `TruncateLog` | Unary | Trigger delta GC up to the minimum consumer watermark. Returns the new GC boundary. |
| `ShardHealth` | Unary | Query shard health, Raft state, and replication status. |
| `SplitShard` | Unary | Trigger mandatory shard split at a given boundary. |
| `SetMaintenance` | Unary | Enable or disable maintenance mode on a shard (I-O6). |
| `CompactShard` | Unary | Trigger compaction (header-only merge, I-O2). |

---

## KeyManagerService

**Port**: Internal network (dedicated key manager cluster)
**Provider**: `kiseki-keymanager` (via `kiseki-keyserver`)
**Consumers**: Storage nodes (chunk encryption), Gateway, Client

| RPC | Type | Description |
|---|---|---|
| `FetchMasterKey` | Unary | Fetch the master key for a given epoch. Used at node startup and rotation. |
| `RotateKey` | Unary | Rotate system or tenant keys. Creates a new epoch. |
| `CryptoShred` | Unary | Destroy tenant KEK, rendering all tenant data unreadable. |
| `FullReEncrypt` | Unary | Trigger full re-encryption of a tenant's data under new keys. |
| `FetchTenantKek` | Unary | Fetch tenant KEK for wrapping/unwrapping operations. |
| `CheckKmsHealth` | Unary | Check tenant KMS provider connectivity. |
| `KeyManagerHealth` | Unary | Query key manager cluster health and Raft state. |

System DEK derivation is local (HKDF, no RPC). Only master key fetch and
tenant KEK operations require network calls (ADR-003).

---

## ControlService

**Port**: Management network
**Provider**: `kiseki-control`
**Consumers**: Admin CLI, storage nodes, advisory runtime

### Tenant management

| RPC | Description |
|---|---|
| `CreateOrg` | Create a new organization (top-level tenant) |
| `CreateProject` | Create a project within an organization |
| `CreateWorkload` | Create a workload within an org or project |
| `DeleteOrg` / `DeleteProject` / `DeleteWorkload` | Remove tenant hierarchy nodes |

### Namespace and policy

| RPC | Description |
|---|---|
| `CreateNamespace` | Create a tenant-scoped namespace |
| `SetComplianceTags` | Set compliance regime tags (inherit downward) |
| `SetQuota` | Set resource quotas at org/project/workload level |
| `SetRetentionHold` | Create a retention hold on a namespace or composition |
| `ReleaseRetentionHold` | Release an active retention hold |

### IAM

| RPC | Description |
|---|---|
| `RequestAccess` | Cluster admin requests access to tenant data |
| `ApproveAccess` | Tenant admin approves access request |
| `DenyAccess` | Tenant admin denies access request |

### Operations

| RPC | Description |
|---|---|
| `SetMaintenanceMode` | Enable/disable cluster-wide maintenance mode |
| `ListFlavors` / `MatchFlavor` | Query and match deployment flavors |

### Federation

| RPC | Description |
|---|---|
| `RegisterFederationPeer` | Register a remote Kiseki cluster for async replication |

### Advisory policy

| RPC | Description |
|---|---|
| `SetAdvisoryPolicy` | Configure profiles, budgets, and state per scope |
| `TransitionAdvisoryState` | Transition advisory state (enabled/draining/disabled) |
| `GetEffectiveAdvisoryPolicy` | Compute effective policy for a workload (min across hierarchy) |

---

## WorkflowAdvisoryService

**Port**: 9101 (data fabric, separate listener)
**Provider**: `kiseki-advisory` (via `kiseki-server`, isolated tokio runtime)
**Consumers**: Native client, any authorized tenant caller

| RPC | Type | Description |
|---|---|---|
| `DeclareWorkflow` | Unary | Declare a new workflow with profile, initial phase, and TTL. Returns a `WorkflowRef` handle and authorized pool handles. |
| `EndWorkflow` | Unary | End a declared workflow. Triggers audit summary and GC of workflow state. |
| `PhaseAdvance` | Unary | Advance to the next phase. Phase order is monotonic (I-WA13). |
| `GetWorkflowStatus` | Unary | Query current workflow state, phase, and budget usage. |
| `AdvisoryStream` | Bidirectional streaming | Multiplexed channel: hints in (client to storage), telemetry out (storage to client). |
| `SubscribeTelemetry` | Server streaming | Subscribe to specific telemetry channels for a workflow. |

### Advisory stream message types

**Inbound hints** (client to storage):

- Access pattern declaration
- Prefetch range (up to 4096 tuples per hint, I-WA16)
- Affinity pool preference (via opaque pool handles, I-WA19)
- Priority class (within policy-allowed maximum)
- Retention intent
- Dedup intent
- Collective checkpoint announcement
- Deadline hint

**Outbound telemetry** (storage to client):

- Backpressure signal (ok / soft / hard severity with retry-after)
- Placement locality class (local-node / local-rack / same-pool / remote / degraded)
- Materialization lag
- Prefetch effectiveness
- QoS headroom
- Hotspot detection (caller-owned compositions only)

---

## StorageAdminService (ADR-025)

**Port**: Same as the data-path gRPC server (default `:50051`)
**Provider**: `kiseki-server`
**Consumers**: Cluster admin via `kiseki-storage` CLI; programmatic admin tooling
**Status**: 26 of 26 RPCs implemented end-to-end (W2-W7 landed 2026-05-03)

| RPC | Type | Description |
|---|---|---|
| `ClusterStatus` | Unary | Cluster-wide status summary (node/shard/pool counts, total/used capacity, leader) |
| `ListDevices` / `GetDevice` | Unary | Query storage devices |
| `AddDevice` / `RemoveDevice` | Unary | Add a device to a pool / remove a device from its pool |
| `EvacuateDevice` / `CancelEvacuation` | Unary | Trigger or cancel device evacuation |
| `ListPools` / `GetPool` / `PoolStatus` | Unary | Query affinity pools (PoolStatus also returns capacity_state ∈ ok/warning/critical/readonly) |
| `CreatePool` / `SetPoolDurability` | Unary | Pool lifecycle (durability change rejected when pool is non-empty) |
| `SetPoolThresholds` | Unary | Per-pool capacity thresholds; ADR-024 defaults when unset |
| `RebalancePool` | Unary | Trigger pool rebalance; returns rebalance_id |
| `ListShards` / `GetShard` | Unary | Query shard state (members, leader, last_applied) |
| `SplitShard` / `MergeShards` | Unary | Shard split/merge per ADR-033/034 |
| `SetShardMaintenance` | Unary | Per-shard atomic flag — gates writes (`PutFragment` returns `FailedPrecondition`); reads stay served |
| `GetTuningParams` / `SetTuningParams` | Unary | Cluster-wide tuning (8 parameters; persisted to redb when KISEKI_DATA_DIR is set) |
| `TriggerScrub` / `RepairChunk` / `ListRepairs` | Unary | On-demand integrity ops + history ring (4096 records) |
| `DeviceHealth` | Server streaming | Live device-state-transition events (broadcast(1024)) |
| `IOStats` | Server streaming | Periodic I/O stats samples (broadcast(1024)) |

**Observability**: every RPC emits `StorageAdminService.<RpcName>` OTEL span and bumps `kiseki_storage_admin_calls_total{rpc, outcome}` (outcome ∈ ok / client_error / server_error / unimplemented).

**Cluster scope**: W4/W5 mutations are *node-local* today (`committed_at_log_index = 0` in responses). Multi-node Raft replication of these mutations via the cluster control shard's delta enum is documented as a follow-up in `specs/implementation/adr-025-storage-admin-api.md`.

**Producer wiring (W7 streams)**: the `DeviceHealth` and `IOStats` channels exist and admin clients can subscribe today, but the data-path producers (chunk-store device-state observer, chunk-cluster periodic IOStats sampler) land in follow-on PRs. Until then the streams hold open subscriptions but emit nothing.

---

## DiscoveryService

**Port**: 9100 (data fabric)
**Provider**: `kiseki-server`
**Consumers**: Native client

Used by the native client to discover shards, views, and gateways from
the data fabric without requiring direct control plane access (I-O4,
ADR-008).

---

## Protocol binding

- **Protobuf definitions**: `proto/kiseki/v1/*.proto`
- **Generated code**: `kiseki-proto` crate
- **Workflow ref header**: `x-kiseki-workflow-ref-bin` (16 raw bytes as
  gRPC binary metadata, not a proto field, per ADR-021)
