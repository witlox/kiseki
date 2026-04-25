# Ubiquitous Language — Kiseki

**Status**: Layer 1 complete. Updated for ADR-027 (Rust-only), ADR-028 (External KMS), ADR-029 (Raw Block Device Allocator), and ADR-033/034/035 (cluster topology, shard merge, node lifecycle — spec-only; enforcement deferred).
**Last updated**: 2026-04-25.

One term per concept. No synonyms. If a term is not in this file, it is
not part of the domain language.

---

## Core data model

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Delta** | A single immutable mutation appended to a shard. Always metadata-about-data (chunk references, attribute changes, directory entries). May carry small inline data below a configurable size threshold (e.g., symlink targets, xattrs, small files). Bulk data is never inline — it goes to chunks. | Log | Immutable once committed. |
| **Chunk** | Opaque, immutable, encrypted data segment. Identity is derived from plaintext hash (content-addressed) by default; HMAC-derived when tenant opts out of cross-tenant dedup. Variable-size (content-defined chunking via Rabin fingerprinting). Always ciphertext at rest. | Chunk Storage | Chunks are never modified — new versions are new chunks. |
| **Composition** | Tenant-scoped metadata structure describing how to assemble chunks into a coherent data unit (e.g., a file, an object). Stored as a sequence of deltas in the log, not as a chunk. Reconstructed by replaying its deltas. | Composition | A POSIX file is one composition. An S3 object is another. Compositions reference chunks; chunks can be referenced by multiple compositions (dedup). |
| **Shard** | The smallest unit of totally-ordered deltas, backed by one Raft group. Automatic lifecycle: a configurable initial set is created when a namespace is created (see *initial shard topology*), splits when size/throughput thresholds are exceeded (I-L6) or when the cluster ratio floor would be violated (I-L11), and merges when sustained underutilization permits (I-L13). Transparent to tenants. Configurable split/merge thresholds. | Log / Cluster topology | Replaces "log shard" and "consistency set" — single term. |
| **View** | A protocol-shaped materialized projection of one or more shards. Maintained incrementally by a stream processor. Rebuildable from the source shard(s) at any time. Multiple views can coexist over the same underlying data (NFS view, S3 view, sequential-scan view). | View Materialization | "Materialization" refers to the physical process/state of maintaining a view, not a separate concept. |
| **View descriptor** | Declarative specification of a view's shape and behavior. Contains: source shard(s), protocol semantics (POSIX/S3), consistency model (read-your-writes/bounded-staleness/eventual), target affinity pool tier, discardability flag (can be dropped and rebuilt from log). | View Materialization | Immutable per version — changing a descriptor creates a new version. |
| **Stream processor** | Component in the View Materialization context that consumes deltas from a shard and maintains a view's materialized state. Separate from the protocol gateway. | View Materialization | Writes the view; does not serve it. |
| **Namespace** | Tenant-scoped collection of compositions distributed across one or more shards. Always belongs to exactly one tenant. May carry compliance regime tags. The namespace-to-shard mapping is recorded in the *namespace shard map* (I-L15) and routes writes by `hashed_key` range. | Composition / Control Plane | "Tenant namespace" is redundant — namespace is always tenant-scoped. Replaces the implicit single-shard model that predated ADR-033. |
| **Namespace shard map** | The persistent, Raft-replicated record describing which shards constitute a namespace and the `hashed_key` range each shard owns. Authoritative routing source for the gateway and native client. Updated atomically on shard creation, split, merge, and namespace creation. | Control Plane / Log | Lives in the control plane's Raft group; never in process memory only (I-L15). Stub today (`crates/kiseki-control/src/namespace.rs:34` is in-memory) — flagged for ADR-033 implementation. |

## Tenancy and access

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Tenant** | Hierarchical isolation boundary. Minimum two levels: organization → workload. Optional intermediate level: project. An organization is the billing, admin, and master-key-authority boundary. A project (if used) is a resource grouping and key delegation boundary. A workload is the runtime isolation unit. | Control Plane | Compliance regime tags attach at any level and inherit downward. |
| **Organization** | Top-level tenant. Owns billing, admin authority, and master key material. Mandatory. | Control Plane | One organization = one isolation domain for keys, quotas, and data. |
| **Project** | Optional grouping within an organization. Delegates key authority and scopes team access. | Control Plane | If absent, workloads belong directly to the organization. |
| **Workload** | Runtime unit within a tenant. Ephemeral or persistent. Scoped to an organization or project. | Control Plane | Represents a training run, experiment, service, etc. |
| **Cluster admin** | Operator of the Kiseki infrastructure. Manages nodes, global policy, system keys. Cannot access tenant config, logs, or data without explicit tenant admin approval. | Control Plane | Zero-trust boundary with tenants. Sees operational metrics only in tenant-anonymous or aggregated form. |
| **Tenant admin** | Administrator of a tenant (organization-level). Controls tenant keys, projects, workload authorization, compliance tags, user access. Grants or denies cluster admin access requests. | Control Plane | Authority for all tenant-scoped configuration. |
| **Retention hold** | Policy-driven constraint preventing physical chunk GC regardless of refcount. Scoped to tenant/namespace/composition. Has TTL or explicit release. Must be set before crypto-shred to prevent race with GC. | Control Plane / Chunk Storage | Ordering contract: set hold → crypto-shred → hold expires → GC eligible. |

## Encryption and key management

| Term | Definition | Context | Notes |
|---|---|---|---|
| **System DEK** | Symmetric key encrypting a chunk at the system layer. Managed by the system key manager. Always present — no unencrypted chunks. | Key Management | Per-chunk or per-group — granularity is an architectural decision. |
| **System KEK** | Key wrapping system DEKs. Managed by cluster admin via system key manager. | Key Management | |
| **Tenant DEK** | Not used in model (C). Tenant layer is key-wrapping, not data-encryption. | — | Retained for clarity: model (C) does NOT double-encrypt. |
| **Tenant KEK** | Key wrapping system DEKs for tenant-scoped access control. Tenant admin controls via tenant's KMS. Destruction = crypto-shred. | Key Management | The tenant access gate. Destroying this renders all tenant data unreadable. |
| **Envelope** | Complete wrapped structure for a chunk: ciphertext + system-layer wrapping metadata + tenant-layer wrapping metadata + authenticated metadata (chunk ID, algorithm identifiers, key epoch). | Key Management / Chunk Storage | Must carry algorithm identifiers for crypto-agility. |
| **Key epoch** | Version marker for key rotation. New data uses current epoch's keys. Old data retains its epoch until background re-encryption migrates it (epoch-based rotation). Full re-encryption available as explicit admin action. | Key Management | Two epochs may coexist during rotation window. |
| **System key manager** | Cluster-level component managing system keys (system DEKs and system KEKs). Onboard or external. Operated by cluster admin. | Key Management | |
| **Tenant KMS** | Tenant-controlled key management system, accessed via the `TenantKmsProvider` trait (ADR-028). Five backends: Kiseki-Internal (default), HashiCorp Vault, KMIP 2.1, AWS KMS, PKCS#11. Selection is per-tenant at onboarding. Must be reachable from storage nodes. | Key Management | Provider abstraction encapsulates local-vs-remote material models. |
| **Tenant KMS provider** | One of five pluggable backends implementing the `TenantKmsProvider` trait. Handles wrap/unwrap of DEK derivation parameters with AAD binding, key rotation, crypto-shred, and health checks. Callers never branch on provider type — the trait fully encapsulates protocol differences. | Key Management | ADR-028. Phases K1-K5. |
| **KMS epoch ID** | Provider-specific key version identifier returned by `rotate()`. Maps to: Vault key version, KMIP key state transitions, AWS KMS key ID, PKCS#11 key handle, or Kiseki-internal `KeyEpoch`. Opaque to callers. | Key Management | ADR-028. |
| **Crypto-shred** | Deletion by destroying the tenant KEK. Renders all tenant data unreadable. Does NOT immediately reclaim storage — physical GC runs separately when chunk refcount drops to 0 and no retention hold is active. | Key Management | Semantically authoritative deletion. Chunk ciphertext remains system-encrypted on disk until GC. |

## Infrastructure

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Affinity pool** | Group of storage devices (not nodes) sharing a device class (fast-NVMe, bulk-NVMe, etc.). A single node with mixed devices has devices in multiple pools. Devices self-classify; admin can override. | Chunk Storage / Control Plane | Placement policies compose across pools. |
| **Flavor** | Named deployment configuration: (protocol, transport, topology, access-path) tuple. Defined cluster-wide. Tenants select flavors; system provides best-fit match against available resources. Not an exact contract. | Control Plane | A flavor the cluster can't serve is reported as unavailable, not silently degraded. |
| **Protocol gateway** | Server-side component in the Protocol Gateway context. Translates wire protocol requests (NFS, S3) into operations against views. Reads from views; does not maintain them. Performs tenant-layer encryption for protocol-path clients (NFS/S3 clients send plaintext over TLS; gateway encrypts before writing). | Protocol Gateway | Separate from stream processor. Different failure modes, different scaling. |
| **Native client** | Client-side library running in workload processes. Exposes POSIX (via FUSE) and native API. Detects access patterns, selects transport, caches. Performs tenant-layer encryption for the native path — plaintext never leaves the workload process. | Native Client | Own bounded context, separate trust boundary (runs on tenant compute, not storage nodes). |

## Observability and audit

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Audit log** | Append-only, immutable, system-wide log of security-relevant events (data access, key lifecycle, admin actions, policy changes). Internal to Kiseki, same durability guarantees as the Log. | Control Plane / all contexts | Authoritative source. Not directly accessible to tenants or cluster admins — consumed via scoped exports. |
| **Tenant audit export** | Filtered projection of the audit log scoped to a single tenant. Delivered on the tenant's VLAN. Contains the tenant's own events plus relevant system events sufficient for a coherent, complete audit trail. | Control Plane | Required for HIPAA §164.312 audit controls. Tenant admin consumes this. |
| **Federation peer** | A remote Kiseki cluster participating in federated-async replication with the local cluster. Shares tenant config and discovery async. All federated sites for a tenant connect to the same tenant KMS. | Control Plane | Ciphertext-only data replication. No key material in replication stream. |

## Authentication

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Cluster CA** | Certificate authority managed by the cluster admin. Signs per-tenant mTLS certificates. The trust root for data-fabric authentication. | Control Plane | Certificates are local credentials — no real-time auth server needed on the data path. |
| **Tenant certificate** | mTLS certificate signed by the Cluster CA, identifying a client/gateway/stream processor as belonging to a specific tenant. Presented on every data-fabric connection. | Control Plane / all contexts | Works on SAN with no control plane access. |
| **Tenant identity provider (optional)** | Second-stage authentication via the tenant's own key manager or IdP. Validates workload identity against tenant admin's authorization. | Control Plane / Key Management | Opt-in. Provides "authorized by my tenant admin" on top of "belongs to this cluster." |

## Time

| Term | Definition | Context | Notes |
|---|---|---|---|
| **HLC (Hybrid Logical Clock)** | Clock combining physical time (ms since epoch) + logical counter + node_id. Authoritative for ordering and causality. Syncs via Lamport rule: local = max(local, remote) + 1. | All contexts | Intra-shard: Raft sequence numbers (total order). Cross-shard and cross-site: HLC. |
| **Wall clock** | Physical time with timezone. Authoritative only for duration-based policies: retention TTLs, staleness bounds, compliance deadlines, audit timestamps. | All contexts | Not used for correctness decisions. Staleness bounds (e.g., 2s HIPAA floor) measured against wall time. |
| **Clock quality** | Self-reported per node: Ntp, Ptp, Gps, or Unsync. Used for drift detection via HLC/wall-clock pairs. | Control Plane | Nodes reporting Unsync are flagged; staleness bounds involving their timestamps are unreliable. |
| **Delta timestamp** | Triple attached to every delta: (HLC, wall_time + timezone, clock_quality). Dual clock model adapted from taba. | Log | Ordering clock and wall clock serve different authorities for different concerns. |

## Compression

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Chunk compression** | Optional compress-then-encrypt with fixed-size padding at the chunk level. Default off (safest). Tenant opt-in. Compliance tags may prohibit enabling. | Chunk Storage | CRIME/BREACH side-channel risk mitigated by padding. Residual risk accepted by tenant on opt-in. |

## Block I/O (ADR-029)

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Extent** | A contiguous range on a data device, identified by (offset, length). The unit of space allocation for chunk data. Extents are block-aligned. | Chunk Storage / Block I/O | An extent maps 1:1 to a chunk fragment on a device. |
| **DeviceBackend** | Trait abstracting raw block device I/O. Two implementations: `RawDevice` (direct block device access via O_DIRECT) and `FileBacked` (regular file fallback for VMs/CI). Auto-detected at device open. | Block I/O | ADR-029. All chunk data I/O goes through this trait. |
| **Superblock** | Per-device metadata block written at offset 0. Contains: magic number, format version, device UUID, bitmap offset, bitmap size, capacity, physical block size. Written once at device initialization; updated on format changes. | Block I/O | ADR-029. Read at device open to validate and configure the allocator. |
| **Allocation bitmap** | Per-device bit vector tracking free vs allocated blocks. Ground truth for space management on a device. Updates are journaled in redb before application. Free-list is a derived in-memory cache rebuilt from the bitmap on startup. | Block I/O | ADR-029. One bit per allocatable block. |
| **DeviceCharacteristics** | Auto-probed properties of a data device: physical block size, logical block size, optimal I/O size, rotational flag, capacity. Probed via `ioctl` on raw devices or `statfs` on file-backed devices. Used to align all I/O. | Block I/O | ADR-029. Cached at device open. |

## Workflow advisory and telemetry

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Client ID** | Stable identifier pinned to one native-client process instance for its lifetime. Derived from the workload's mTLS certificate plus a process-local nonce at startup. Scoped within `(org, project?, workload)`. Never reused across processes. | Workflow Advisory / Native Client | Ties an operation stream to a single executing process. Not a user identity. Not a session token. |
| **Workflow** | Ephemeral correlation handle declared by a client to group a related sequence of operations (e.g., one HPC job, one AI training run, one inference serving session). Owned by exactly one workload. Lifecycle: `Declare → (one or more phases) → End or TTL expiry`. Not a persistent domain object; not a bounded context; no durable on-disk state beyond audit. | Workflow Advisory | `workflow_id` is opaque, tenant-scoped, and GC'd on end or TTL. Uniqueness required only within the owning workload. |
| **Phase** | A named, monotonically-advancing segment of a workflow carrying semantic intent (e.g., `stage-in`, `compute`, `checkpoint`, `stage-out`, `epoch-N`). A workflow has one current phase at any instant. Phases advance forward only; no reopening. | Workflow Advisory | Drives phase-adaptive tuning (cache policy, prefetch, write-absorb provisioning). Bounded phase history per workflow (last-K, configurable). |
| **Workload profile** | One-shot declaration of the workload's overall character, chosen from a cluster-defined preset set (e.g., `hpc-checkpoint`, `ai-training`, `ai-inference`, `batch-etl`, `interactive`). Bundles default phase-tag semantics, priority class, and hint-handler behavior. | Workflow Advisory | Profile availability is policy-gated per org/project/workload. An unsupported profile is rejected at `DeclareWorkflow`. |
| **Hint** | A unit of advisory information sent by a client to the storage system about its intended or current behavior (access pattern, prefetch range, priority class, affinity preference, retention intent, dedup intent, collective announcement, deadline). Advisory only — never authoritative. | Workflow Advisory | Hints can be ignored, throttled, or rejected with no correctness impact. A hint that cannot be honoured does not fail the underlying operation. |
| **Telemetry feedback** | A signal emitted by the storage system *back to a client* about the state of its own operations and the storage resources it is currently using (backpressure, saturation, materialization lag, placement locality, prefetch effectiveness, QoS headroom, hotspot on its own compositions). Scoped strictly to the caller's own authorization scope. | Workflow Advisory | Distinct from operator-facing observability (ADR-015 metrics/traces/logs), which is aggregate and cluster/tenant-admin-consumed. |
| **Advisory channel** | The bidirectional transport (gRPC bidi stream on the data fabric) carrying hints in and telemetry feedback out for a single declared workflow. One channel per active workflow. | Workflow Advisory | Session-bound to the client's mTLS identity. Closure terminates the workflow if `End` was not sent. |
| **Hint budget** | Per-workload policy-enforced cap on advisory activity: hints/sec, concurrent workflows, phases per workflow, telemetry subscribers, prefetch bytes declared. Exceeding the budget degrades only the offending workload. | Workflow Advisory / Control Plane | Set by tenant admin within org/project ceilings. Cluster admin sets cluster-wide ceilings. |
| **Hint handler** | Component in the Workflow Advisory cross-cutting concern that validates, rate-limits, audits, and routes a hint to the bounded context best able to act on it (Chunk Storage placement, View prefetch, Composition write-absorb, etc.). | Workflow Advisory | Stateless routing + bounded per-workflow state. |
| **Locality class** | Bucketed answer telemetry can return about a chunk set: `local-node`, `local-rack`, `same-pool`, `remote`, `degraded`. Bucket granularity is deliberate — finer values would leak placement. | Workflow Advisory | Scoped to chunks the caller owns. Never reveals neighbours' placement. |
| **Backpressure signal** | Telemetry feedback that a resource the caller is using (pool, shard, view) is saturated. Carries a coarse severity (`ok`, `soft`, `hard`) and a retry-after hint. Computed over the caller's own traffic plus aggregate resource state with k-anonymous neighbour bucketing. | Workflow Advisory | `hard` means the caller must slow or stop; `soft` is advisory. |
| **Advisory audit event** | Audit log entry for a Workflow Advisory decision: `declare`, `end`, `phase-advance`, `hint-accepted`, `hint-rejected`, `hint-throttled`, `telemetry-subscribed`, `budget-exceeded`, `advisory-state-transition`, `phase-summary`, `subscription-revoked`. Routed to the tenant audit shard. | Workflow Advisory / Audit | Same durability and scoping guarantees as all other tenant audit events (ADR-009). |
| **Pool handle** | Opaque tenant-scoped reference to an affinity pool the workload is authorized to target via advisory hints. Minted by the advisory subsystem at `DeclareWorkflow` and returned to the caller as part of the workflow's `authorized pools` set. Never reveals the cluster-internal pool identity. Lifetime = the workflow's lifetime. | Workflow Advisory | Replaces any cluster-internal pool ID on the advisory path (I-WA11). A pool decommissioned during the workflow's life turns the handle into a `scope-not-found` on use. |
| **Pool descriptor** | The record returned alongside a pool handle: `{handle, opaque_label}`. The label is a tenant-chosen string (e.g., "fast-nvme", "bulk-nvme") set at workload-authorization time. It is meaningful to the workload operator; it is not a cluster-internal identifier. | Workflow Advisory | Multiple tenants can see the same opaque label attached to different internal pools; correlation across tenants is impossible because handles differ. |

## Client-side cache (ADR-031)

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Client cache** | Two-tier (L1 in-memory, L2 local NVMe) read-only cache of decrypted plaintext chunks in `kiseki-client`. Content-addressed by `ChunkId`. Ephemeral — wiped on process restart and long disconnect. | Native Client | Performance feature, not correctness mechanism. Three modes: pinned, organic, bypass. |
| **Cache pool** | Per-process L2 directory on local NVMe, identified by a 128-bit CSPRNG `pool_id`. Isolated per process and per tenant. Ownership proven by `flock` on `pool.lock`. Orphaned pools (no live flock holder) are scavenged on startup or by `kiseki-cache-scrub`. | Native Client | No cross-process sharing. Concurrent same-tenant processes have independent pools. |
| **Cache mode** | One of three operational modes per client instance: **pinned** (staging-driven, eviction-resistant, for declared datasets), **organic** (LRU with usage-weighted retention, default for mixed workloads), **bypass** (no caching, for streaming/checkpoint workloads). Selected at session establishment within admin-allowed set. | Native Client | Mode is per session, not per file. |
| **Staging** | Client-local pull-based operation that pre-fetches a dataset's chunks from canonical into the L2 cache with pinned retention. Takes a namespace path, recursively enumerates compositions, fetches and verifies all chunks. Idempotent and resumable. | Native Client | Used by Slurm prolog, Lattice pre-dispatch, or manual invocation. Produces a manifest file listing staged compositions and chunk_ids. |
| **Staging handoff** | Mechanism for transferring an L2 cache pool from a staging daemon process to a workload process. The staging daemon holds the `pool.lock` flock; the workload adopts the pool via `KISEKI_CACHE_POOL_ID` environment variable and takes over the flock. | Native Client | Enables staging in Slurm prolog (separate process) to survive into the workload process. |
| **Metadata TTL** | Time-to-live for cached file→chunk_list mappings. The sole freshness window in the cache design. Within TTL, cached metadata is authoritative (may serve stale data for modified or deleted files). Default 5 seconds. | Native Client | Chunk data has no TTL — chunks are immutable (I-C1). |
| **Key health check** | Periodic client-side probe of the tenant KMS (default every 30s) to detect crypto-shred events. Returns `KEK_DESTROYED` if the tenant KEK has been deleted, triggering immediate cache wipe. | Native Client / Key Management | Primary detection mechanism for crypto-shred. Bounded detection latency: `min(key_health_interval, max_disconnect_seconds)`. |

## Cluster topology and node lifecycle (ADR-033 / ADR-034 / ADR-035)

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Initial shard topology** | The set of shards created at namespace creation. Default count: `max(min(3 × node_count_at_creation, 64), 3)`. Cluster admin may override per cluster; tenant admin may override per namespace within admin-defined min/max bounds. Leader placement uses *best-effort round-robin* across nodes at creation. | Cluster topology | Day-one multi-leader distribution so an N-node cluster does not bottleneck on a single Raft leader. Once created, the count grows only via split (I-L11/I-L6) or shrinks only via merge (I-L13). |
| **Shards-per-node ratio** | The instantaneous quotient `shard_count / node_count` for a given namespace within a cluster. Watched as an indicator of write parallelism headroom. | Cluster topology | Distinct from the per-shard ceilings (I-L6) — this is a topology indicator, not a per-shard size measure. |
| **Ratio floor** | The minimum permitted shards-per-node ratio. Default 1.5×. Whenever the live ratio for a namespace drops below this floor (e.g., after a node-add), the system triggers an auto-split to restore it. | Cluster topology | Cluster-admin configurable. Default chosen so every node has, on average, at least one and a half leaderable shards — protects against a single hot shard stranding a node idle. |
| **Leader placement policy** | The rule the system uses to assign a Raft leader role for a newly created shard (initial topology, split, or merge). Policy = *best-effort round-robin*: at creation/split/merge time, pick the node currently hosting the fewest leaders for that namespace; on tie, pick deterministically by node ID. No invariant enforces post-hoc balance — drift between rebalancing events is permitted. | Cluster topology | Operator may trigger explicit rebalance via control plane (out of scope for ADR-033; deferred). Aligns with assumption A-N3. |
| **Shard merge** | The inverse of shard split: two adjacent shards (by `hashed_key` range) are combined into one. Trigger: combined utilization across all dimensions (delta count, byte size, write throughput) stays below the *merge threshold* for the *merge interval* AND the merge would not violate the ratio floor (I-L13). Preserves total order across the merged range (I-L14). | Log / Cluster topology | Spec-only at present — no merge code exists in `kiseki-log`; deferred to ADR-034 implementation. |
| **Merge threshold** | The per-dimension utilization level below which a shard is merge-eligible. Default: 25% of the corresponding split ceiling on every dimension simultaneously. Cluster-admin sets defaults; tenant admin may override per namespace within admin envelope. | Log / Cluster topology | Asymmetric to the split ceiling so that a workload near 25% does not split-then-merge oscillate. |
| **Merge interval** | The minimum sustained duration a shard must remain merge-eligible before merge fires. Default 24 hours. Prevents transient drops from triggering merges. | Log / Cluster topology | Cluster-admin configurable. |
| **Node state** | One of `Active`, `Draining`, or `Evicted`. State machine is forward-only: `Active → Draining → Evicted`. Only the `Active → Draining` transition is operator-initiated; `Draining → Evicted` happens automatically once drain conditions are satisfied. There is no `Evicted → Active` — re-adding requires a fresh node identity. | Cluster topology / Node lifecycle | Distinct from device state (I-D1..I-D5). A node hosts devices; a node can drain even if its devices are healthy. |
| **Drain** | The operation that transitions a node from `Active` to `Evicted`. While `Draining`: (a) leadership is transferred off the node for every shard it leads, (b) a new voter is added on a surviving node and promoted for every shard the draining node holds a voter slot in, (c) the draining node accepts no new leader assignments. The node enters `Evicted` only when all three conditions hold for every shard. | Node lifecycle | Drain refusal at the RF floor: if completing the drain would drop any shard below RF=3, the drain request is rejected up front (I-N4). |
| **Leadership transfer** | The Raft operation that moves a shard's leader role from one voter to another without re-election. Required as the first stage of drain (I-N2). Implemented as an openraft membership operation; new voter must be caught up before transfer. | Node lifecycle | `crates/kiseki-raft/src/membership.rs:11-18` defines the primitive (`AddLearner`, `PromoteVoter`, `RemoveVoter`); the drain orchestration that calls them is absent — flagged for ADR-035 implementation. |
| **Voter replacement** | The Raft operation sequence that removes one voter and adds another while preserving RF: `AddLearner(new) → wait_caught_up(new) → ChangeMembership(promote new + remove old)`. RF is preserved at every intermediate state — the cluster never operates below RF during a drain (I-N3). | Node lifecycle | Same primitives as I-SF3 (shard migration via learner promotion). |
| **Re-replication guarantee** | The drain protocol's commitment that, for every shard the draining node held a voter slot in, a replacement voter is fully caught up and promoted before the node is allowed to enter `Evicted`. The cluster is never below RF=3 even transiently during a drain. | Node lifecycle | Aligns with I-CS1 (CP for writes) — no drain-induced reduction in durability. |

## Retired / rejected terms

| Term | Replaced by | Reason |
|---|---|---|
| Log shard | Shard | Single term; consistency is an invariant of a shard, not a separate concept. |
| Consistency set | Shard | Same as above. |
| Tenant namespace | Namespace | Namespace is always tenant-scoped; prefix was redundant. |
| Materialization | View (process: "view materialization") | "View" is the concept; "materialization" describes the process of maintaining it. |
| Tenant DEK | (not used in model C) | Model (C): system encrypts data, tenant wraps access. No tenant-layer data encryption. |
