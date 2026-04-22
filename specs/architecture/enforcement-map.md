# Enforcement Map — Invariant → Enforcement Point

**Status**: Architect phase.
**Last updated**: 2026-04-22.

Every invariant from specs/invariants.md mapped to WHERE in the
architecture it gets enforced. Invariant without enforcement = violation.

---

## Log invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-L1 (total order within shard) | `kiseki-log` Raft leader | Raft assigns monotonic sequence numbers |
| I-L2 (durable on majority before ack) | `kiseki-log` Raft commit | openraft commit callback; no ack until majority persist |
| I-L3 (delta immutable once committed) | `kiseki-log` storage layer | Append-only SSTable format; no mutation API |
| I-L4 (GC requires all consumers advanced) | `kiseki-log` truncation | ConsumerWatermarks checked; min(all watermarks) = GC boundary |
| I-L5 (composition visible only after chunks durable) | `kiseki-composition` write path | Finalize step gates visibility; chunks confirmed before delta commit |
| I-L6 (hard shard ceiling) | `kiseki-log` shard monitor | Background task checks dimensions; triggers SplitShard |
| I-L7 (header/payload structural separation) | `kiseki-log` + `kiseki-crypto` | DeltaHeader + DeltaPayload structs; compaction reads headers only |
| I-L8 (cross-shard rename = EXDEV) | `kiseki-composition` rename handler | Check source/target shard; return EXDEV if different |
| I-L9 (inline payload immutable after write) | `kiseki-log` delta storage | inline_threshold_bytes changes apply prospectively only; existing deltas untouched |

## Chunk invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-C1 (chunks immutable) | `kiseki-chunk` write path | No update API; WriteChunk is create-or-dedup only |
| I-C2 (no GC while refcount > 0) | `kiseki-chunk` GC process | Check refcount before delete; atomic decrement + delete-if-zero |
| I-C2b (no GC while retention hold) | `kiseki-chunk` GC process | Check retention_holds list before delete |
| I-C3 (placement per affinity policy) | `kiseki-chunk` placement engine | Pool selection from view descriptor's affinity_pool |
| I-C4 (EC per pool) | `kiseki-chunk` pool config | DurabilityStrategy on AffinityPool; applied at write time |
| I-C5 (pool capacity thresholds) | `kiseki-chunk` placement engine | PoolHealth state machine; per-device-class thresholds; redirect within same class; ENOSPC at Full |
| I-C6 (EC params immutable per pool) | `kiseki-chunk` pool config | SetPoolDurability applies to new chunks only; existing chunks retain original EC |

## Device invariants (ADR-024)

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-D1 (auto-repair on device failure) | `kiseki-chunk` repair subsystem | Device failure event → identify affected chunks → EC reconstruct → re-place |
| I-D2 (device state transitions audited) | `kiseki-server` device monitor | State change → audit event to cluster audit shard with timestamp, reason, admin |
| I-D3 (auto-evacuation on SMART/sectors) | `kiseki-server` device monitor | SMART wear >90% (SSD) or >100 bad sectors (HDD) → background evacuation |
| I-D4 (EC fragments on distinct devices) | `kiseki-chunk` CRUSH placement | hash(chunk_id, frag_idx) → device; distinct-device constraint enforced |
| I-D5 (RemoveDevice requires evacuated) | `StorageAdminService` RPC handler | Precondition check: reject if device state ≠ Removed |

## Composition invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-X1 (composition belongs to one tenant) | `kiseki-composition` create | tenant_id set at creation; immutable |
| I-X2 (chunks respect dedup policy) | `kiseki-crypto` ChunkId derivation | derive_chunk_id checks DedupPolicy; sha256 or HMAC |
| I-X3 (reconstructible from deltas) | `kiseki-log` + `kiseki-composition` | Composition state = replay of shard deltas; no out-of-band state |

## View invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-V1 (derivable from shards alone) | `kiseki-view` rebuild | rebuild_view replays from position 0; no external deps |
| I-V2 (consistent prefix up to watermark) | `kiseki-view` stream processor | Sequential delta consumption; watermark tracks position |
| I-V3 (cross-view consistency per protocol) | `kiseki-view` + `kiseki-gateway-*` | ViewDescriptor.consistency enforced by stream processor tick rate |
| I-V4 (MVCC pin TTL) | `kiseki-view` pin manager | Background reaper expires pins past TTL |

## Tenant invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-T1 (full tenant isolation) | `kiseki-gateway-*` + `kiseki-client` auth | mTLS cert validation; tenant_id checked on every request |
| I-T2 (quota enforcement) | `control/pkg/policy` + gateway/client | Quota check before write; reject with QuotaExceeded |
| I-T3 (tenant keys not accessible to others) | `kiseki-crypto` + process isolation | Separate processes per tenant (ADR-012); mlock on key material |
| I-T4 (cluster admin needs approval) | `control/pkg/iam` | AccessRequest workflow; deny by default |
| I-T4c (pool mods audited to tenant) | `StorageAdminService` + `kiseki-audit` | Pool modifications logged to affected tenant's audit shard |

## Encryption invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-K1 (no plaintext at rest) | `kiseki-crypto` encrypt_chunk | Called before any write to disk; no bypass path |
| I-K2 (no plaintext on wire) | `kiseki-transport` TLS + `kiseki-crypto` | All transport encrypted; chunk data encrypted before send |
| I-K3 (delta payloads encrypted) | `kiseki-composition` → `kiseki-crypto` | Payload encrypted before AppendDelta; header in clear |
| I-K4 (system enforces without plaintext) | Architecture-wide | System operates on hashed_keys, chunk_ids, refcounts — never decrypts |
| I-K5 (crypto-shred renders unreadable) | `kiseki-keymanager` crypto_shred | KEK destroyed + invalidation broadcast (ADR-011) |
| I-K6 (rotation preserves old access) | `kiseki-keymanager` epoch mgmt | Old epoch master keys retained during rotation window |
| I-K7 (authenticated encryption) | `kiseki-crypto` AEAD | AES-256-GCM (FIPS); no unauthenticated mode |
| I-K8 (keys never logged/printed) | `kiseki-crypto` + zeroize | zeroize::Zeroizing wrapper; keys excluded from Debug/Display |
| I-K9 (compliance floor on staleness) | `kiseki-view` + `control/pkg/policy` | Effective staleness = max(view_pref, compliance_floor) |
| I-K10 (chunk ID derivation per policy) | `kiseki-crypto` derive_chunk_id | DedupPolicy enum selects sha256 vs HMAC |
| I-K11 (tenant KMS loss = data loss) | `kiseki-keymanager` | No escrow. Documentation in deployment guide. |
| I-K12 (system key manager HA) | `kiseki-keyserver` binary | Dedicated Raft group (ADR-007) |
| I-K13 (mTLS auth) | `kiseki-transport` + Cluster CA | TLS handshake validates cert chain to Cluster CA |
| I-K14 (compression off by default) | `kiseki-chunk` + `control/pkg/policy` | Compression flag per tenant; default false; compliance can prohibit |

## Time invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-T5 (HLC for ordering) | `kiseki-common` HLC impl | All delta timestamps use HLC; no wall-clock ordering decisions |
| I-T6 (clock quality reporting) | `kiseki-common` + node health | ClockQuality reported in node heartbeat; Unsync flagged |
| I-T7 (intra-shard Raft, cross-shard HLC) | `kiseki-log` + `kiseki-common` | Raft sequence within shard; HLC in delta timestamps for cross-shard |

## Authentication invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-Auth1 (mTLS on data fabric) | `kiseki-transport` TLS config | Require client cert; validate against Cluster CA |
| I-Auth2 (optional tenant IdP) | `kiseki-gateway-*` auth middleware | Second-stage validation if tenant config specifies IdP |
| I-Auth3 (SPIFFE alternative) | `kiseki-transport` SVID validation | Accept SPIFFE SVIDs as alternative to raw mTLS certs |
| I-Auth4 (cluster admin on control network) | `control/cmd/kiseki-control` | gRPC server binds to management network only |

## Audit invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-A1 (append-only, immutable) | `kiseki-audit` | Append-only Raft shard; no mutation API |
| I-A2 (tenant export coherent) | `control/pkg/audit` | Filter pipeline: tenant events + system event filter |
| I-A3 (cluster admin: tenant-anonymous) | `control/pkg/audit` | Anonymization layer on system metrics export |
| I-A4 (audit log is GC consumer) | `kiseki-log` + `kiseki-audit` | Audit watermark tracked per tenant audit shard (ADR-009) |
| I-A6 (tuning changes audited) | `StorageAdminService` | SetTuningParams → TuningParameterChanged event to cluster audit shard |

## Operational invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-O1 (shard split doesn't block writes) | `kiseki-log` split handler | Write buffer for new key range during split |
| I-O2 (compaction never decrypts) | `kiseki-log` compaction | Reads DeltaHeader only; DeltaPayload carried as opaque bytes |
| I-O3 (stream processors cache tenant key) | `kiseki-view` stream processor | CachedTenantKey with TTL in stream processor state |
| I-O4 (client discovery without control plane) | `kiseki-client` + `kiseki-server` discovery responder | Seed-based discovery on data fabric (ADR-008) |
| I-O5 (compaction trusts hash, explicit reconstruction) | `kiseki-log` compaction + verify command | Normal: merge by hashed_key. Verify: decrypt + re-hash with tenant key |
| I-O6 (maintenance mode = read-only) | `kiseki-log` shard state machine | ShardState::Maintenance rejects AppendDelta |

## Consistency invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-CS1 (CP for writes) | `kiseki-log` Raft | Raft majority required for commit |
| I-CS2 (bounded staleness for reads) | `kiseki-view` stream processor | Staleness check on materialization tick |
| I-CS3 (federated sites eventually consistent) | `control/pkg/federation` | Async replication; no cross-site Raft |

## Integrity invariants

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-O5 (trust hash, explicit reconstruction) | `kiseki-log` + `kiseki-crypto` | Compaction: hashed_key ordering. Verify: tenant-key-decrypt + re-hash |
| I-K11 (tenant KMS loss unrecoverable) | Documentation + audit trail | No code enforcement — by design |

## Workflow advisory invariants (ADR-020 — architect will refine)

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-WA1 (hints advisory only) | all data-path crates + `kiseki-advisory` | Data-path operations receive `Option<&OperationAdvisory>` (shared type from `kiseki-common`) and use it only for tuning preferences. Property test at Phase 11.5 exit: outcome equivalence with/without the bundle, byte-for-byte on durability/refcount/encryption outputs. |
| I-WA2 (advisory isolated from data path) | `kiseki-advisory` runtime + `kiseki-server` wiring | Separate tokio runtime with own thread pool (ADR-021 §1); separate gRPC listener; `AdvisoryLookup` is a bounded-deadline (≤500 µs) snapshot-read that returns `None` on miss/timeout/unavailable; no data-path Cargo dependency on `kiseki-advisory` enforced by crate layout (ADR-021 §2). |
| I-WA3 (workflow scoped to workload) | `kiseki-advisory` session manager + mTLS validator | WorkflowSession binds to mTLS peer identity at DeclareWorkflow; every subsequent op checks identity match |
| I-WA4 (client_id pinned per process) | `kiseki-client` startup + `kiseki-advisory` registrar | client_id = CSPRNG(≥128 bits) generated at process start; registrar binds `(client_id, mTLS identity)` at first use and rejects re-registration or identity-mismatch |
| I-WA5 (telemetry scoped + k-anonymous) | `kiseki-advisory` telemetry emitter | Ownership check before any metric computation; aggregator applies k-anonymity ≥5 over neighbour workloads before exposure |
| I-WA6 (telemetry/hint not existence oracle) | `kiseki-advisory` ingress | Ownership check runs before hint/telemetry processing; unauthorized-target and absent-target both return `SCOPE_NOT_FOUND` (application code) mapped to gRPC status `NOT_FOUND` (5); response shape, payload size, and latency are bucketed uniformly (ADR-021 §8); integration test at Phase 11.5 exit compares status-code and latency distributions |
| I-WA7 (hint budgets hierarchical) | `control/pkg/policy` + `kiseki-advisory` budget enforcer | Policy pipeline computes effective budget = min across levels; budget-exceeded → throttle + audit, local to offending workload |
| I-WA8 (advisory operations audited, batching allowed) | `kiseki-advisory` → `kiseki-audit` | Lifecycle + policy-violation events emitted per occurrence; `hint-accepted` and `hint-throttled` MAY be batched with per-second per-(workflow_id, reason) sampling guarantee and exact per-second counts preserved |
| I-WA9 (placement server-authoritative) | `kiseki-chunk` placement engine | Hint read as preference only; engine decision ordered: policy > durability > retention > hint |
| I-WA10 (opaque correlation IDs) | `kiseki-advisory` id generator | ≥128-bit CSPRNG; per-workload namespace; GC on End/TTL; handle treated as capability-reference validated by mTLS every call |
| I-WA11 (restricted advisory target fields) | `kiseki-advisory` schema + serde | Protobuf schema admits only enums, numeric metrics, and caller-owned opaque references (composition_id, view_id, workflow_id); shard_id, log_position, chunk_id, dedup_hash, node_id, device_id, rack_label are rejected at schema-validation with `forbidden_target_field`; lint rule on proto enforces at compile time |
| I-WA12 (opt-out with enabled/draining/disabled) | `control/pkg/policy` + `kiseki-advisory` gate | State machine per scope (org/project/workload, cluster-wide): enabled → draining (reject new Declare, continue existing) → disabled (audit-end active, reject all); all transitions audited; data path unaffected in every state |
| I-WA13 (phase monotonic, bounded) | `kiseki-advisory` session manager | phase_id strict-increase check; ring buffer of last-K phases; older phases → aggregate audit summary |
| I-WA14 (hints don't extend capability) | All data-path crates | Authorisation, quota, and retention checks run before consulting `OperationAdvisory`; the bundle only tunes parameters within the already-authorized outcome space. Enforced by code-review + per-crate property test in Phase 11.5. |
| I-WA15 (no covert channel via latency/size) | `kiseki-advisory` responder | Rejection path emits response after fixed delay bucket; telemetry messages padded to bucketed sizes; property test compares distributions |
| I-WA16 (hint payload size bound) | `kiseki-advisory` ingress | Per-hint schema validator: prefetch-tuple cap from policy (default 4096, max 16384); 4 KiB hard cap for other hint types; oversize → `hint_too_large` + audit |
| I-WA17 (declare-rate bound) | `kiseki-advisory` session manager + `control/pkg/policy` | Token-bucket rate limiter per `(workload_id)` at default 10/s; exceed → `declare_rate_exceeded` + audit |
| I-WA18 (prospective policy application) | `kiseki-advisory` session manager | Snapshot effective policy at `DeclareWorkflow` into the workflow record; `PhaseAdvance` re-validates against current policy; revocation → `profile_revoked` / `priority_revoked`; active telemetry subscriptions re-evaluated on policy narrowing, terminated with `SUBSCRIPTION_REVOKED` StreamWarning |
| workflow_ref header carriage | `kiseki-server` gRPC interceptor | Binary metadata key `x-kiseki-workflow-ref-bin` (16 bytes); lifted into request-scoped context; data-path protos unchanged (ADR-021 §3.a) |

## Small-file placement invariants (ADR-030)

| Invariant | Enforcement point | Mechanism |
|---|---|---|
| I-SF1 (per-shard inline threshold) | `kiseki-log` shard leader + `kiseki-server` control plane | Leader computes threshold = clamp(min(voter_budgets) / file_count, FLOOR, CEILING); stored in ShardConfig; replicated via Raft |
| I-SF2 (system disk capacity limits) | `kiseki-server` boot + periodic monitor | Boot: detect disk type/capacity via sysfs, compute budget. Periodic: report `NodeMetadataCapacity` via gRPC health. Hard-limit breach → out-of-band gRPC alert to leader (not Raft). Leader commits threshold=FLOOR with 2/3 majority. |
| I-SF3 (migration catch-up before promote) | `kiseki-log` Raft membership change | `add_learner` → wait until learner's last_applied matches leader's committed index → `change_membership`. Leader checks learner metrics before initiating promotion. |
| I-SF4 (placement rate limiting) | `kiseki-server` control plane / placement policy | Per-shard exponential backoff timer (2h floor, 24h cap). Cluster-wide semaphore: `max(1, num_nodes/10)` concurrent migrations. Timer persisted in control plane state. |
| I-SF5 (inline content in Raft, offloaded on apply) | `kiseki-log` state machine `apply()` + `build_snapshot()` | `apply()`: writes payload to `small/objects.redb`, keeps only header in memory. `build_snapshot()`: reads from redb, includes in snapshot. `install_snapshot()`: writes to redb on target. |
| I-SF6 (GC covers small/objects.redb) | `kiseki-log` `truncate_log` + `compact_shard` | When removing a delta referencing inline content, delete corresponding `small/objects.redb` entry by chunk_id. Periodic scrub detects orphans. |
| I-SF7 (Raft inline throughput guard) | `kiseki-log` shard leader write path | Sliding-window rate meter (10s). If inline_write_rate > `RAFT_INLINE_MBPS`, effective threshold drops to FLOOR. Rate check is pre-routing: before deciding inline vs chunk for each write. |
