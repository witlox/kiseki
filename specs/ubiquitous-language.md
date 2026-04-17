# Ubiquitous Language — Kiseki

**Status**: Draft — Layer 1 interrogation in progress.
**Last updated**: 2026-04-17, Session 2.

One term per concept. No synonyms. If a term is not in this file, it is
not part of the domain language.

---

## Core data model

| Term | Definition | Context | Notes |
|---|---|---|---|
| **Delta** | A single immutable mutation appended to a shard. Always metadata-about-data (chunk references, attribute changes, directory entries). May carry small inline data below a configurable size threshold (e.g., symlink targets, xattrs, small files). Bulk data is never inline — it goes to chunks. | Log | Immutable once committed. |
| **Chunk** | Opaque, immutable, encrypted data segment. Identity is derived from plaintext hash (content-addressed) by default; HMAC-derived when tenant opts out of cross-tenant dedup. Variable-size (content-defined chunking via Rabin fingerprinting). Always ciphertext at rest. | Chunk Storage | Chunks are never modified — new versions are new chunks. |
| **Composition** | Tenant-scoped metadata structure describing how to assemble chunks into a coherent data unit (e.g., a file, an object). Stored as a sequence of deltas in the log, not as a chunk. Reconstructed by replaying its deltas. | Composition | A POSIX file is one composition. An S3 object is another. Compositions reference chunks; chunks can be referenced by multiple compositions (dedup). |
| **Shard** | The smallest unit of totally-ordered deltas, backed by one Raft group. Automatic lifecycle: created when a namespace is created, splits when size/throughput thresholds are exceeded. Transparent to tenants. Configurable split/merge thresholds. | Log | Replaces "log shard" and "consistency set" — single term. |
| **View** | A protocol-shaped materialized projection of one or more shards. Maintained incrementally by a stream processor. Rebuildable from the source shard(s) at any time. Multiple views can coexist over the same underlying data (NFS view, S3 view, sequential-scan view). | View Materialization | "Materialization" refers to the physical process/state of maintaining a view, not a separate concept. |
| **View descriptor** | Declarative specification of a view's shape and behavior. Contains: source shard(s), protocol semantics (POSIX/S3), consistency model (read-your-writes/bounded-staleness/eventual), target affinity pool tier, discardability flag (can be dropped and rebuilt from log). | View Materialization | Immutable per version — changing a descriptor creates a new version. |
| **Stream processor** | Component in the View Materialization context that consumes deltas from a shard and maintains a view's materialized state. Separate from the protocol gateway. | View Materialization | Writes the view; does not serve it. |
| **Namespace** | Tenant-scoped collection of compositions within a shard. Always belongs to exactly one tenant. May carry compliance regime tags. | Composition / Control Plane | "Tenant namespace" is redundant — namespace is always tenant-scoped. |

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
| **Tenant KMS** | Tenant-controlled key management system. External (tenant brings own: AWS KMS, HashiCorp Vault, HSM) or Kiseki-hosted with tenant-admin-only access. Must be reachable on tenant's VLAN. | Key Management | Exact deployment topology deferred to architect. |
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

## Retired / rejected terms

| Term | Replaced by | Reason |
|---|---|---|
| Log shard | Shard | Single term; consistency is an invariant of a shard, not a separate concept. |
| Consistency set | Shard | Same as above. |
| Tenant namespace | Namespace | Namespace is always tenant-scoped; prefix was redundant. |
| Materialization | View (process: "view materialization") | "View" is the concept; "materialization" describes the process of maintaining it. |
| Tenant DEK | (not used in model C) | Model (C): system encrypts data, tenant wraps access. No tenant-layer data encryption. |
