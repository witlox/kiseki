# Invariants — Kiseki

**Status**: Layer 2 complete. Updated for ADR-028 (External KMS Providers) and ADR-029 (Raw Block Device Allocator).
**Last updated**: 2026-04-22.

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
| I-L9 | A delta's inlined payload is immutable after write. `inline_threshold_bytes` changes apply prospectively only — existing deltas not re-evaluated. | Confirmed |

---

## Chunk invariants

| ID | Invariant | Status |
|---|---|---|
| I-C1 | Chunks are immutable. New versions are new chunks. | Confirmed |
| I-C2 | A chunk is not GC'd while any composition references it (refcount > 0). | Confirmed |
| I-C2b | A chunk is not GC'd while a retention hold is active, regardless of refcount. Retention hold must be set before crypto-shred to prevent GC race. Ordering: set hold → crypto-shred → hold expires → GC eligible. | Confirmed |
| I-C3 | Chunks are placed according to affinity policy derived from the referencing composition's view descriptor. | Confirmed |
| I-C4 | Chunk durability strategy is per affinity pool. EC is the default. Replication (N-copy) available for pools where EC overhead is unacceptable. Pool-level policy set by cluster admin. | Confirmed |
| I-C5 | Pool writes are rejected when pool reaches Critical threshold (per-device-class: SSD 85%, HDD 92%). Pool redirection stays within same device class only. ENOSPC returned when pool is Full. | Confirmed |
| I-C6 | EC parameters (data_chunks, parity_chunks) are immutable per pool. `SetPoolDurability` applies only to new chunks. Existing chunks retain original EC config. Re-encoding requires explicit `ReencodePool` RPC. | Confirmed |
| I-C7 | All chunk data writes are aligned to the device's physical block size (auto-detected via `DeviceCharacteristics`). No unaligned I/O. Alignment enforced by `kiseki-block` `DeviceBackend` trait (ADR-029). | Confirmed |
| I-C8 | Allocation bitmap on each data device is the ground truth for space management. Free-list is a derived cache rebuilt on startup. Bitmap updates are journaled in redb (`device_alloc` table) before application to the on-device bitmap. Crash between journal and bitmap apply is recovered by replaying the redb journal (ADR-029). | Confirmed |

---

## Device invariants

| ID | Invariant | Status |
|---|---|---|
| I-D1 | Chunks on a failed device are automatically repaired from EC parity or replicas. Repair is triggered immediately on device failure detection. | Confirmed |
| I-D2 | Device state transitions (Healthy → Degraded → Evacuating → Failed → Removed) are recorded in the audit log with timestamp, reason, and admin identity (if manual). | Confirmed |
| I-D3 | Automatic evacuation is triggered when a device reports SMART wear >90% (SSD) or >100 bad sectors (HDD). Evacuation is background, cancellable by admin. | Confirmed |
| I-D4 | EC fragments are placed across distinct physical devices within a pool via deterministic hashing (CRUSH-like). No two fragments of the same chunk on the same device. | Confirmed |
| I-D5 | `RemoveDevice` rejects if device state is not `Removed` (post-evacuation). Evacuation must complete before physical removal. | Confirmed |

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
| I-T4c | Cluster admin modifications to pools containing tenant data are audit-logged to the affected tenant's audit shard. Tenant admin can review. | Confirmed |

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
| I-A6 | All tuning parameter changes via `SetTuningParams` are recorded in the cluster audit shard with parameter name, old value, new value, timestamp, and admin identity. | Confirmed |

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

## External KMS invariants (ADR-028)

| ID | Invariant | Status |
|---|---|---|
| I-K16 | Provider abstraction is opaque to callers. No access-control, correctness, or crypto decision depends on which TenantKmsProvider backend is selected. Provider failures are handled identically regardless of backend. | Confirmed |
| I-K17 | Wrap/unwrap operations include AAD (chunk_id) binding. A wrapped blob cannot be spliced from one envelope to another without authentication failure. Each provider maps AAD to its native mechanism (Vault context, KMS EncryptionContext, KMIP Correlation Value, PKCS#11 pParameter). | Confirmed |
| I-K18 | Tenant KMS provider is validated on configuration: connectivity test, wrap/unwrap round-trip, certificate chain (if mTLS). Validation failure prevents tenant activation. Invalid configurations are rejected atomically — no partial state. | Confirmed |
| I-K19 | Internal provider stores tenant KEKs in a separate Raft group from system master keys. Compromise of one group does not expose the other. Internal mode does not provide the full two-layer security guarantee of ADR-002 — operators with access to both groups have full access. | Confirmed |
| I-K20 | Provider migration (e.g., Internal → Vault) requires re-wrapping all existing envelopes. Migration is operator-initiated, background, audited, and preserves data availability throughout. Provider switch is atomic after 100% re-wrap. | Confirmed |

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

## Workflow advisory and client telemetry invariants

| ID | Invariant | Status |
|---|---|---|
| I-WA1 | Hints are advisory. No correctness, ACL, quota, dedup-safety, retention-hold, or placement-correctness decision depends on a hint being present, absent, accepted, rejected, or honoured. A data-path operation's outcome must be identical whether accompanying hints are obeyed, silently dropped, or arbitrarily replaced by an adversary on a side channel. | Confirmed |
| I-WA2 | The advisory subsystem is isolated from the data path. Failure, slowness, saturation, or complete outage of the advisory channel (hint handler, telemetry emission, advisory audit) must not block, delay, fail, or reorder any data-path operation. Loss of advisory context degrades only steering quality. | Confirmed |
| I-WA3 | A workflow belongs to exactly one workload. The `workflow_id` is unique within its owning workload and opaque outside it. Authorization is **per operation, not per stream**: every DeclareWorkflow, EndWorkflow, PhaseAdvance, hint submission, telemetry subscription, and telemetry message re-validates the caller's currently-valid mTLS identity against the workflow's owning workload. Stream-level TLS establishment is necessary but not sufficient. A stream may be torn down mid-session if the peer's certificate becomes invalid (revoked, rotated, expired). | Confirmed |
| I-WA4 | `client_id` is pinned to one native-client process instance and never reused across processes. Generated by the client as a ≥128-bit CSPRNG draw at process start. The advisory registrar binds `(client_id, mTLS identity)` at first use; subsequent requests must present both and are rejected on re-registration attempts or identity mismatch. A restarted process generates a new `client_id`. | Confirmed |
| I-WA5 | Telemetry feedback is scoped to the caller's authorization. Every value returned (saturation, locality class, lag, hotspot, prefetch effectiveness, QoS headroom) is computed over resources the caller is authorized to read and expresses no inference-usable information about other workloads, projects, orgs, or cluster-wide state. Cluster-level aggregates, if exposed at all, use k-anonymous neighbour bucketing (k configurable per policy, minimum 5). Under low-k conditions (fewer than k distinct neighbour workloads in the aggregation set) the response shape is unchanged: neighbour-derived components are set to a fixed sentinel value and the rest of the response is delivered exactly as in the populated case. Suppressing the response, omitting fields, or varying response size based on k is forbidden. | Confirmed |
| I-WA6 | Advisory requests are not existence or content oracles. A client cannot, through any hint submission or telemetry request, determine the existence, placement, size, access frequency, or any other property of compositions, chunks, shards, views, workflows, or tenants it is not authorized to observe. Ownership is checked before any advisory computation; an unauthorized target returns an error indistinguishable in code, payload structure, size, and latency distribution from an absent target. This guarantee applies uniformly to hints and telemetry. | Confirmed |
| I-WA7 | Hint budgets are enforced per workload within parent ceilings. Org/project/workload each declare bounds on hints/sec, concurrent workflows, phases per workflow, telemetry subscribers, and declared prefetch bytes. A child level may narrow (never broaden) its parent's ceiling. Exceeding a budget produces local degradation (throttle, reject, cap-to-budget) only for the offending workload; neighbours are unaffected. | Confirmed |
| I-WA8 | Advisory operations are audited. Lifecycle events (`declare-workflow`, `end-workflow`, `phase-advance`, `telemetry-subscribed`, `budget-exceeded`, policy-violation rejections such as `profile_not_allowed` / `priority_not_allowed` / `retention_policy_conflict`, and `scope_violation`) produce one audit event per occurrence on the tenant's audit shard (ADR-009) with the same durability and retention as all other tenant audit events. High-volume events (`hint-accepted`, `hint-throttled`) MAY be batched or sampled, provided that: at least one event per unique `(workflow_id, rejection_reason)` tuple is written per second when any such event occurs, and exact counts of accepted and throttled hints per second per workflow are preserved. Semantic phase tags and workflow IDs are tenant-scoped; cluster-admin views see opaque hashes only (consistent with I-A3, ADR-015). | Confirmed |
| I-WA9 | Placement remains server-authoritative. Affinity and locality hints are preferences; the placement engine may ignore or override any hint to satisfy I-C3 (policy-driven placement), durability (I-C4), retention holds (I-C2b), or pool constraints. Hint rejection reason strings returned to the caller must not disclose other tenants' pool utilisation, quota state, or placement details. | Confirmed |
| I-WA10 | Correlation identifiers are opaque and non-guessable. `workflow_id` is generated with ≥128 bits of entropy, never reused within a workload, never encodes tenant-hierarchy information, and is treated as a capability reference — mere knowledge of a neighbour's `workflow_id` grants no access because every advisory request is also authorized by mTLS tenant identity. Workflow handles GC on `End` or TTL expiry (default 1 h, configurable per workload, max 24 h). | Confirmed |
| I-WA11 | Advisory target fields are restricted. Every field in a hint or telemetry request that identifies a target is either (a) a caller-owned opaque reference (composition_id, view_id, workflow_id) that is verified to belong to the caller's workload before use, or (b) an enum-bucketed classification (profile, phase-tag-from-allowed-set, access-pattern, priority-class, retention-intent, affinity-pool-handle). Shard IDs, log positions, chunk IDs, dedup hashes, node IDs, device IDs, rack labels, and any other cluster-internal identifier are forbidden as advisory fields. The channel exposes no key material, no plaintext, and no raw delta payload. (Compatible with I-K1, I-K2, I-K8, I-X2, I-K10.) | Confirmed |
| I-WA12 | Advisory subsystem is opt-out with three explicit states: `enabled`, `draining`, `disabled`. Tenant admin may transition per org, project, or workload; cluster admin may transition cluster-wide. Transitions are forward-only during a single decision cycle: `enabled → draining` (new DeclareWorkflow returns `ADVISORY_DISABLED`; existing workflows continue until current phase ends or TTL expires; new hints in existing workflows are still processed) and `draining → disabled` (all hint processing and telemetry subscriptions end; active workflows are audit-ended; any data-path annotations are ignored). The reverse path `disabled → enabled` is permitted as a single transition for administrative reversal. Data-path operations are unaffected in every state. All transitions are auditable. | Confirmed |
| I-WA13 | Phase order is monotonic within a workflow. `PhaseAdvance` rejects non-increasing `phase_id` with `phase_not_monotonic`. A workflow has at most one active phase at any instant; concurrent `PhaseAdvance` calls for the same workflow are serialized by the advisory subsystem via compare-and-swap on `phase_id`, and the losing caller sees `phase_not_monotonic`. Phase history per workflow is bounded (last-K phases, K configurable, default 64); older phases are compacted to aggregate audit summaries. | Confirmed |
| I-WA14 | Hints do not extend tenant capabilities. A hint cannot cause an operation to succeed that would otherwise be rejected, cannot cross a namespace/workload/tenant boundary, cannot bypass a retention hold, cannot elevate a priority class beyond the workload's policy-allowed maximum, and cannot disable compression/encryption. The set of authorized outcomes is the set of outcomes without hints. | Confirmed |
| I-WA15 | Advisory timing and response structure are non-informative about neighbour workloads. Latency of a hint accept/reject decision, size of telemetry responses, and choice of error codes do not vary with neighbour-workload state in a way a caller can exploit as a covert channel. Where such variation is unavoidable (e.g., genuine pool saturation), it is coarsened to the bucket granularity defined in I-WA5. |  Confirmed |
| I-WA16 | Hint payload size is bounded. Per-hint limits: at most `max_prefetch_tuples_per_hint` (default 4096, policy-configurable maximum 16384) tuples per PrefetchHint; at most 4 KiB total serialized hint size for all other hint types. Violations return `hint_too_large` and are audited. Excess prefetch tuples are discarded, not truncated-and-accepted. | Confirmed |
| I-WA17 | Workflow declaration rate is bounded per workload. `workflow_declares_per_sec` (default 10, policy-configurable) caps the rate of new `DeclareWorkflow` calls. Exceeding the rate returns `declare_rate_exceeded`; the calling workload's concurrent workflow cap remains enforced independently. | Confirmed |
| I-WA18 | Runtime policy changes apply prospectively, not retroactively. An active workflow runs to completion (or TTL) under the profile, allow-lists, and budgets effective at its `DeclareWorkflow` moment. A policy change takes effect for the next `DeclareWorkflow` and for the next `PhaseAdvance` within existing workflows; if the new policy forbids the current phase's priority class or profile, that `PhaseAdvance` is rejected with `profile_revoked` or `priority_revoked` and the workflow continues on its current phase. Budget reductions apply prospectively from the next second. Active telemetry subscriptions are re-evaluated on policy narrowing (e.g., pool handle no longer authorized); revoked subscriptions receive a terminal `subscription-revoked` notification and are closed, with an audit event; the data path access to the revoked resource is governed independently by the data-path authorization stack. | Confirmed |
| I-WA19 | Pool handles are the sole advisory-layer reference to affinity pools. At `DeclareWorkflow`, the advisory subsystem returns the set of pool handles (with tenant-chosen opaque labels) the workload is authorized to target in hints. Handles are valid for the workflow's lifetime only, are never reused across workflows, and never equal or leak the cluster-internal pool identity. A handle for a pool decommissioned or de-authorized during the workflow's life returns `scope-not-found` on use (uniform with the general oracle invariant I-WA6). | Confirmed |

---

## Open / deferred

- Maximum pin TTL defaults — needs operational experience
- Audit event enumeration (what system events are "relevant" for
  tenant export) — behavioral spec
