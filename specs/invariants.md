# Invariants — Kiseki

**Status**: Layer 2 complete. Updated for ADR-028 (External KMS Providers), ADR-029 (Raw Block Device Allocator), ADR-030 (Dynamic Small-File Placement), ADR-031 (Client-Side Cache), and ADR-033/034/035 (cluster topology, shard merge, node lifecycle — *architect designed, enforcement code pending*).
**Last updated**: 2026-04-25.

> **Spec-only marker**: Invariants tagged **`Spec-only`** below describe required system
> behavior whose enforcement code is incomplete or absent today. The audit performed
> 2026-04-25 found `auto_split::execute_split` is dead code, `NamespaceStore` is
> in-process memory only, and shard merge / node drain have no implementation.
> ADR-033/034/035 accepted with enforcement designs; implementer to wire.
> Do not remove the marker until enforcement is wired and tested.

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
| I-L5 | A composition is not visible to readers until all chunks referenced by its deltas are durable on at least `min_acks` peers per the pool's durability strategy. "Durable" here means "in stable storage OR in OS page cache on a node where the data has been replicated to N peers per the pool's durability strategy" — the chunk store may defer per-write fsync (group commit, see ADR-029 amendment 2026-05-04 + escalation `2026-05-04-group-commit-i-l5-durability-window.md`) provided the cross-node replication factor satisfies the pool's durability strategy. Normal writes: protocol enforces chunk-before-delta ordering. Bulk/multipart: finalize step gates reader visibility after all chunks confirmed durable. Single-node power loss within `KISEKI_CHUNK_FLUSH_INTERVAL_MS` of an unflushed write loses that write's local copy on the failed node only; the under-replication scrub re-replicates from surviving peers when the node returns. Whole-cluster correlated power loss within the flush window can lose up to one flush-interval of writes from page caches on all nodes — operators with stricter durability requirements must run with `sync_per_write=true` (a future per-pool config knob, ADR-024) or set the flush interval to 0. | Confirmed |
| I-L6 | Shards have a hard ceiling triggering mandatory split. Ceiling configurable across dimensions: delta count, byte size, write throughput. Any dimension exceeding its ceiling forces split. Thresholds set by cluster admin or control plane defaults. | Confirmed |
| I-L7 | Delta envelope has structurally separated system-visible header (cleartext/system-encrypted) and tenant-encrypted payload. Compaction operates on headers only; payloads are carried opaquely. | Confirmed |
| I-L8 | Cross-shard rename returns EXDEV. Shards are independent consensus domains; no 2PC. Applications handle via copy + delete. | Confirmed |
| I-L9 | A delta's inlined payload is immutable after write. `inline_threshold_bytes` changes apply prospectively only — existing deltas not re-evaluated. | Confirmed |
| I-L10 | A namespace is created with an *initial shard topology*: `initial_shards = max(min(3 × node_count_at_creation, 64), 3)` by default. Cluster admin sets cluster-wide defaults; tenant admin may override per namespace within admin-defined min/max. The initial Raft groups are placed by *best-effort round-robin* (I-L12) so leaders distribute across nodes from creation. | Spec-only (ADR-033) |
| I-L11 | The shards-per-node ratio for any namespace MUST NOT drop below the cluster *ratio floor* (default 1.5). Whenever a node-add event or namespace-creation event would leave the ratio below the floor, an auto-split is triggered for the affected namespace until the floor is restored. This trigger is in addition to the per-shard ceilings of I-L6; either trigger alone suffices to fire a split. | Spec-only (ADR-033) |
| I-L12 | Leader placement at shard creation, split, and merge is *best-effort round-robin*: the new shard's initial leader is chosen as the node currently hosting the fewest leaders for the namespace, with deterministic tie-break on node ID. No invariant constrains post-hoc balance — drift between rebalancing events is permitted. Operators may trigger explicit rebalance (out of scope for ADR-033). | Spec-only (ADR-033) |
| I-L13 | Two adjacent shards (by `hashed_key` range) are merged when (a) their combined utilization on every dimension (delta count, byte size, write throughput) is below the *merge threshold* (default 25% of split ceiling per dimension) for the *merge interval* (default 24 h), AND (b) the merge would not violate the I-L11 ratio floor. If either condition fails, no merge fires. Merge thresholds are configurable cluster-wide; tenant admin may override per namespace within the admin envelope. | Spec-only (ADR-034) |
| I-L14 | Shard merge preserves total order across the merged `hashed_key` range. A reader of the resulting shard sees a sequence of deltas that is consistent with the per-shard total orders of both inputs at the time of merge. Merge does not block writes (consistent with A-O1 / I-O1). The merge protocol records a `ShardMerged` event identifying the input shard IDs, the resulting shard ID, the merged range, and the merge HLC timestamp. | Spec-only (ADR-034) |
| I-L15 | The namespace-shard map (which shards constitute a namespace and which `hashed_key` range each owns) MUST be persisted in a Raft-replicated control-plane store. In-process memory ONLY is forbidden. Updates on namespace creation, shard split, and shard merge are applied atomically through the control plane's Raft group; the gateway and native client refresh their routing caches from this store. | Spec-only (ADR-033) |

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

## Node lifecycle invariants (ADR-035)

| ID | Invariant | Status |
|---|---|---|
| I-N1 | Every cluster node has a *node state* in `{Active, Degraded, Failed, Draining, Evicted}`. `Active`: healthy. `Degraded`: partial device failures or SMART warnings — still reachable, no new shard assignments. `Failed`: unreachable (heartbeat timeout). `Draining`: operator-initiated graceful removal. `Evicted`: terminal. Automatic transitions: Active↔Degraded (condition-based), Active/Degraded→Failed (heartbeat timeout), Failed→Active (recovery). Operator transitions: Active/Degraded/Failed→Draining (`DrainNode`), Draining→Active (`CancelDrain`, I-N7). Draining→Evicted is automatic when all voter replacements complete. No transition out of `Evicted` — re-adding requires a fresh node identity. | Spec-only (ADR-035) |
| I-N2 | A node in `Draining` state accepts no new leader assignments and MUST have leadership transferred off for every shard it currently leads before it can transition to `Evicted`. Leadership transfer uses the openraft membership primitive (no re-election). The new leader is selected per the I-L12 placement policy. | Spec-only (ADR-035) |
| I-N3 | For every shard the draining node holds a voter slot in, a replacement voter is added on a surviving node, caught up to the leader's committed index, and promoted to voter before the old voter is removed. The cluster MUST NOT operate below RF=3 for any shard at any intermediate state during a drain (consistent with I-CS1, ADR-026). | Spec-only (ADR-035) |
| I-N4 | A drain request is REFUSED at submission if completing it would leave any shard unable to satisfy I-N3 (e.g., a 3-node cluster cannot drain a node without first adding a replacement). The control plane returns `DrainRefused: insufficient capacity to maintain RF=N`. The operator must add a replacement node first, then re-issue the drain. | Spec-only (ADR-035) |
| I-N5 | Re-replication completes — replacement voters caught up and promoted for every affected shard — before the node is allowed to enter `Evicted`. The drain may proceed concurrently across shards, bounded by the I-SF4 cluster-wide migration concurrency cap (`max(1, num_nodes / 10)` simultaneous migrations) to avoid Raft instability. | Spec-only (ADR-035) |
| I-N6 | Node state transitions are recorded in the cluster audit shard with timestamp, reason, and admin identity. `Active → Draining`, `Draining → Evicted`, and any `DrainRefused` outcome are auditable events. Independent of device state transitions (I-D2). | Spec-only (ADR-035) |
| I-N7 | A drain MAY be cancelled by the operator while the node is in `Draining` and has not yet entered `Evicted`. Cancellation transitions the node back to `Active` (the only permitted reverse transition). Voter replacements that completed before cancellation are NOT rolled back — they remain in their new placements; pending replacements are aborted; leader assignments may be rebalanced later by explicit operator action. | Spec-only (ADR-035) |

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

## Small-file placement invariants (ADR-030)

| ID | Invariant | Status |
|---|---|---|
| I-SF1 | The inline threshold for a shard is the minimum affordable threshold across all nodes hosting that shard's voter set. Threshold stored in ShardConfig, replicated via Raft. Computation: `clamp(min(voter_budgets) / file_count_estimate, INLINE_FLOOR, INLINE_CEILING)`. | Confirmed |
| I-SF2 | System disk metadata usage must not exceed `hard_limit_pct` (default 75%) of system partition capacity. Exceeding `soft_limit_pct` (default 50%) triggers threshold reduction. Exceeding hard limit forces threshold to `INLINE_FLOOR` and emits alert. Capacity alerts use out-of-band gRPC health reports, not Raft, so full-disk nodes can signal without writing Raft entries. | Confirmed |
| I-SF3 | Shard migration via Raft membership change must not proceed until the target node has fully caught up (learner state matches leader's committed index). Old voter remains in membership until new voter is promoted. | Confirmed |
| I-SF4 | Placement change rate per shard follows exponential backoff (2h floor, 24h cap). Backoff resets never go below 2h floor, even on workload profile changes. Cluster-wide concurrent migrations bounded by `max(1, num_nodes / 10)`. | Confirmed |
| I-SF5 | Inline content is carried in Raft log entries and offloaded to `small/objects.redb` on state machine apply. Snapshots include inline content read from redb. No inline content is held in the in-memory state machine after apply. This ensures snapshot transfer to learners/restarted nodes includes all inline data. | Confirmed |
| I-SF6 | GC (`truncate_log`, `compact_shard`) must delete corresponding entries from `small/objects.redb` when removing deltas that reference inline objects. Orphan redb entries are a capacity leak. Periodic scrub detects orphans. | Confirmed |
| I-SF7 | Per-shard Raft inline throughput must not exceed `KISEKI_RAFT_INLINE_MBPS` (default 10 MB/s). When exceeded, effective inline threshold drops to `INLINE_FLOOR` until rate subsides. Prevents inline data from starving metadata-only Raft operations during write storms. | Confirmed |

---

## Client-side cache invariants (ADR-031)

| ID | Invariant | Status |
|---|---|---|
| I-CC1 | A chunk in pinned or organic mode is served from cache if and only if (a) the chunk was fetched from canonical and verified by chunk_id content-address match (SHA-256) at fetch time, and (b) no crypto-shred event has been detected for that tenant since fetch. Chunks are immutable in canonical (I-C1); therefore a verified chunk remains correct indefinitely absent crypto-shred. | Confirmed |
| I-CC2 | Cached plaintext is overwritten with zeros (zeroize) before deallocation, eviction, or cache wipe. File-level: overwrite contents before unlink. Memory-level: `Zeroizing<Vec<u8>>` for L1 entries. This provides logical-level erasure; physical-level erasure on flash storage requires hardware encryption (OPAL/SED). | Confirmed |
| I-CC3 | File→chunk_list metadata mappings are served from cache only within the configured TTL (default 5s). After TTL expiry, the mapping must be re-fetched from canonical. Within the TTL window, the cached mapping is authoritative: it may serve data for files that have since been modified or deleted in canonical. This is the sole freshness window in the cache design — chunk data itself has no TTL. | Confirmed |
| I-CC5 | Metadata TTL is the upper bound on read staleness. A file modified or deleted in canonical is visible to a caching client within at most one metadata TTL period (default 5s). | Confirmed |
| I-CC6 | Cached entries remain authoritative across fabric disconnects shorter than `max_disconnect_seconds` (default 300s). Beyond this threshold, the entire cache (L1 + L2) is wiped. Disconnect defined as: no successful RPC to any canonical endpoint for the threshold duration. Background heartbeat RPCs (every 60s) maintain the `last_successful_rpc` timestamp. | Confirmed |
| I-CC7 | Any local cache error (L2 I/O failure, CRC32 mismatch, metadata lookup failure) bypasses to canonical unconditionally. The cache never serves data it cannot verify. | Confirmed |
| I-CC8 | The cache is ephemeral. On process start, the client either creates a new L2 pool (wiping orphaned pools detected via flock) or adopts an existing pool via `KISEKI_CACHE_POOL_ID`. A `kiseki-cache-scrub` service runs on node boot and periodically to clean orphaned pools from crashed processes. | Confirmed |
| I-CC9 | When effective cache policy is unreachable at session start, the client operates with conservative defaults (cache enabled, organic mode, 10 GB ceiling, 5s metadata TTL). Policy is fetched via data-path gRPC (primary), gateway (secondary), persisted last-known (tertiary), or conservative defaults (fallback). | Confirmed |
| I-CC10 | Cache policy changes apply to new sessions only. Active sessions continue under session-start policy (consistent with I-WA18). | Confirmed |
| I-CC11 | Staged chunks are fetched from canonical, verified by content-address, and stored with pinned retention as a point-in-time snapshot. The staged version is immutable in the cache regardless of canonical updates. To pick up updates, the user must explicitly release and re-stage. Staging enumerates directory trees recursively up to `max_staging_depth` (10) and `max_staging_files` (100,000). | Confirmed |
| I-CC12 | On crypto-shred event, all cached plaintext for the affected tenant is wiped from L1 and L2 with zeroize. Detection via periodic key health check (default 30s), advisory channel notification, or KMS error on next operation. Maximum detection latency bounded by `min(key_health_interval, max_disconnect_seconds)`. | Confirmed |
| I-CC13 | L2 cache entries are protected by a CRC32 checksum computed at insert time and stored as a 4-byte trailer. On L2 read, the CRC32 is verified before serving. Mismatch triggers bypass to canonical and L2 entry deletion. | Confirmed |

## pNFS invariants (ADR-038)

| ID | Invariant | Status |
|---|---|---|
| I-PN1 | A pNFS file handle (`fh4`) accepted by a Data Server is valid only if (a) the HMAC-SHA256 over `tenant_id ‖ namespace_id ‖ composition_id ‖ stripe_index ‖ expiry_ms` (truncated to 16 bytes) matches the per-cluster MAC key derived via `HKDF-SHA256(master_key, info=b"kiseki/pnfs-fh/v1")`, AND (b) `expiry_ms` has not elapsed by the DS's local wall clock. Failure → NFS4ERR_BADHANDLE. fh4 is not a secret; it is an authenticator. Per ADR-039, MDS issues a single layout segment with N mirrors; every mirror's fh4 carries `stripe_index = 0` (whole-segment FH) and the DS resolves the byte range from the READ/WRITE op offset. | Proposed |
| I-PN2 | A Data Server holds **no persistent state**. It validates a fh4 + delegates to `GatewayOps::read`/`write` for the indicated `(composition_id, byte_range)`. DS crash recovery is a no-op: subsequent client RPCs re-enter with the same fh4. All open-state, lock-state, and layout-state lives on the MDS (RFC 8435 tightly_coupled mode). | Proposed |
| I-PN3 | A Data Server returns plaintext to clients (LAYOUTIOMODE4_READ) and receives plaintext from clients (LAYOUTIOMODE4_RW). The decrypt/encrypt boundary is the DS's `GatewayOps::read`/`write` call — identical to the existing MDS path. All stripes for a single composition share the composition's DEK; no per-stripe keys exist. | Proposed |
| I-PN4 | A LAYOUTGET response is valid for at most `layout_ttl_seconds` (default 300s) regardless of whether LAYOUTRECALL is delivered. Stale-routing risk after split/merge/drain is bounded by this TTL. LAYOUTRECALL is best-effort acceleration of invalidation, not the safety mechanism. | Proposed |
| I-PN5 | LAYOUTRECALL must fire within 1 sec on any of: ADR-035 node-state transition into `Drain`, ADR-033 shard split commit, ADR-034 shard merge commit, fh4 MAC key rotation, or composition deletion. Recall failure does not violate safety (I-PN4 bounds staleness), but a recall-fire SLA exists for layout responsiveness. | Proposed |
| I-PN6 | A pNFS layout is issued only for compositions whose namespace shard map (ADR-033) places at least one shard on the issuing MDS. Layouts referencing nodes outside the namespace's `active_node_set` are forbidden. Drained nodes (ADR-035) are excluded from new layouts but continue serving in-flight ones until I-PN5 recall fires. | Proposed |
| I-PN7 | Both NFS listeners (MDS at `nfs_addr`, DS at `ds_addr` default `:2052`) terminate **NFS-over-TLS (RFC 9289)** using the existing Cluster CA (ADR-009) by default. A plaintext fallback exists only when **both** `[security].allow_plaintext_nfs = true` (config) **and** `KISEKI_INSECURE_NFS=true` (env) are set; in that mode the operator has accepted the documented fh4-replay risk (ADR-038 §D4.2/§D4.4), the layout TTL is auto-halved to 60s, the configuration is rejected if more than one tenant maps to the listener, and a `SecurityDowngradeEnabled` audit event is emitted on every boot. The DS port answers a strict op subset: `EXCHANGE_ID`, `CREATE_SESSION`, `DESTROY_SESSION`, `PUTFH`, `READ`, `WRITE`, `COMMIT`, `GETATTR` only. All other ops return `NFS4ERR_NOTSUPP`. | Proposed |
| I-PN8 | The MDS layout cache is bounded: at most `layout_cache_max_entries` (default 100,000) live entries at any time. A sweeper task running every `layout_cache_sweep_interval_seconds` (default 75s, = `layout_ttl/4`) removes entries past their TTL. On capacity hit, eviction is LRU-on-issuance (smallest `issued_at_ms` first). Routine TTL eviction does NOT fire LAYOUTRECALL — clients re-LAYOUTGET on next op. A cache hit short-circuits the LAYOUTGET path **only when the cached layout's `(offset, length)` segment fully covers the requested range**; partial-coverage requests fall through to a fresh layout build and re-cache. | Proposed |
| I-PN9 | Topology events that affect outstanding layouts (drain, shard split/merge, composition deletion, fh4-MAC key rotation) are delivered to MDS LayoutManagers via the control-plane `TopologyEventBus` (ADR-038 §D10), emitted **after** the corresponding control-Raft commit. Subscriber lag (broadcast channel overflow) does not break safety: lag triggers a full layout-cache invalidation on the affected MDS, and clients re-issue LAYOUTGET. The 5-min I-PN4 TTL remains the ultimate safety net even if every event-bus subscription fails. | Proposed |

---

## Composition persistence invariants (ADR-040)

| ID | Invariant | Status |
|---|---|---|
| I-CP1 | A persistent `CompositionStore` advances `meta.last_applied_seq` only as part of the same redb transaction that applies the corresponding state changes (or no-ops, per I-CP6). A crash between batches loses at most one batch's worth of work; on restart the hydrator resumes from the durably-committed `last_applied_seq + 1`. | Proposed |
| I-CP2 | At most one composition hydrator runs per node at any time. Enforced by the runtime spawn; concurrent writers would also be serialized by redb's single-writer transaction model, but we don't rely on that — the runtime is the gate. | Proposed |
| I-CP3 | Every persisted composition record is `[1 byte: schema_version][postcard payload]`. Decoders that see an unknown version return a typed `SchemaTooNew` error rather than crashing or interpreting the payload as a different schema (per ADR-004). | Proposed |
| I-CP4 | The gateway's read path looks up compositions through the persistent store's LRU cache; cache hits do not acquire the redb transaction. Cache invalidation happens inside the same critical section as the redb commit, so a cache hit always reflects state at-or-after the last commit. | Proposed |
| I-CP5 | When the openraft log compaction window advances past `meta.last_applied_seq`, the hydrator detects the gap via **positive evidence** — either (a) a `read_deltas` response whose first delta has `sequence > last_applied + 1`, or (b) an empty `read_deltas` response combined with `earliest_visible_seq > last_applied + 1` — per ADR-040 §D6.3 (amended 2026-05-19, issue #87). The hydrator MUST NOT enter halt mode on weaker signals: in particular, `shard_health.tip > last_applied` with empty `read_deltas` is **NOT** evidence of compaction (it fires on snapshot install and on transient races between `append_delta_inner`'s `tip++` and `deltas.push` on a sibling shard / fresh shard with `last_applied=0`) and must be treated as steady-state until the next poll. On a genuine gap the hydrator emits one throttled `tracing::error!` per minute, sets `kiseki_composition_hydrator_stalled = 1`, stops polling for that shard. Existing reads continue to be served from the persistent store. The operator's recovery action is to drop the node's metadata redbs and restart; the persistent store re-hydrates from the next openraft snapshot. (Until the deferred snapshot-bundle ADR lands, this is the only correct behavior.) | Proposed |
| I-CP5b | The halt flag from I-CP5 is **per-shard**, not node-global (amended 2026-05-19, issue #87 PR-2). One shard's hydrator tripping its compaction-gap halt MUST NOT block reads of other shards' compositions on the same node. The persistent store keeps `halted: HashMap<ShardId, bool>` and the gateway's read-path 503 short-circuit looks up `halted(shard_id_for(composition_id))` rather than a single global flag. Rationale: production incident 2026-05-19 (issue #87) demonstrated that the prior node-global flag amplifies a per-shard transient into a cluster-wide read 503 — a self-inflicted DoS that defeats the fail-safe intent. With per-shard halts, reads of other shards on the node continue normally, and the operator can clear a single shard's halt via the admin RPC without nuking the whole node's metadata. | Proposed |
| I-CP6 | The hydrator advances `last_applied_seq` past delta `D` if and only if one of: (a) the apply method returned `Ok`; (b) `D` is structurally un-applyable (bad payload length, unknown `OperationType`, decode error) — *permanent skip*, advances + warns + emits `kiseki_composition_hydrator_skip_total{reason}`; (c) `D` is a no-op for the hydrator (`Rename`/`SetAttribute`/`Finalize`) — silent advance. A *transient skip* (e.g. `update_at` returning `CompositionNotFound`, `create_at` returning `NamespaceNotFound`) does NOT advance; the hydrator retries on the next poll. After `KISEKI_HYDRATOR_TRANSIENT_RETRIES` consecutive transient skips (default 100, ≈ 10 s at 100 ms poll cadence), the skip is promoted to permanent with `reason="exhausted_retries"` and the hydrator alarms via `kiseki_composition_hydrator_stalled`. | Proposed |

---

## Native gateway data service invariants (ADR-042 — in design)

| ID | Invariant | Status |
|---|---|---|
| I-NG1 | Every native gRPC `GatewayDataService` request is rejected at the proto-handler boundary if the request payload's `tenant_id` does not match the tenant identity carried in the client mTLS certificate's SAN URI. The match is byte-exact between the two values *after* both have been canonicalized per the *canonical SAN URI form* rules (see ubiquitous-language entry): scheme lowercased, authority lowercased, no trailing slash, percent-encoding decoded for unreserved characters per RFC 3986 §2.3, tenant-id segment in Unicode NFC, ASCII-only (any non-ASCII byte rejects, blocking IDN homograph attacks). Idempotency-key length is also validated here: keys longer than 64 bytes are rejected with `InvalidArgument` before reaching the gateway. | Proposed |
| I-NG2 | Object-flavored writes are atomic *commit-on-close*: a streaming write becomes visible to readers only after the server returns `Ok` from the `CommitStream` RPC. A stream interrupted before `CommitStream` is server-side garbage-collected within the idempotency-key dedup window (default 5 min) and the partial state is not visible at any point. CommitStream itself MUST NOT return `Ok` until every chunk referenced by the composition is durable on `min_acks` peers per the pool's durability strategy (preserves I-L5). | Proposed |
| I-NG3 | POSIX-flavored writes against an open inode handle become visible to readers only after `Fsync` returns `Ok`, matching POSIX `fsync(2)` semantics. After a successful fsync at lsn L on shard S, the same client's subsequent reads of inodes on shard S are routed to the shard's leader until the client observes that lsn L has been applied on a follower (per A-NG16); other clients reading from followers may briefly see pre-fsync state until follower catch-up, matching standard distributed-POSIX behavior. Writes that have not been fsync'd may be lost on a node hard reset within the configured durability window (`KISEKI_COMPOSITION_FLUSH_INTERVAL_MS`, `KISEKI_CHUNK_FLUSH_INTERVAL_MS`); multi-node R-3 / EC-4+2 + scrub recover the loss window per the durability contract in `docs/operations/durability.md`. | Proposed |
| I-NG4 | Cross-shard rename via the native API returns `EXDEV` (preserves I-L8). Within-shard cross-directory rename is atomic: source is no longer visible at the same instant the destination becomes visible at any reader. | Proposed |
| I-NG5 | Every native write request carries an `idempotency_key` (1..=64 bytes opaque, client-generated; longer rejected at the proto-handler per I-NG1). The server deduplicates by `(tenant_id, namespace_id, idempotency_key)` over a 5-minute window. The dedup state is Raft-replicated as a side-effect of the apply phase (each accepted write writes a small TTL-bounded row to a per-shard dedup table on every voter), so retries arriving after a leader change are deduplicated by the new leader and return the original outcome. | Proposed |
| I-NG6a | (Object verbs) `crypto_boundary` mode flips on a namespace are immediately observable for object-flavored verbs (every object Read is a fresh open). The mode in effect for a Read is the namespace's mode at the moment the Read RPC is dispatched at the server; subsequent flips do not retroactively affect the response. The keymanager validates DEK fetch requests against the namespace mode *at the moment of the corresponding Read* (the Read response carries an opaque DEK-fetch ticket whose contents commit to the at-Read-time mode), so a flag flip between Read and DEK-fetch does not break in-flight reads. | Proposed |
| I-NG6b | (POSIX verbs) `crypto_boundary` mode flips on a namespace only affect POSIX inode handles opened *after* the flip. Existing handles see the mode they opened under, achieved without server-side per-handle state by encoding the mode into the opaque, server-signed handle token returned at open time and presented on every subsequent op (see A-NG6). On flip, currently-open handles continue under the old mode until they are closed; the next open-on-the-namespace returns a token for the new mode. | Proposed |
| I-NG6c | When `crypto_boundary = TrustedCompute` is requested in an `UpdateNamespace` / `CreateNamespace` op, the server rejects the request with `InvalidArgument{crypto_shred_policy_required}` unless the same op also sets `crypto_shred_policy = best_effort`. The two flags MUST be set together; setting only one is a configuration error. (Resolves F-C1.) | Proposed |
| I-NG7 | Every native data-plane operation produces an audit event identical in shape to the existing S3 / NFS / FUSE audit events when it reaches the gateway dispatch path (auto-fires through `GatewayOps`); operations rejected at the proto-handler boundary (e.g., I-NG1 SAN/payload mismatch, I-NG6c missing dual-flag, I-NG11 stream cap exceeded, I-NG12 fenced write, I-NG14 lease-against-draining-node, etc.) emit a *security-failure* audit event via an explicit emit hook at the proto-handler boundary so no native op — accepted or rejected — escapes the audit pipeline. The `principal` field carries the cert SAN URI in canonical form; the `tenant_id`, `namespace_id`, and `workflow_ref` fields carry the validated request payload values (workflow_ref defaults to the literal `unattributed` when absent and tenant policy permits). | Proposed |
| I-NG8 | The native client's leader-routing follows the hybrid policy: client-side topology cache → direct dial of the shard leader → server-side proxy fallback when the dialed node responds `LeaderUnavailable` or `NotLeader{leader=X}`. The topology cache is invalidated by *both* (a) push-based — server-supplied `topology_version` (I-NG13) on every response triggers a refresh on mismatch — and (b) TTL-based fallback (default 30 s) as a safety net if the version channel regresses. Client retries with refreshed topology after at most one `LeaderUnavailable`; the server-side proxy fallback covers the window between a leader change and the client's next response with the new version. | Proposed |
| I-NG9 | Streaming threshold for both verb families: requests whose payload is ≤ inline-threshold (default 8 KiB, ADR-006) use unary RPCs; larger payloads use streaming RPCs. For object verbs the boundary is the entire object body. For POSIX verbs the boundary applies per `Write(inode, offset, bytes)` op: a single-call `Write` ≤ inline-threshold is unary, larger uses a streaming write within the same open handle. The per-stream cap is 64 MiB; above the cap the client must use the multipart-equivalent flow (`InitMultipart` / `PutPart` × N / `CompleteMultipart`) — POSIX writes split larger requests into multiple `Write` ops on the same inode handle. | Proposed |
| I-NG10 | The lease-based RMW path: a successful `AcquireLease(inode, mode=Write)` grants the client an exclusive-write token with a TTL (default 30 s, refreshable via `RenewLease`) AND a monotonic 64-bit *fencing token* (see I-NG12). Writes from the lease holder bypass per-op coordination but MUST attach the fencing token; concurrent `AcquireLease` from another client is rejected with `LeaseHeld{by, ttl_remaining_ms}` until the lease expires or is voluntarily released (`ReleaseLease`). On lease holder death (lease TTL expires without renewal), the server invalidates pending uncommitted writes for the inode and accepts new lease requests. Concurrent reads from non-lease-holders are permitted; readers see the inode's last fsync'd state, matching standard POSIX reader-while-writer semantics. | Proposed |
| I-NG11 | Concurrent in-flight streaming writes per tenant are bounded by a server-enforced cap (default 256 streams; configurable per tenant via control plane). Excess `OpenStream` requests are rejected with `ResourceExhausted{native_concurrent_stream_cap}` at the proto-handler boundary BEFORE any server-side staging buffer is allocated. The cap is enforced atomically against a per-tenant counter; on stream close (whether `CommitStream` or interrupt-cleanup) the counter decrements. (Resolves F-H6.) | Proposed |
| I-NG12 | Every successful `AcquireLease` and `RenewLease` returns a monotonic 64-bit fencing token (issuance is per-(tenant, namespace, inode); successive issuances are strictly greater). Every write issued under a lease MUST attach the lease's current fencing token. The server rejects any write whose attached fencing token is older than the inode's current lease's token (or any other lease's token if the lease was revoked + re-acquired) with `LeaseFenced{current_token}`. The fencing token is recorded in the audit event so operators can trace which holder issued each write. (Resolves F-H4.) | Proposed |
| I-NG13 | Native gRPC responses carry a `topology_version` value in the trailing metadata. The cluster increments the version monotonically on every shard-leader change, namespace-shard-map mutation, or split/merge event. Native clients compare the server-supplied version against their cached version on every response and refresh the topology cache on mismatch; the cache TTL (I-NG8) remains a safety-net. | Proposed |
| I-NG14 | Drain interaction (cross-context with ADR-035): when a node is in `Draining` state, leases held against inodes whose shard's leader runs on the draining node are *not forcibly revoked*; the drain protocol waits for outstanding leases to expire or be released (lease TTL bounds the additional drain latency to one TTL window). New `AcquireLease` requests against shards led by the draining node are accepted only if the lease TTL would expire before the drain quiesce window; otherwise rejected with `Unavailable{node_draining}`. (Resolves gate-0 F-M6.) | Proposed |
| I-NG15 | Handle tokens (POSIX `Open` → `handle_token`) and DEK-fetch tickets are HMAC-SHA256-signed under distinct, well-named keys derived from the system master key at process startup: `handle_token_signing_key = HKDF-SHA256(master_key, salt="kiseki-handle-token-v1", okm_len=32)` (gateway only); `dek_fetch_ticket_signing_key = HKDF-SHA256(master_key, salt="kiseki-dek-fetch-ticket-v1", okm_len=32)` (gateway signs, keymanager verifies — both derive from master_key). Tokens carry the issuing master_key_epoch; rotation invalidates older tokens after a configurable grace window (default 5 min). (Resolves gate-1 F-C1.) | Proposed |
| I-NG16 | Multi-chunk PUT atomicity: a `PutObjectStream` / `PutPart` whose payload spans multiple chunks (across multiple shards via rendezvous-hash placement) commits all-or-nothing. CommitStream returns `Ok` only when every referenced chunk is durable on `min_acks` peers per pool durability strategy (preserves I-L5). On any chunk's failure, CommitStream returns `Aborted{partial_chunk_failure, succeeded=N, total=M}`; staged chunks orphan with refcount=1 and the orphan-fragment scrub reclaims them on its standard cadence (24-hour TTL). No partial composition state is ever visible to readers (preserves I-NG2). (Resolves gate-1 F-C2.) | Proposed |
| I-NG17 | Handle tokens are bound to the cert SAN that minted them: the token's `cert_san_canonical` field is checked byte-equal against the canonical SAN URI of the connection's mTLS cert on every op. A token presented over a connection with a different SAN is rejected with `Unauthenticated{handle_token_san_mismatch}`, regardless of revocation status of either cert. Cert revocation invalidates handle tokens implicitly because the revoked cert cannot establish a new mTLS connection (per ADR-023). (Resolves gate-1 F-H1.) | Proposed |
| I-NG18 | When a request carries a `fencing_token` (lease-bound write), the server validates the fencing token BEFORE consulting the idempotency dedup table. A stale fencing token returns `LeaseFenced{current_token}` even if the request's `idempotency_key` matches a prior successful response in the dedup table. The dedup table records the request's fencing_token alongside the response_summary so audit / post-mortem can correlate. (Resolves gate-1 F-H4.) | Proposed |
| I-NG19 | The `workflow_ref` policy on writes follows the per-tenant flag `workflow_ref_required_for_writes` (set via the existing tenant store): when `true`, requests omitting `workflow_ref` are rejected at the proto-handler boundary with `InvalidArgument{workflow_ref_required}` and emit a security-failure audit event (no storage work runs). When `false`, the server substitutes the literal token `unattributed` for the audit event's `workflow_ref` field. The metric `kiseki_native_writes_unattributed_total{tenant}` exposes the substitution rate so operators can flag policy drift. (Resolves gate-1 F-H5.) | Proposed |
| I-NG20 | `GetTopology` responses are tenant-scoped: the `shards` list is filtered to only shards that own at least one of the caller's namespaces. Cross-tenant shard-leadership maps are never returned (resolves gate-1 F-H3). The `nodes` list remains complete because clients need to dial nodes for ops they may legitimately route to. The responding node's `topology_version` view may be stale by up to one Raft heartbeat (default 100 ms, ADR-026); clients treat the version as eventually consistent within this bound. | Proposed |
| I-NG21 | mTLS certs presented to native gateway clients MUST contain exactly one SAN URI matching the canonical kiseki tenant pattern (`spiffe://kiseki/tenant/<org_id>` after canonicalization per the canonical SAN URI form rules). Zero matches → `Unauthenticated{no_kiseki_tenant_san}`. Two or more matching URIs → `Unauthenticated{ambiguous_kiseki_tenant_san}`. Non-kiseki SAN URIs (e.g., DNS names for service-mesh interop) are ignored. (Resolves gate-1 F-M4.) | Proposed |
| I-NG22 | Inodes in a namespace are allocated by the composition store via a per-namespace monotonic 64-bit counter persisted in the redb `meta` table. **Inodes are never reused**, even after unlink + GC of the underlying composition. A handle token whose `inode` no longer references a live composition returns `Unauthenticated{inode_orphaned}` on the next op + emits `kiseki_native_handle_orphaned_inode_total{tenant}`. (Resolves gate-1 F-M3.) | Proposed |
| I-NG23 | `AcquireLease` and `ReleaseLease` produce audit events as gateway-dispatched ops do (per I-NG7). `RenewLease` does NOT emit per-event audit; instead the metric `kiseki_native_lease_renewals_total{tenant, namespace}` increments on each successful renewal. Renewals at default 1/3-TTL cadence on 10 000 active leases would otherwise produce 3.6 M audit events / hour; the counter conveys the same SLO signal at fixed cost. (Resolves gate-1 F-M2.) | Proposed |
| I-NG24 | The native streaming RPC handlers accept exactly ONE `first` oneof message per stream. Repeated `first` messages return `InvalidArgument{stream_already_initialized}` and abort the stream. Frames before the first `first` return `InvalidArgument{stream_not_initialized}`. The client SDK enforces single-`first` invariant; the server-side check defends against malicious or buggy clients re-injecting `ControlFields` mid-stream. (Resolves gate-1 F-M1.) | Proposed |
| I-NG25 | On the libfabric/cxi binding, every connection MUST present a valid `CxiAttestationEnvelope` as its first message before any other RPC frame. The server MUST close connections that fail attestation with `Unauthenticated{cxi_attestation_*}` before any application data is processed. The envelope's cert chain is validated against the cluster CA (same trust root mTLS bindings use); the canonical SAN extracted from the cert is byte-equal-checked against the envelope's `canonical_san` field; the ECDSA-P256 signature is verified; replay protection via timestamp ±30 s + per-(canonical_san, nonce) bloom filter (60 s window). cxi auth-key (cluster-scoped) authenticates the connection at the libfabric provider layer; the envelope adds the per-tenant identity layer that I-NG1 requires. (Resolves 2026-05-06 gate-1 H1 for the libfabric/cxi binding; ADR-042 §2.4.2.) | Proposed |
| I-NG26 | The cxi binding MUST enforce all four DoS-mitigation controls before §2.4.2 step-1 envelope decode runs: (a) per-source connection-establishment rate limit (`KISEKI_NATIVE_CXI_ATTEST_RATE_PER_SOURCE`, default 100 attempts / 60 s sliding window); (b) envelope size caps (≤16 KiB total, cert chain ≤8 KiB, signature ≤128 bytes); (c) failed-attestation per-source cooldown (`KISEKI_NATIVE_CXI_ATTEST_FAIL_THRESHOLD` consecutive failures park the source for `KISEKI_NATIVE_CXI_ATTEST_COOLDOWN_MS`); (d) bounded ECDSA-verify thread pool (default 16 workers × 64 queue depth, overflow returns `ResourceExhausted{cxi_attestation_queue_full}`). No cxi binding deployment ships without all four controls. (Resolves 2026-05-06 gate-1 round-2 R2-H1; ADR-042 §2.4.2.1.) | Proposed |
| I-NG27 | The native gateway's transport-binding architecture splits the **service contract** (Rust trait `NativeGatewayService` defining verbs + semantics) from the **binding** (wire-format adapter mapping the contract onto a specific over-the-wire protocol). At least four bindings MUST be available at runtime when their prerequisites are met: gRPC/h2, TCP-framed-postcard, ibverbs, libfabric. Each binding implements `NativeTransportServer<S>` + `NativeTransportClient<C>` traits; no binding redefines verbs or types. The runtime probes each compiled binding at startup via three sequential phases (probe → port-conflict check → listener-spawn); per-binding listener-spawn failure is non-fatal as long as at least one binding succeeds. Selection is per-edge (each client → node connection picks independently from the topology's `NodeBindings.bindings` list), with operator pin via `KISEKI_NATIVE_TRANSPORT={grpc\|tcp\|ibverbs\|libfabric\|auto}`. (Encodes ADR-042 §1, §2, §3 contract/binding architecture.) | Proposed |
| I-NG28 | When a node transitions to `Draining` (per ADR-035 drain protocol), the topology advertises `NodeBindings.state = Draining` with `bindings[*].drain_state = Some(DrainState { quiesce_window_remaining_ms, accepts_new_work })`. `accepts_new_work` is owned by `kiseki-control` (the drain coordinator) with two transition points: drain-start sets it `false`; quiesce expiry OR all-in-flight-done flips it `true` for `KISEKI_NATIVE_DRAIN_GRACEFUL_RELEASE_MS` before transitioning the node to `Evicted`. Topology version bumps on every transition. Clients reading any version always see consistent `(state, drain_state)` pairs; new opens / new lease acquisitions skip Draining nodes; in-flight work continues until completion or hard-close per the binding-specific cleanup discipline (ADR-042 §3.2.2). (Encodes ADR-042 §1.7 + §1.7.1 + §3.2.1 drain semantics.) | Proposed |

## FUSE wrapper invariants (libfuse-swap, ADR-043 §D2)

These invariants govern the `kiseki-fuse` safe wrapper around libfuse 3.x. Promoted from `specs/implementation/libfuse-swap.md` §"Safety contract" (round-3 closure of F3-H2 in `2026-05-09-adv-gate1-libfuse-swap-findings.md`). The gate-2 audit on `kiseki-fuse` validates each invariant in code; tests in §"Acceptance criterion 8" of the plan exercise them.

| ID | Invariant | Status |
|---|---|---|
| I-FUSE-1 | Every libfuse reply token (`ReplyAttr`, `ReplyData`, `ReplyEmpty`, `ReplyWrite`, `ReplyCreate`, `ReplyEntry`, `ReplyOpen`, `ReplyDirectory`, `ReplyStatfs`, `ReplyXattr`, `ReplyLock`) is consume-once: it MUST be passed to exactly one `.attr(...)` / `.data(...)` / `.ok()` / etc. call before being dropped. After the consume call, the token's `Drop` is a no-op. | `crates/kiseki-fuse/` |
| I-FUSE-2 | If a Rust handler returns or panics without consuming its reply token, the token's `Drop` impl issues `fuse_reply_err(req, EIO)` to libfuse, increments the Prometheus counter `kiseki_fuse_drop_without_consume_total{op}`, and logs at WARN level. Debug builds additionally panic. Guarantees libfuse never holds a request slot indefinitely. | `crates/kiseki-fuse/` |
| I-FUSE-3 | Reply tokens cross the libfuse-session-thread / tokio-task boundary ONLY through a wrapper-provided `tokio::sync::oneshot::{Sender, Receiver}<Result<ReplyResult, FuseError>>` bridge. The session thread blocks on `recv()` with a per-request bounded timeout (default 30 s, configurable). Reply tokens are `!Send` outside the bridge; sending one via direct `tokio::spawn` fails to compile. | `crates/kiseki-fuse/` |
| I-FUSE-4 | Cancellation of an async handler future MUST produce `EINTR` to the kernel (not a leaked slot or default-EIO). Plaintext buffers handled by the cancelled future MUST be zeroized on drop via the `ZeroOnCancel<Vec<u8>>` smart pointer the wrapper provides; this extends the existing `Zeroizing<Vec<u8>>` discipline used in `mem_gateway::DecryptCache` to the cancellation path. | `crates/kiseki-fuse/` |
| I-FUSE-5 | The libfuse session loop MUST run on a dedicated `std::thread` named `kiseki-fuse-session`, NOT on `tokio::task::spawn_blocking`. The dedicated thread isolates session work from kiseki's other blocking work (which competes for tokio's blocking-pool slots, default cap 512). | `crates/kiseki-fuse/` |
| I-FUSE-6 | The wrapper's `Filesystem` trait is `Send + Sync + 'static`. Reply-token lifetime is the tightly-constrained surface (per I-FUSE-1..3); the handler's own state is unconstrained beyond `Send + Sync`. | `crates/kiseki-fuse/` |
| I-FUSE-7 | The wrapper enforces `max_pending_ops` (default 1024) on the in-flight bridge map. When the cap is hit, new FUSE requests are immediately replied with `EAGAIN` rather than queued. Configurable via `KisekiFuseConfig::max_pending_ops`. | `crates/kiseki-fuse/` |
| I-FUSE-8 | If the libfuse session thread exits unexpectedly (panic, EIO from kernel `/dev/fuse`, signal), the wrapper detects this via the thread's join handle. Default behavior: abort the kiseki-client process (fail-fast, preserves the pre-swap process-crash shape). Operator-opt-in via `KisekiFuseConfig::auto_remount = true`: attempts a single re-mount with the same options; on second crash falls back to abort. | `crates/kiseki-fuse/` |

---

## Open / deferred

- Maximum pin TTL defaults — needs operational experience
- Audit event enumeration (what system events are "relevant" for
  tenant export) — behavioral spec
