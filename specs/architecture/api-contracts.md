# API Contracts — Per-Context Interfaces

**Status**: Architect phase.
**Last updated**: 2026-04-25. Added ADR-033 (shard map, initial topology), ADR-034 (merge), ADR-035 (drain) operations.

Commands, events, and queries per bounded context. Traces to Gherkin
scenarios in specs/features/.

---

## Cross-language boundary (gRPC)

All communication is intra-Rust via trait implementations (Go removed per
ADR-027). gRPC is used for inter-process and network boundaries.

### Services exposed via gRPC

| Service | Provider | Consumer | Transport |
|---|---|---|---|
| `ControlService` | Rust control plane (kiseki-control) | Rust server, CLI | Management network |
| `AuditExportService` | Rust control plane | Tenant SIEM | Tenant VLAN |
| `KeyManagerService` | Rust keyserver (kiseki-keymanager) | Rust server | Internal network |
| `DiscoveryService` | Rust server | Rust client | Data fabric |
| `WorkflowAdvisoryService` | Rust server (kiseki-advisory) | Rust client (and any tenant-authorized caller) | Data fabric (separate listener, ADR-021 §1) |
| `LogService` | Rust server (kiseki-log) | Rust client, gateways | Data fabric |
| `StorageAdminService` | Rust server (kiseki-server) | Cluster admin, SRE | Management network (ADR-025) |

### Services that are intra-process (Rust trait calls)

Co-located callers use the trait directly; remote callers use the gRPC
service backed by the same trait implementation.

| Interface | Provider | Consumer | Sync/Async |
|---|---|---|---|
| `LogOps` | kiseki-log | kiseki-composition, kiseki-gateway (co-located) | **async** (ADR-032) |
| `ChunkOps` | kiseki-chunk | kiseki-composition, kiseki-view | sync |
| `CompositionOps` | kiseki-composition | kiseki-gateway-*, kiseki-client | **sync** (in-memory only; log emission handled by gateway — ADR-032) |
| `ViewOps` | kiseki-view | kiseki-gateway-*, kiseki-client |
| `CryptoOps` | kiseki-crypto | all crates that encrypt/decrypt |
| `KeyManagerOps` | kiseki-keymanager (remote) | kiseki-chunk, kiseki-crypto |
| `TenantKmsProvider` | kiseki-crypto (5 impls: Internal, Vault, KMIP, AWS KMS, PKCS#11 — ADR-028) | kiseki-gateway, kiseki-client, kiseki-view (via CryptoOps) |
| `DeviceBackend` | kiseki-block (2 impls: RawDevice, FileBacked — ADR-029) | kiseki-chunk (block-aligned I/O, bitmap allocation) |
| `AdvisoryLookup` | kiseki-advisory | kiseki-log, kiseki-chunk, kiseki-composition, kiseki-view, kiseki-gateway-* (wired by `kiseki-server`; bounded-deadline, non-blocking — ADR-021 §3) |

---

## Per-context API summary

### Log context

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| Command | `AppendDelta(shard, delta) → seq` | Composition | log.feature#SuccessfulDeltaAppend |
| Command | `SplitShard(shard, boundary) → new_shard` | System/Admin | log.feature#ShardSplitTriggered |
| Command | `CompactShard(shard, trigger)` | System/Admin | log.feature#AutomaticCompaction |
| Command | `TruncateLog(shard) → gc_boundary` | System/Admin | log.feature#DeltaGC |
| Command | `SetMaintenance(shard, enabled)` | Admin | log.feature#MaintenanceMode |
| Command | `CreateShard(shard_id, tenant, node, config)` | Control | ADR-033, ADR-036 |
| Command | `UpdateShardRange(shard_id, start, end)` | Control | ADR-033, ADR-036 |
| Command | `SetShardState(shard_id, state)` | Control | ADR-034, ADR-036 |
| Query | `ReadDeltas(shard, from, to) → [delta]` | View stream proc | log.feature#StreamProcessorReads |
| Query | `ShardHealth(shard) → ShardInfo` | Admin, Control | log.feature |
| Event | `DeltaCommitted(shard, seq)` | → View, Audit | log.feature#SuccessfulDeltaAppend |
| Command | `MergeShard(shard_a, shard_b) → merged_shard` | System/Admin | log.feature#ShardMerge, ADR-034 |
| Event | `ShardSplit(old, new, boundary)` | → Control, View, Client | log.feature#ShardSplitTriggered |
| Event | `ShardMerged(input_ids, merged_id, range, hlc)` | → Control, View, Client, Gateway | log.feature#ShardMerge, ADR-034 |
| Event | `MergeAborted(input_ids, reason)` | → Control | ADR-034 (ADV-034-2) |
| Event | `MaintenanceEntered(shard)` | → Gateway, Client | log.feature#MaintenanceMode |

### Chunk Storage context

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| Command | `WriteChunk(id, envelope, pool, tenant) → response` | Composition | chunk-storage.feature#WriteChunk |
| Command | `RepairChunk(id, trigger)` | System/Admin | chunk-storage.feature#DeviceFailure |
| Command | `SetRetentionHold(hold)` | Control | chunk-storage.feature#RetentionHold |
| Command | `ReleaseRetentionHold(hold_id)` | Control | chunk-storage.feature#ReleaseHold |
| Query | `ReadChunk(id) → envelope` | View, Gateway, Client | chunk-storage.feature#ReadChunk |
| Query | `ChunkHealth(id) → ChunkMeta` | Admin | chunk-storage.feature |
| Event | `ChunkStored(id, was_dedup)` | → Composition | chunk-storage.feature#WriteChunk |
| Event | `ChunkLost(id)` | → Composition, Control | chunk-storage.feature#ChunkUnrecoverable |
| Event | `DeviceFailure(device_id)` | → Chunk (internal) | chunk-storage.feature#DeviceFailure |

### Key Management context

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| Command | `FetchMasterKey(epoch) → master_key` | Server (startup/rotation) | key-management.feature#SystemDEK |
| Local | `DeriveSystemDek(chunk_id, epoch) → dek` | kiseki-server (local HKDF, no RPC) | ADR-003, ADV-ARCH-01 |
| Command | `RotateSystemKey() → new_epoch` | Admin | key-management.feature#SystemKEKRotation |
| Command | `RotateTenantKey(tenant) → new_epoch` | Tenant Admin | key-management.feature#EpochRotation |
| Command | `CryptoShred(tenant) → result` | Tenant Admin | key-management.feature#CryptoShred |
| Command | `FullReEncrypt(tenant, reason)` | Tenant Admin | key-management.feature#FullReEncrypt |
| Query | `FetchTenantKek(tenant, epoch) → kek` | Gateway, Client, View | key-management.feature#TenantKEKWrap |
| Query | `CheckKmsHealth(tenant) → bool` | Monitor | key-management.feature#KMSConnectivity |
| Query | `KeyManagerHealth() → state` | Admin | key-management.feature |
| Event | `KeyRotated(scope, old, new)` | → Audit | key-management.feature#AllEventsAudited |
| Event | `CryptoShredComplete(tenant, result)` | → Audit, Chunk, View | key-management.feature#CryptoShred |
| Event | `InvalidateTenantKey(tenant)` | → Gateway, Client, View | ADR-011 |

### Composition context

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| Command | `Create(namespace, tenant, chunks/inline) → id` | Gateway, Client | composition.feature#CreateComposition |
| Command | `Update(composition, mutations) → seq` | Gateway, Client | composition.feature#AppendData |
| Command | `Delete(composition, version_aware)` | Gateway, Client | composition.feature#DeleteComposition |
| Command | `StartMultipart(namespace, tenant) → upload` | Gateway, Client | composition.feature#S3MultipartUpload |
| Command | `FinalizeMultipart(upload_id, parts) → id` | Gateway, Client | composition.feature#S3MultipartUpload |
| Command | `AbortMultipart(upload_id)` | Gateway, Client | composition.feature#MultipartAborted |
| Query | `Get(composition, at_version?) → composition` | Gateway, Client | composition.feature |
| Query | `ListNamespace(namespace, prefix?) → [composition]` | Gateway, Client | composition.feature |
| Query | `ListVersions(composition) → [version]` | Gateway, Client | composition.feature |

### View Materialization context

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| Command | `CreateView(descriptor) → view_id` | Control/Tenant Admin | view-materialization.feature#CreateView |
| Command | `DiscardView(view_id)` | Admin | view-materialization.feature#DiscardView |
| Command | `RebuildView(view_id)` | Admin | view-materialization.feature#DiscardRebuild |
| Command | `UpdateDescriptor(view_id, new_desc)` | Tenant Admin | view-materialization.feature#DescriptorChange |
| Command | `AcquirePin(view_id, ttl) → pin` | Gateway, Client | view-materialization.feature#MVCCPin |
| Command | `ReleasePin(pin_id)` | Gateway, Client | view-materialization.feature#MVCCPin |
| Query | `ViewStatus(view_id) → state` | Admin | view-materialization.feature |
| Query | `ReadView(view_id, path/key) → data` | Gateway, Client | view-materialization.feature |

### Protocol Gateway context (NFS)

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| NFS | READ, WRITE, CREATE, REMOVE, RENAME, READDIR, OPEN, CLOSE, LOCK | NFS clients | protocol-gateway.feature, ADR-013 |

### Protocol Gateway context (S3)

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| S3 | PutObject, GetObject, DeleteObject, ListObjectsV2, multipart ops, versioning ops | S3 clients | protocol-gateway.feature, ADR-014 |

### Native Client context

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| FUSE | All supported POSIX ops (ADR-013) | Workload | native-client.feature |
| Native | kiseki_read, kiseki_write, kiseki_stat, etc. | Workload | native-client.feature#NativeAPIRead |

### Control Plane context (Go, gRPC)

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| Command | `CreateOrg / CreateProject / CreateWorkload` | Admin | control-plane.feature#TenantLifecycle |
| Command | `SetComplianceTags / SetQuota` | Admin | control-plane.feature#ComplianceTags |
| Command | `RequestAccess / ApproveAccess / DenyAccess` | Admin | control-plane.feature#IAM |
| Command | `CreateNamespace(ns, initial_shards?)` | Tenant Admin | control-plane.feature#NamespaceMgmt, ADR-033 |
| Query | `GetNamespaceShardMap(ns) → NamespaceShardMap` | Gateway, Client (tenant-authorized — ADV-033-9) | ADR-033 (I-L15) |
| Command | `DrainNode(node_id)` | Cluster Admin | multi-node-raft.feature#DrainNode, ADR-035 |
| Command | `CancelDrain(node_id)` | Cluster Admin | multi-node-raft.feature#DrainCancel, ADR-035 |
| Query | `ListNodes → [NodeRecord]` | Cluster Admin, SRE | ADR-035 |
| Query | `GetNodeStatus(node_id) → NodeRecord` | Cluster Admin, SRE | ADR-035 |
| Command | `RegisterFederationPeer` | Cluster Admin | control-plane.feature#Federation |
| Command | `SetMaintenanceMode` | Cluster Admin | control-plane.feature#Maintenance |
| Query | `ListFlavors / MatchFlavor` | Tenant Admin | control-plane.feature#FlavorMgmt |
| Query | `GetAuditExport(tenant) → stream` | Tenant Admin | control-plane.feature |
| Command | `SetAdvisoryPolicy(scope, profiles, budgets, state)` | Cluster/Tenant Admin | control-plane.feature#AdvisoryPolicy, ADR-021 §6 |
| Command | `TransitionAdvisoryState(scope, state)` | Cluster/Tenant Admin | control-plane.feature#AdvisoryOptOut, I-WA12 |
| Query | `GetEffectiveAdvisoryPolicy(workload) → policy` | kiseki-advisory | ADR-021 §6 (computed as min across cluster/org/project/workload) |

### Storage Admin context (Rust server, gRPC — ADR-025)

Service definition: `specs/architecture/proto/kiseki/v1/admin.proto` (planned).
Authorization: cluster admin only (mTLS cert with admin OU). SRE read-only role.

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| Query | `ListDevices / GetDevice` | Cluster admin, SRE | storage-admin.feature |
| Command | `AddDevice / RemoveDevice` | Cluster admin | storage-admin.feature, device-management.feature |
| Command | `EvacuateDevice / CancelEvacuation` | Cluster admin | device-management.feature |
| Query | `ListPools / GetPool / PoolStatus` | Cluster admin, SRE | storage-admin.feature |
| Command | `CreatePool / SetPoolDurability / SetPoolThresholds` | Cluster admin | storage-admin.feature |
| Command | `RebalancePool / CancelRebalance` | Cluster admin | storage-admin.feature |
| Command | `SetTuningParams / GetTuningParams` | Cluster admin | storage-admin.feature |
| Query | `ClusterStatus` | Cluster admin, SRE | storage-admin.feature |
| Stream | `DeviceHealth → stream DeviceHealthEvent` | Cluster admin, SRE | storage-admin.feature |
| Stream | `IOStats → stream IOStatsEvent` | Cluster admin, SRE | storage-admin.feature |
| Stream | `DeviceIOStats → stream DeviceIOStatsEvent` | Cluster admin, SRE | storage-admin.feature |
| Query | `ListShards / GetShard / GetShardHealth` | Cluster admin, SRE | storage-admin.feature |
| Command | `SplitShard / SetShardMaintenance` | Cluster admin | storage-admin.feature |
| Command | `TriggerScrub / RepairChunk / ListRepairs` | Cluster admin, SRE-IR | storage-admin.feature |
| Command | `DrainNode` | Cluster admin | storage-admin.feature, ADR-035 |
| Command | `CancelDrain` | Cluster admin | storage-admin.feature, ADR-035 |
| Query | `ListNodes / GetNodeStatus` | Cluster admin, SRE | ADR-035 |

### Workflow Advisory context (Rust server, gRPC)

Service definition: `specs/architecture/proto/kiseki/v1/advisory.proto`.

| Type | Operation | Caller | Spec reference |
|---|---|---|---|
| Command | `DeclareWorkflow(profile, initial_phase, ttl) → workflow_ref` | Native Client, any authorized caller | workflow-advisory.feature#DeclareWorkflow |
| Command | `EndWorkflow(workflow_ref)` | Caller that owns the workflow | workflow-advisory.feature#EndWorkflow |
| Command | `PhaseAdvance(workflow_ref, next_phase)` | Caller that owns the workflow | workflow-advisory.feature#PhaseAdvance, I-WA13 |
| Query | `GetWorkflowStatus(workflow_ref) → status` | Caller that owns the workflow | workflow-advisory.feature |
| Stream (bidi) | `AdvisoryStream` — hints in, telemetry out, multiplexed | Caller | workflow-advisory.feature, I-WA5..I-WA15 |
| Stream (server) | `SubscribeTelemetry(channels)` | Caller | workflow-advisory.feature |
| Event (internal) | `AdvisoryAuditEvent(declare/end/phase/hint/subscribe/budget/policy)` | → kiseki-audit (tenant shard) | I-WA8, ADR-021 §9 |

### OperationAdvisory on data-path operations

Every data-path `*Ops` trait method gains an optional advisory parameter
(typed as `Option<&OperationAdvisory>`; crate-level shared type from
`kiseki-common`). Methods treat the bundle as preferences only — any
behaviour they tune on advisory must be cleanly skippable when the
bundle is `None` or a given field is `None` (I-WA1, I-WA2).

| Trait | Method | Advisory fields consumed |
|---|---|---|
| `LogOps` | `ReadDeltas` | phase_id (heuristic compaction pacing only) |
| `ChunkOps` | `WriteChunk` | affinity, retention_intent, dedup_intent |
| `ChunkOps` | `ReadChunk` | access_pattern, priority (for QoS scheduling within policy) |
| `CompositionOps` | `FinalizeMultipart`, `Update` | retention_intent, collective_announcement side effect (via kiseki-advisory) |
| `ViewOps` | `ReadView` | access_pattern, phase_id (cache retention bias) |
| `ViewOps` | (internal prefetch path) | prefetch_tuples (pushed from kiseki-advisory) |

Hints are consumed via read-only access through `AdvisoryLookup`
(ADR-021 §3). A failed or timed-out lookup returns `None`, at which
point the data path proceeds with its no-advisory code path, which is
required to be byte-for-byte equivalent in outcome (I-WA1 property).

### `workflow_ref` header binding on data-path RPCs

The `workflow_ref` is carried as a **gRPC binary metadata entry**:

| Metadata key | `x-kiseki-workflow-ref-bin` |
|---|---|
| Value | 16 raw bytes (the `WorkflowRef` handle) |
| Scope | Every data-path RPC (chunk, composition, log, view, gateway) |
| Parsing | `kiseki-server` ingress interceptor lifts into request context |
| Proto impact | Zero — data-path protos are unchanged (ADR-021 §3.a) |
| Intra-Rust path | Same value via `CurrentAdvisory` task-local set by caller |

Any proposal to add a `workflow_ref` field to a data-path protobuf
message is rejected at architecture review — the header is the only
supported carrier, to preserve I-WA2 isolation.
