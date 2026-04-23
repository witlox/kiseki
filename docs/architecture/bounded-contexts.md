# Bounded Contexts

Eight bounded contexts form the core domain model. Each has a distinct
responsibility, failure domain, and scaling concern. This page describes
each context's purpose, implementing crate, key types, and governing
invariants.

---

## 1. Log

**Crate**: `kiseki-log`

**Purpose**: Accept deltas, assign them a total order within a shard,
replicate via Raft, persist durably, and support range reads for view
materialization and replay.

**Key types**: `Delta`, `DeltaEnvelope`, `Shard`, `ShardConfig`, `ShardInfo`

**Key invariants**:

| ID | Rule |
|---|---|
| I-L1 | Within a shard, deltas have a total order |
| I-L2 | A committed delta is durable on a majority of Raft replicas before ack |
| I-L3 | A delta is immutable once committed |
| I-L4 | Delta GC requires ALL consumers (views + audit) to have advanced past the delta |
| I-L5 | A composition is not visible until all referenced chunks are durable |
| I-L6 | Shards have a hard ceiling triggering mandatory split (delta count, byte size, or throughput) |
| I-L7 | Delta envelope has separated system-visible header and tenant-encrypted payload |
| I-L8 | Cross-shard rename returns EXDEV (no 2PC across shards) |
| I-L9 | A delta's inlined payload is immutable after write; threshold changes apply prospectively |

**Failure domain**: Per-shard. Leader loss causes transient latency
(election). Quorum loss makes the shard unavailable.

---

## 2. Chunk Storage

**Crate**: `kiseki-chunk` (with `kiseki-block` for device I/O)

**Purpose**: Store and retrieve opaque encrypted chunks. Manage placement
across affinity pools. Handle erasure coding and replication. Run GC based
on refcounts and retention holds.

**Key types**: `Chunk`, `ChunkId`, `Envelope`, `AffinityPool`, `DeviceBackend`

**Key invariants**:

| ID | Rule |
|---|---|
| I-C1 | Chunks are immutable; new versions are new chunks |
| I-C2 | A chunk is not GC'd while any composition references it (refcount > 0) |
| I-C2b | A chunk is not GC'd while a retention hold is active |
| I-C3 | Chunks are placed according to affinity policy from the referencing view descriptor |
| I-C4 | Durability strategy is per affinity pool (EC default, N-copy replication available) |
| I-C5 | Pool writes rejected at Critical threshold (SSD 85%, HDD 92%); ENOSPC at Full |
| I-C6 | EC parameters are immutable per pool; `SetPoolDurability` applies to new chunks only |
| I-C7 | All chunk data writes are aligned to device physical block size (ADR-029) |
| I-C8 | Allocation bitmap is ground truth; free-list is a derived cache rebuilt on startup |

**Failure domain**: Per-chunk or per-device. Chunk loss recoverable via
EC parity or replicas.

---

## 3. Composition

**Crate**: `kiseki-composition`

**Purpose**: Maintain tenant-scoped metadata structures describing how
chunks assemble into data units (files, objects). Manage namespaces.
Record mutations as deltas in the log.

**Key types**: `Composition`, `Namespace`, `CompositionMutation`

**Key invariants**:

| ID | Rule |
|---|---|
| I-X1 | A composition belongs to exactly one tenant |
| I-X2 | A composition's chunks respect the tenant's dedup policy (global hash or per-tenant HMAC) |
| I-X3 | A composition's mutation history is fully reconstructible from its shard's deltas |

**Failure domain**: Coupled to Log. If a shard fails, its compositions
are affected.

---

## 4. View Materialization

**Crate**: `kiseki-view`

**Purpose**: Consume deltas from shards and maintain materialized views
per view descriptor. Handle view lifecycle (create, discard, rebuild)
and MVCC read pins.

**Key types**: `View`, `ViewDescriptor`, `StreamProcessor`, `MvccPin`

**Key invariants**:

| ID | Rule |
|---|---|
| I-V1 | A view is derivable from its source shard(s) alone (rebuildable-from-log) |
| I-V2 | A view's observed state is a consistent prefix of its source log(s) up to a watermark |
| I-V3 | Cross-view consistency governed by the reading protocol's declared consistency model |
| I-V4 | MVCC read pins have bounded lifetime; pin expiration revokes the snapshot guarantee |

**Failure domain**: Per-view. A fallen-behind view serves stale data.
A lost view can be rebuilt from the log.

---

## 5. Protocol Gateway

**Crate**: `kiseki-gateway`

**Purpose**: Translate wire protocol requests (NFS, S3) into operations
against views and the log. Serve reads from views. Route writes as deltas
to the log via composition. Perform tenant-layer encryption for
protocol-path clients.

**Key types**: Protocol gateway instance, protocol plugin

**Trust boundary**: NFS/S3 clients send plaintext over TLS to the gateway.
The gateway encrypts before writing to log/chunks. Plaintext exists in
gateway memory only ephemerally.

**Failure domain**: Per-gateway. Crash disconnects affected clients.
Restart and client reconnect recovers.

---

## 6. Control Plane

**Crate**: `kiseki-control`

**Purpose**: Declarative API for tenancy, IAM, policy, placement,
discovery, compliance tagging, and federation. Manages cluster-level and
tenant-level configuration.

**Key types**: `Organization`, `Project`, `Workload`, `Flavor`,
`ComplianceRegime`, `RetentionHold`, `FederationPeer`

**Key invariants**:

| ID | Rule |
|---|---|
| I-T1 | Tenants are fully isolated; no cross-tenant data access |
| I-T2 | Tenant resource consumption bounded by quotas at org and workload levels |
| I-T3 | Tenant keys not accessible to other tenants or shared processes |
| I-T4 | Cluster admin cannot access tenant data without tenant admin approval |
| I-T4c | Cluster admin modifications to pools with tenant data are audit-logged to tenant |

**Failure domain**: Control plane unavailability prevents new tenant
creation and policy changes, but the existing data path continues with
last-known configuration.

---

## 7. Key Management

**Crates**: `kiseki-keymanager`, `kiseki-crypto`

**Purpose**: Custody, rotation, escrow, and issuance of all key material.
Two layers: system keys (cluster admin) and tenant key wrapping (tenant
admin via tenant KMS). Orchestrate crypto-shred.

**Key types**: `SystemDek`, `SystemKek`, `TenantKek`, `KeyEpoch`,
`Envelope`, `TenantKmsProvider`

**Tenant KMS providers** (ADR-028): Five pluggable backends implementing
the `TenantKmsProvider` trait -- Kiseki-Internal, HashiCorp Vault, KMIP 2.1,
AWS KMS, and PKCS#11.

**Key invariants**:

| ID | Rule |
|---|---|
| I-K1 | No plaintext chunk is ever persisted to storage |
| I-K2 | No plaintext payload is ever sent on the wire |
| I-K4 | System can enforce access without reading plaintext |
| I-K5 | Crypto-shred renders data unreadable within bounded time |
| I-K6 | Key rotation does not lose access to old data until explicit cutover |
| I-K7 | Authenticated encryption everywhere |
| I-K8 | Keys are never logged, printed, transmitted in the clear, or in config files |
| I-K16 | Provider abstraction is opaque to callers |
| I-K17 | Wrap/unwrap operations include AAD (chunk_id) binding |

**Failure domain**: KMS unavailability blocks new encrypt/decrypt
operations. This context's availability is as critical as the Log's.

---

## 8. Workflow Advisory (cross-cutting)

**Crate**: `kiseki-advisory`

**Purpose**: Carry workflow hints from clients to storage and telemetry
feedback from storage back to clients. Route advisory signals to the
bounded context best able to act on them.

**Key types**: `WorkflowRef`, `OperationAdvisory`, `PoolHandle`,
`PoolDescriptor`, `HintBudget`

**Key invariants**:

| ID | Rule |
|---|---|
| I-WA1 | Hints are advisory only; no correctness decision depends on a hint |
| I-WA2 | Advisory subsystem is isolated from the data path; failures do not block data-path operations |
| I-WA3 | A workflow belongs to exactly one workload; authorization is per-operation |
| I-WA5 | Telemetry feedback is scoped to the caller's authorization |
| I-WA6 | Advisory requests are not existence or content oracles |
| I-WA7 | Hint budgets enforced per workload within parent ceilings |
| I-WA14 | Hints do not extend tenant capabilities |

**Runtime isolation**: The advisory runtime runs on a dedicated tokio
runtime separate from the data-path runtime (ADR-021). No data-path crate
depends on `kiseki-advisory`.

---

## Cross-context relationships

| Producer | Consumer | What flows |
|---|---|---|
| Control Plane | All contexts | Policy, placement, tenant config, compliance tags |
| Log | Composition, View | Deltas (ordered, durable) |
| Composition | Chunk Storage | Chunk references (refcounts) |
| Key Management | Chunk Storage | System DEKs |
| Key Management | Gateway, Native Client | Tenant KEK (wrapping) |
| View Materialization | Gateway, Native Client | Materialized view state |
| Chunk Storage | View, Native Client | Chunk data (encrypted) |
