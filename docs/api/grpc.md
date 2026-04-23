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

**Port**: Management network
**Provider**: `kiseki-server`
**Consumers**: Cluster admin, SRE (read-only role)

| RPC | Type | Description |
|---|---|---|
| `ClusterStatus` | Unary | Cluster-wide status summary |
| `ListDevices` / `GetDevice` | Unary | Query storage devices |
| `AddDevice` / `RemoveDevice` | Unary | Add or remove a device (removal requires Removed state) |
| `EvacuateDevice` / `CancelEvacuation` | Unary | Trigger or cancel device evacuation |
| `ListPools` / `GetPool` / `PoolStatus` | Unary | Query affinity pools |
| `CreatePool` / `SetPoolDurability` / `SetPoolThresholds` | Unary | Manage pool configuration |
| `RebalancePool` / `CancelRebalance` | Unary | Trigger or cancel pool rebalance |
| `ListShards` / `GetShard` / `GetShardHealth` | Unary | Query shard state |
| `SplitShard` / `SetShardMaintenance` | Unary | Shard management |
| `SetTuningParams` / `GetTuningParams` | Unary | Runtime tuning parameters |
| `DrainNode` | Unary | Drain all shards and chunks from a node |
| `TriggerScrub` / `RepairChunk` / `ListRepairs` | Unary | Data integrity operations |
| `DeviceHealth` | Server streaming | Live device health events |
| `IOStats` | Server streaming | Live I/O statistics |
| `DeviceIOStats` | Server streaming | Per-device I/O statistics |

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
