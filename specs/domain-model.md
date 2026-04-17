# Domain Model — Kiseki

**Status**: First cut — Layer 1 interrogation in progress.
**Last updated**: 2026-04-17, Session 2.

---

## Bounded contexts

Eight bounded contexts confirmed through interrogation. Each has a
distinct responsibility, failure domain, and scaling concern.

```
┌─────────────────────────────────────────────────────────────────┐
│                        Control Plane                            │
│  Tenancy · IAM · Policy · Placement · Discovery · Compliance    │
│  Federation (async cross-site) · Flavor management              │
│  Cluster admin operations · Tenant admin operations             │
└──────────┬──────────────────────────────────┬───────────────────┘
           │ policy/placement                 │ key policy
           ▼                                  ▼
┌────────────────────┐              ┌────────────────────┐
│        Log         │              │  Key Management    │
│  Delta ordering    │              │  System key mgr    │
│  Raft per shard    │              │  Tenant KMS integ  │
│  Replication       │              │  Rotation/escrow   │
│  Durability        │              │  Crypto-shred      │
└────────┬───────────┘              └────────┬───────────┘
         │ deltas                            │ keys
         ▼                                   │
┌────────────────────┐                       │
│   Composition      │                       │
│  Chunk assembly    │                       │
│  Tenant-scoped     │                       │
│  Namespace mgmt    │                       │
└────────┬───────────┘                       │
         │ chunk refs                        │
         ▼                                   │
┌────────────────────┐                       │
│  Chunk Storage     │◄──────────────────────┘
│  Encrypted chunks  │  encrypt/decrypt ops
│  Placement (pools) │
│  Replication / EC  │
│  GC + refcount     │
│  Retention holds   │
└────────────────────┘
         ▲ read chunks    ▲ read chunks
         │                │
┌────────┴───────┐  ┌─────┴──────────┐
│ View Material. │  │ Native Client  │
│ Stream proc.   │  │ FUSE + native  │
│ Incremental    │  │ Pattern detect │
│ view maint.    │  │ Transport sel. │
└────────┬───────┘  │ Client encrypt │
         │ views    └────────────────┘
         ▼
┌────────────────────┐
│ Protocol Gateway   │
│ NFS / S3 translate │
│ View reads         │
│ Gateway encrypt    │
│ Wire protocol      │
└────────────────────┘
```

---

## Context descriptions

### 1. Log

**Responsibility**: Accept deltas, assign them a total order within a
shard, replicate via Raft, persist durably, support range reads for
view materialization and replay.

**Key entities**: Delta, Shard, Raft group.

**Owns**: Delta ordering, shard lifecycle (create, split, merge),
replication, durability guarantee.

**Key invariants**:
- Within a shard, deltas have a total order (I-L1)
- A committed delta is durable on a majority of replicas before ack (I-L2)
- A delta is immutable once committed (I-L3)

**Failure domain**: Per-shard. Leader loss → transient latency (election).
Quorum loss → shard unavailable. Log corruption → catastrophic for the
shard's compositions (recovery design TBD).

**Consumes from**: Control Plane (shard placement policy).
**Produces for**: Composition (deltas), View Materialization (delta stream).

---

### 2. Chunk Storage

**Responsibility**: Store and retrieve opaque encrypted chunks. Manage
placement across affinity pools. Handle replication/EC. Run GC based on
refcounts. Enforce retention holds.

**Key entities**: Chunk, Affinity pool, Envelope.

**Owns**: Chunk persistence, placement, replication, GC, retention hold
enforcement.

**Key invariants**:
- Chunks are immutable; new versions are new chunks (I-C1)
- A chunk is not GC'd while any composition references it (I-C2)
- A chunk is not GC'd while a retention hold is active (I-C2b)
- No plaintext chunk is ever persisted to storage (I-K1)

**Failure domain**: Per-chunk or per-device. Chunk loss recoverable
via replication/EC. Device loss affects chunks placed on that device.

**Consumes from**: Key Management (system DEKs for encryption),
Control Plane (placement policy, retention holds).
**Produces for**: View Materialization (chunk reads), Native Client
(chunk reads), Protocol Gateway (chunk reads via views).

---

### 3. Composition

**Responsibility**: Maintain tenant-scoped metadata structures that
describe how chunks assemble into data units. Manage namespaces.
Record mutations as deltas in the log.

**Key entities**: Composition, Namespace.

**Owns**: Composition lifecycle, namespace management, chunk-reference
bookkeeping (refcounts).

**Key invariants**:
- A composition belongs to exactly one tenant (I-X1)
- A composition's chunks respect the tenant's dedup policy (I-X2)
- A composition's mutation history is fully reconstructible from its
  shard's deltas (I-X3)

**Failure domain**: Coupled to Log — if a shard fails, its compositions
are affected.

**Consumes from**: Log (delta persistence), Chunk Storage (chunk
references), Control Plane (tenant/namespace policy).
**Produces for**: View Materialization (composition state via log).

---

### 4. View Materialization

**Responsibility**: Consume deltas from shards. Maintain materialized
views per view descriptor. Handle view lifecycle: create, update, tier,
discard, rebuild from log.

**Key entities**: View, View descriptor, Stream processor.

**Owns**: View state, materialization lag, view lifecycle.

**Key invariants**:
- A view is derivable from its source shard(s) alone (I-V1)
- A view's observed state is a consistent prefix of its source log(s)
  up to some watermark (I-V2)
- Reads from a view see a snapshot at a specific log position (I-V3)

**Failure domain**: Per-view. A fallen-behind view serves stale data.
A lost view can be rebuilt from the log.

**Consumes from**: Log (delta stream), Chunk Storage (chunk reads for
materialization), Control Plane (view descriptor policy).
**Produces for**: Protocol Gateway (view reads), Native Client (view reads).

---

### 5. Protocol Gateway

**Responsibility**: Translate wire protocol requests (NFS, S3) into
operations against views and the log. Serve reads from views. Route
writes as deltas to the log (via composition). Perform tenant-layer
encryption for protocol-path clients.

**Key entities**: Protocol gateway instance, Protocol plugin, Transport
plugin.

**Owns**: Protocol semantics enforcement, wire-level access, session
state (e.g., NFSv4.1 state).

**Trust boundary**: NFS/S3 clients send plaintext over TLS to the
gateway. The gateway encrypts before writing to log/chunks. Plaintext
exists in gateway memory, ephemerally.

**Failure domain**: Per-gateway. Crash disconnects the affected tenant's
clients on that protocol. Restart + client reconnect recovers.

**Consumes from**: View Materialization (view reads), Log (delta writes),
Key Management (tenant KEK for wrapping), Control Plane (gateway placement).
**Produces for**: External clients (NFS/S3 responses).

---

### 6. Native Client

**Responsibility**: Client-side library running in workload processes.
Expose POSIX (FUSE) and native API. Detect access patterns, select best
transport (libfabric/CXI → verbs → TCP), cache, and perform tenant-layer
encryption. Plaintext never leaves the workload process.

**Key entities**: Native client instance, FUSE mount, Transport selector.

**Owns**: Client-side caching, transport negotiation, access pattern
detection, client-side encryption.

**Trust boundary**: Runs on tenant compute. Holds tenant key material
in process memory. Encrypts before any data leaves the process.

**Failure domain**: Per-client-process. Crash loses in-flight uncommitted
writes (same as any client). No cluster-wide impact.

**Consumes from**: View Materialization (view reads), Log (delta writes),
Chunk Storage (chunk reads/writes), Key Management (tenant KEK).
**Produces for**: Workload (POSIX/native API).

---

### 7. Key Management

**Responsibility**: Custody, rotation, escrow, and issuance of all key
material. Two layers: system keys (cluster admin) and tenant key wrapping
(tenant admin via tenant KMS). Orchestrate crypto-shred. Manage key
epochs.

**Key entities**: System DEK, System KEK, Tenant KEK, Key epoch,
Envelope, System key manager, Tenant KMS.

**Owns**: Key lifecycle (create, rotate, escrow, destroy), wrapping
operations, epoch tracking, crypto-shred orchestration, audit trail
for key events.

**Two-layer encryption model (C)**:
- System layer: system DEK encrypts chunk data. Always on.
- Tenant layer: tenant KEK wraps system DEK for tenant-scoped access.
  No double encryption — one data encryption pass, key wrapping for access control.

**Key rotation**: Epoch-based (C). New data uses current epoch keys.
Background re-encryption migrates old data. Full re-encryption available
as admin action for key-compromise incidents.

**Key invariants**:
- No plaintext chunk is ever persisted (I-K1)
- No plaintext payload is ever on the wire (I-K2)
- The system can enforce access without reading plaintext (I-K4)
- Crypto-shred renders data unreadable within bounded time (I-K5)
- Key rotation does not lose access to old data until explicit cutover (I-K6)
- Authenticated encryption everywhere (I-K7)
- Keys are never logged, printed, transmitted in the clear, or in config files (I-K8)

**Failure domain**: KMS unavailability blocks new encrypt/decrypt
operations. Cached keys may sustain a bounded window. Tenant key loss
without escrow = data loss. This context's availability is as critical
as the Log's.

**Consumes from**: Control Plane (tenant/key policy, compliance regime).
**Produces for**: Chunk Storage (system DEKs), Protocol Gateway (tenant
KEK for wrapping), Native Client (tenant KEK for wrapping).

---

### 8. Control Plane

**Responsibility**: Declarative API for tenancy, IAM, policy, placement,
discovery, compliance tagging, and federation. Manages cluster-level
configuration and tenant-level configuration (with appropriate access
controls).

**Key entities**: Organization, Project (optional), Workload, Cluster
admin, Tenant admin, Flavor, Compliance regime tag, Retention hold,
Federation peer.

**Owns**: Tenant hierarchy, IAM, quota enforcement, placement decisions,
flavor matching, VLAN configuration, compliance tag inheritance,
cross-site federation (async replication orchestration, data residency
enforcement).

**Key invariants**:
- Tenants cannot read each other's compositions without explicit
  cross-tenant authorization (I-T1)
- A tenant's resource consumption is bounded by quotas (I-T2)
- A tenant's keys are not accessible to other tenants or shared
  processes (I-T3)
- Cluster admin cannot access tenant config/logs/data without tenant
  admin approval

**Failure domain**: Control plane unavailability prevents new tenant
creation, policy changes, and placement decisions. Existing data path
continues (log, chunks, views keep working with last-known config).

**Consumes from**: Key Management (key policy status).
**Produces for**: All other contexts (policy, placement, tenant config).

---

## Cross-context relationships (summary)

| Producer | Consumer | What flows |
|---|---|---|
| Control Plane | All contexts | Policy, placement, tenant config, compliance tags |
| Log | Composition, View Materialization | Deltas (ordered, durable) |
| Composition | Chunk Storage | Chunk references (refcounts) |
| Key Management | Chunk Storage | System DEKs |
| Key Management | Protocol Gateway, Native Client | Tenant KEK (wrapping) |
| View Materialization | Protocol Gateway, Native Client | Materialized view state |
| Chunk Storage | View Materialization, Native Client | Chunk data (encrypted) |

---

## Resolved design decisions (Session 2)

- **Shards are single-tenant.** Raft replication never mixes tenants'
  deltas. Cross-tenant dedup operates at the chunk layer (orthogonal
  to shard isolation).
- **Multiple compositions per shard.** Shard scoped to a namespace or
  namespace partition. All compositions in that namespace share the
  shard's total ordering. Shard splits on configurable thresholds.
- **View descriptor changes: pull-based.** Stream processors watch
  descriptor version in the control plane; pick up changes on next
  materialization cycle.
- **Federation: tenant config + discovery replicated async; KMS
  connectivity is live cross-site.** All federated sites for a tenant
  connect to the same tenant KMS instance. Key material is never
  replicated — one source of truth. Async data replication carries
  ciphertext only.
- **Quota enforcement: per-org and per-workload.** Per-project optional
  (mirrors tenant hierarchy). Org sets ceiling; workload gets
  allocation within it.
- **Object versioning in scope.** Log-as-truth naturally supports
  history. Exposed via S3 versioned buckets and POSIX snapshots.
  Behavioral spec to follow.

- **Cross-tenant data access: out of scope.** Tenants are fully
  isolated. No delegation tokens, no cross-tenant key sharing.
- **Native client discovery: must work without control plane access.**
  Client runs on SAN fabric compute nodes. Bootstrap and discovery
  must work over the data fabric. Client protocol spec defines the
  mechanism (architect's decision).
- **Audit trail: internal authoritative log + tenant-scoped export.**
  Internal log is append-only, immutable, system-wide. Tenant export
  is on tenant VLAN, includes tenant's events + relevant system
  events (filtered) for a coherent complete audit trail. Cluster admin
  sees system-level events only (tenant-anonymous/aggregated per
  zero-trust boundary).

## Open questions for continued interrogation

- [ ] KMS availability across federated sites: bounded cache TTL for
      tenant keys at remote sites? Behavior when cache expires and
      KMS is unreachable?
- [ ] Shard split/merge: who configures thresholds (cluster admin,
      tenant admin, both)?
- [ ] Object versioning: retention/expiration policies for versions?
      Interaction with crypto-shred (shred current + all versions?)
- [ ] Native client bootstrap: what mechanism discovers shards/views/
      gateways from the data fabric? (Architect's decision, but
      constraint is: no control plane access assumed.)
- [ ] Audit event filtering: what system events are "relevant" for
      tenant export? Needs explicit enumeration during behavioral spec.
