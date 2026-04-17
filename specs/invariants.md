# Invariants — Kiseki

**Status**: Layer 2 — interrogation substantially complete.
**Last updated**: 2026-04-17, Session 2.

All invariants below have been confirmed through interrogation with the
domain expert unless marked otherwise.

---

## Log invariants

| ID | Invariant | Status |
|---|---|---|
| I-L1 | Within a shard, deltas have a total order. | Confirmed |
| I-L2 | A committed delta is durable on a majority of Raft replicas before ack. | Confirmed |
| I-L3 | A delta is immutable once committed. | Confirmed |
| I-L4 | GC of deltas requires that ALL consumers (all views consuming from the shard AND the audit log) have advanced past the delta's position. A stalled consumer blocks GC for that shard. | Confirmed |
| I-L5 | A composition is not visible to readers until all chunks referenced by its deltas are durable. Normal writes: protocol enforces chunk-before-delta ordering. Bulk/multipart: finalize step gates reader visibility after all chunks confirmed durable. | Confirmed |
| I-L6 | Shards have a hard ceiling triggering mandatory split. Ceiling configurable across dimensions: delta count, byte size, write throughput. Any dimension exceeding its ceiling forces split. Thresholds set by cluster admin or control plane defaults. | Confirmed |
| I-L7 | Delta envelope has structurally separated system-visible header (cleartext/system-encrypted) and tenant-encrypted payload. Compaction operates on headers only; payloads are carried opaquely. | Confirmed |
| I-L8 | Cross-shard rename returns EXDEV. Shards are independent consensus domains; no 2PC. Applications handle via copy + delete. | Confirmed |

---

## Chunk invariants

| ID | Invariant | Status |
|---|---|---|
| I-C1 | Chunks are immutable. New versions are new chunks. | Confirmed |
| I-C2 | A chunk is not GC'd while any composition references it (refcount > 0). | Confirmed |
| I-C2b | A chunk is not GC'd while a retention hold is active, regardless of refcount. Retention hold must be set before crypto-shred to prevent GC race. Ordering: set hold → crypto-shred → hold expires → GC eligible. | Confirmed |
| I-C3 | Chunks are placed according to affinity policy derived from the referencing composition's view descriptor. | Confirmed |
| I-C4 | Chunk durability strategy is per affinity pool. EC is the default. Replication (N-copy) available for pools where EC overhead is unacceptable. Pool-level policy set by cluster admin. | Confirmed |

---

## Composition invariants

| ID | Invariant | Status |
|---|---|---|
| I-X1 | A composition belongs to exactly one tenant. | Confirmed |
| I-X2 | A composition's chunks respect the tenant's dedup policy: global hash (default, cross-tenant dedup active) or per-tenant HMAC (opted-out, cross-tenant dedup impossible). | Confirmed |
| I-X3 | A composition's mutation history is fully reconstructible from its shard's deltas. | Confirmed |

---

## View invariants

| ID | Invariant | Status |
|---|---|---|
| I-V1 | A view is derivable from its source shard(s) alone — no external state required. (Rebuildable-from-log property.) | Confirmed |
| I-V2 | A view's observed state is a consistent prefix of its source log(s) up to some watermark. | Confirmed |
| I-V3 | Cross-view consistency is governed by the reading protocol's declared consistency model. Strong-consistency protocols (POSIX) see read-your-writes across views. Weak-consistency protocols may see bounded staleness. The view descriptor declares the model; the stream processor enforces it. | Confirmed |
| I-V4 | MVCC read pins have a bounded lifetime. Pin expiration revokes the snapshot guarantee. Pin TTL configurable per view descriptor, subject to cluster-wide maximum. Prevents long-running reads from blocking compaction/GC. | Confirmed |

---

## Tenant invariants

| ID | Invariant | Status |
|---|---|---|
| I-T1 | Tenants are fully isolated. No cross-tenant data access. No delegation tokens, no cross-tenant key sharing. | Confirmed |
| I-T2 | A tenant's resource consumption (capacity, IOPS, metadata ops) is bounded by quotas at org and workload levels. Project-level quotas optional. Org sets ceiling; workload gets allocation within it. | Confirmed |
| I-T3 | A tenant's keys are not accessible to other tenants or to shared system processes. | Confirmed |
| I-T4 | Cluster admin cannot access tenant config, logs, or data without explicit tenant admin approval. Zero-trust infra/tenant boundary. | Confirmed |

---

## Encryption / key invariants

| ID | Invariant | Status |
|---|---|---|
| I-K1 | No plaintext chunk is ever persisted to storage. | Confirmed |
| I-K2 | No plaintext payload is ever sent on the wire (between any components). | Confirmed |
| I-K3 | Log delta payloads (filenames, attributes, inline data) are encrypted with system DEK, wrapped with tenant KEK. System-visible headers (sequence, shard, hashed_key, operation type, timestamp) are cleartext or system-encrypted. | Confirmed |
| I-K4 | The system can enforce access to ciphertext without being able to read plaintext without tenant key material. | Confirmed |
| I-K5 | Crypto-shred (tenant KEK destruction) renders previously-accessible data unreadable. Physical GC runs separately when refcount = 0 AND no retention hold. | Confirmed |
| I-K6 | Key rotation does not lose access to data encrypted under prior keys until explicit cutover. Epoch-based: two epochs coexist during rotation window. Full re-encryption available as admin action. | Confirmed |
| I-K7 | Authenticated encryption is used everywhere. Unauthenticated encryption is never acceptable. | Confirmed |
| I-K8 | Keys are never logged, printed, transmitted in the clear, or stored in configuration files. | Confirmed |
| I-K9 | Staleness bounds: compliance tags set a non-overridable floor (minimum strictness). View descriptors set preference within that floor. Effective bound = max(view_preference, compliance_floor). | Confirmed |
| I-K10 | Chunk ID derivation: hash(plaintext) for default tenants (cross-tenant dedup active). HMAC(plaintext, tenant_key) for opted-out tenants (cross-tenant dedup impossible, zero co-occurrence leak). | Confirmed |

---

## Audit invariants

| ID | Invariant | Status |
|---|---|---|
| I-A1 | Audit log is append-only, immutable, system-wide. Same durability guarantees as the Log. | Confirmed |
| I-A2 | Tenant audit export: filtered to tenant's events + relevant system events. Delivered on tenant VLAN. Coherent enough for independent compliance demonstration. | Confirmed |
| I-A3 | Cluster admin audit view: system-level events only. Tenant-anonymous or aggregated per zero-trust boundary. | Confirmed |
| I-A4 | Audit log is a GC consumer: delta GC blocked until audit log has captured the relevant event. | Confirmed (subset of I-L4) |

---

## Consistency invariants

| ID | Invariant | Status |
|---|---|---|
| I-CS1 | CP for writes: no split-brain for regulated data. A write is not acknowledged until durable on a Raft majority. | Confirmed |
| I-CS2 | Bounded staleness for reads: acceptable per view descriptor, subject to compliance floor (I-K9). | Confirmed |
| I-CS3 | Federated sites are eventually consistent (async replication). No cross-site Raft. Per-site CP for writes. | Confirmed |

---

## Operational invariants

| ID | Invariant | Status |
|---|---|---|
| I-O1 | Shard split does not block writes to the existing shard during split. | Needs architect confirmation |
| I-O2 | Compaction operates on delta headers only; never decrypts tenant-encrypted payloads. | Confirmed |
| I-O3 | Stream processors cache tenant key material; are in the tenant trust domain. | Confirmed |
| I-O4 | Native client discovery must work without direct control plane access. Bootstrap via data fabric. | Confirmed (mechanism deferred to architect) |

---

## Integrity invariants

| ID | Invariant | Status |
|---|---|---|
| I-O5 | Compaction trusts hashed_key for merge ordering (no decryption). Explicit reconstruction (tenant-key-required, operator-triggered) available to verify integrity by decrypting and re-hashing. | Confirmed |
| I-K11 | Tenant KMS loss without tenant-maintained backups is unrecoverable data loss. Kiseki documents the requirement but provides no system-side escrow. Tenant controls and is responsible for their keys. | Confirmed |
| I-K12 | System key manager is an internal Kiseki service with its own HA/consensus. Unavailability blocks all chunk writes cluster-wide. Must be at least as available as the Log. | Confirmed |
| I-K13 | Data-fabric authentication is mTLS with per-tenant certificates signed by the Cluster CA. Optional second-stage auth via tenant IdP. No real-time auth server required on the data path. | Confirmed |
| I-K14 | Compression is off by default. Tenant opt-in for compress-then-encrypt with padding. Compliance tags may prohibit compression (e.g., HIPAA namespaces). | Confirmed |

---

## Time invariants

| ID | Invariant | Status |
|---|---|---|
| I-T5 | HLC is authoritative for ordering and causality. Wall clock is authoritative only for duration-based policies (retention, staleness, compliance deadlines, audit). No correctness decision depends on wall-clock accuracy. Dual clock model adapted from taba. | Confirmed |
| I-T6 | Nodes self-report clock quality (Ntp/Ptp/Gps/Unsync). Unsync nodes are flagged — staleness bounds involving their timestamps are unreliable. Drift detection uses HLC/wall-clock pairs. | Confirmed |
| I-T7 | Intra-shard ordering uses Raft sequence numbers (total order). Cross-shard causal ordering uses HLC. Cross-site ordering uses HLC with async sync via replication. | Confirmed |

---

## Authentication invariants

| ID | Invariant | Status |
|---|---|---|
| I-Auth1 | Data-fabric authentication is mTLS with per-tenant certificates signed by Cluster CA. No real-time auth server on data path. | Confirmed |
| I-Auth2 | Optional second-stage auth via tenant's own IdP/key manager for workload-level identity. | Confirmed |
| I-Auth3 | SPIFFE/SPIRE available as alternative to raw mTLS (maps to tenant hierarchy). | Confirmed |
| I-Auth4 | Cluster admin authenticates via IAM in Control Plane on management/control network (separate from data fabric). | Confirmed |

---

## Operational invariants (continued)

| ID | Invariant | Status |
|---|---|---|
| I-O6 | Maintenance mode sets cluster or specific shards to read-only. Write commands rejected with retriable error. Shard splits, compaction, and GC continue for in-progress operations but no new triggers fire from write pressure. | Confirmed |

---

## Backpass invariants (analyst backpass, 2026-04-17)

| ID | Invariant | Status |
|---|---|---|
| I-O7 | Runtime integrity monitor detects ptrace, /proc/pid/mem access, debugger attachment, core dump attempts on kiseki processes. Alerts cluster admin + tenant admin. Optional auto-rotate of keys on detection. | Confirmed |
| I-A5 | Audit GC safety valve: if tenant audit export stalls > threshold (default 24h), data shard GC proceeds with documented gap. Per-tenant configurable: backpressure mode throttles writes instead. | Confirmed |
| I-K15 | Crypto-shred cache TTL configurable per tenant within [5s, 300s], default 60s. Minimum 5s is hard engineering limit. | Confirmed |
| I-O8 | Writable shared mmap returns ENOTSUP with clear error message. Read-only mmap supported. | Confirmed |
| I-O9 | Client resilience via multi-endpoint resolution (DNS round-robin, seed list, multiple A records). Node failure → client reconnects to another node. | Confirmed |

---

## Open / deferred

- Maximum pin TTL defaults — needs operational experience
- Audit event enumeration (what system events are "relevant" for
  tenant export) — behavioral spec
