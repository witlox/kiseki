# Error Taxonomy — Typed Errors Per Context

**Status**: Architect phase.
**Last updated**: 2026-04-17.

Every error is typed, categorized, and traceable to a spec artifact.
No generic errors. No silent swallowing.

---

## Error categories

| Category | Meaning | Caller action |
|---|---|---|
| **Retriable** | Transient failure; retry with backoff | Retry |
| **Permanent** | Cannot succeed; data loss or configuration error | Report to user/admin |
| **Security** | Authentication or authorization failure | Deny and audit |

---

## Per-context errors

### kiseki-log

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `ShardUnavailable` | Retriable | Raft quorum lost or leader election | F-C1, F-C2 |
| `MaintenanceMode` | Retriable | Shard in read-only maintenance | I-O6 |
| `WriteBuffering` | Retriable | Shard splitting, writes buffered | I-O1 |
| `SequenceGap` | Permanent | Log corruption detected | F-C3 |
| `CompactionFailed` | Retriable | SSTable merge error | F-D4 |
| `ConsumerStalled` | Retriable | Consumer watermark not advancing | I-L4 |

### kiseki-chunk

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `PoolFull` | Retriable | Affinity pool at capacity | F-D5 |
| `ChunkNotFound` | Permanent | Chunk ID doesn't exist | - |
| `ChunkCorrupted` | Permanent | AEAD auth tag verification failed | F-D5 |
| `ChunkLost` | Permanent | EC repair failed, insufficient parity | F-D5 |
| `RepairFailed` | Retriable | Repair in progress, retry later | F-I4 |
| `RetentionHoldActive` | Permanent | Cannot delete: hold in place | I-C2b |
| `RefcountUnderflow` | Permanent | Decrement below zero (invariant violation) | I-C2 |
| `QuorumLost { acks, required }` | Retriable | Cross-node fragment fan-out reached fewer than `min_acks` peers (Phase 16a, D-5). Maps to `RetriableError::ShardUnavailable` → S3 503 / NFS4ERR_DELAY with retry-after. | I-L2, phase-16-cross-node-chunks.md |

### kiseki-composition

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `NamespaceNotFound` | Permanent | Unknown namespace ID | - |
| `CompositionNotFound` | Permanent | Unknown composition ID | - |
| `CrossShardRename` | Permanent | Rename across shards → EXDEV | I-L8 |
| `ReadOnlyNamespace` | Permanent | Write to read-only namespace | - |
| `MultipartNotFound` | Permanent | Unknown upload ID | - |
| `MultipartIncomplete` | Retriable | Not all parts durable yet | I-L5 |
| `QuotaExceeded` | Permanent | Tenant/workload quota exceeded | I-T2 |
| `VersionNotFound` | Permanent | Requested version doesn't exist | - |

### kiseki-view

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `ViewNotFound` | Permanent | Unknown view ID | - |
| `ViewDiscarded` | Permanent | View was discarded, needs rebuild | - |
| `PinExpired` | Retriable | MVCC read pin TTL exceeded | I-V4 |
| `StalenessViolation` | Retriable | View behind staleness bound | I-K9 |
| `KeyUnavailable` | Retriable | Tenant KMS unreachable, cache expired | F-K1 |

### kiseki-gateway-nfs / kiseki-gateway-s3

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `AuthenticationFailed` | Security | mTLS cert invalid or missing | I-Auth1 |
| `TenantMismatch` | Security | Request credentials don't match gateway's tenant | I-T1 |
| `ProtocolError` | Permanent | Malformed NFS/S3 request | - |
| `OperationNotSupported` | Permanent | Unsupported POSIX op or S3 API | ADR-013, 014 |
| `EncryptionFailed` | Retriable | System key manager unreachable | F-I1 |

### kiseki-client

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `DiscoveryFailed` | Retriable | No seed endpoints reachable | ADR-008 |
| `TransportUnavailable` | Retriable | All transports failed | - |
| `FuseError` | Permanent | FUSE mount/unmount failure | - |
| `CacheCorrupted` | Retriable | Local cache inconsistency | - |

### kiseki-keymanager

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `KeyManagerUnavailable` | Retriable | Raft quorum lost | F-I1, I-K12 |
| `EpochNotFound` | Permanent | Requested epoch doesn't exist | - |
| `RotationInProgress` | Retriable | Key rotation not yet complete | I-K6 |
| `TenantKmsUnreachable` | Retriable | External KMS connectivity failure | F-K1 |
| `TenantKmsLost` | Permanent | KMS permanently unavailable | F-K2, I-K11 |
| `CryptoShredFailed` | Permanent | KEK destruction failed at KMS | - |

### kiseki-crypto

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `EncryptionFailed` | Permanent | AEAD encryption error (should be rare) | - |
| `DecryptionFailed` | Permanent | AEAD auth tag mismatch | - |
| `KeyDerivationFailed` | Permanent | HKDF failure (should never happen) | - |
| `UnwrapFailed` | Permanent | Tenant KEK can't unwrap material | - |

### control (Go)

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `TenantNotFound` | Permanent | Unknown org/project/workload ID | - |
| `QuotaExceeded` | Permanent | Cannot allocate beyond ceiling | I-T2 |
| `ComplianceTagRemovalBlocked` | Permanent | Tag has data under it | - |
| `AccessRequestPending` | Retriable | Waiting for tenant admin approval | I-T4 |
| `FederationPeerUnreachable` | Retriable | Cross-site sync failure | F-O3 |
| `FlavorUnavailable` | Permanent | No matching cluster capability | - |
| `ChildExceedsParentCeiling` | Permanent | Workload/project budget > parent ceiling | I-WA7 |
| `ProfileNotInParent` | Permanent | Child allow-list adds a profile absent from parent | I-WA7 |

### kiseki-advisory

Every advisory error is **scoped to the caller's own operation**. On
scope-violation paths, the canonical `ScopeNotFound` is returned with
identical response shape, payload size, and latency distribution as a
genuinely absent target (I-WA6, ADR-021 §8). Internal audit records
carry the true reason (I-WA8).

| Error | Category | Trigger | Spec |
|---|---|---|---|
| `AdvisoryDisabled` | Retriable | Scope is in `disabled` or `draining` state | I-WA12 |
| `AdvisoryUnavailable` | Retriable | Advisory runtime overloaded or restarting | I-WA2, F-ADV-1 |
| `ProfileNotAllowed` | Permanent | Profile not in effective allow-list at DeclareWorkflow | I-WA7 |
| `PriorityNotAllowed` | Permanent | Priority class exceeds policy-allowed maximum | I-WA14 |
| `RetentionPolicyConflict` | Permanent | `retention: temp` against composition with active retention hold | I-WA14 |
| `BudgetExceeded` | Retriable | Workload exceeded hints/sec, concurrent_workflows, telemetry_subscribers, or declared_prefetch_bytes | I-WA7 |
| `DeclareRateExceeded` | Retriable | DeclareWorkflow rate exceeded `workflow_declares_per_sec` | I-WA17 |
| `HintTooLarge` | Permanent | PrefetchHint tuple count > max_prefetch_tuples_per_hint, or other hint > 4 KiB | I-WA16 |
| `PrefetchBudgetExceeded` | Retriable | Aggregate declared prefetch bytes > budget | I-WA16, I-WA7 |
| `ForbiddenTargetField` | Permanent | Hint references a forbidden target field (shard, log position, chunk, dedup hash, node, device, rack) | I-WA11 |
| `PhaseNotMonotonic` | Permanent | `next_phase_id` ≤ current; or concurrent CAS lost | I-WA13 |
| `ProfileRevoked` | Permanent | Next PhaseAdvance after the profile was removed from allow-list | I-WA18 |
| `PriorityRevoked` | Permanent | Next PhaseAdvance after the priority class was disallowed | I-WA18 |
| `CertRevoked` | Security | mTLS cert revoked mid-stream | I-WA3, I-Auth1 |
| `ScopeNotFound` | Security | Caller is not authorized for the target, OR target does not exist (unified) | I-WA3, I-WA6 |

**Note on `AdvisoryUnavailable`**: this code is returned on the advisory
control path only (DeclareWorkflow, hints, subscriptions). It NEVER
propagates onto the data path — an `AdvisoryLookup::lookup()` timing
out or seeing advisory unavailable returns `None`, not an error
(ADR-021 §3, I-WA2).

**Note on `ScopeNotFound` canonicalization**: cross-tenant, cross-
workload, stolen-workflow_id, typo-in-composition_id, and
never-existed-composition_id all return this code. The human-readable
message is a constant (`"scope not found"`); any variation would be a
covert-channel (I-WA15). Internal audit records distinguish the causes
for forensic use.

**gRPC status code binding**: `WorkflowAdvisoryService` MUST map every
`AdvisoryErrorCode::SCOPE_NOT_FOUND` to gRPC status `NOT_FOUND`
(code 5). Using `PERMISSION_DENIED` (7) or `UNAUTHENTICATED` (16) for
authorization failures would leak the distinction through gRPC
trailers. Enforced by a Phase 11.5 integration test that compares
gRPC status code distributions across authorized-absent and
unauthorized-existing cases (ADR-021 §8). Full status-code mapping:

| Error | gRPC status |
|---|---|
| `AdvisoryDisabled`, `AdvisoryUnavailable` | `UNAVAILABLE` (14) |
| `ProfileNotAllowed`, `PriorityNotAllowed`, `RetentionPolicyConflict`, `HintTooLarge`, `ForbiddenTargetField`, `PhaseNotMonotonic`, `ProfileRevoked`, `PriorityRevoked` | `FAILED_PRECONDITION` (9) |
| `BudgetExceeded`, `DeclareRateExceeded`, `PrefetchBudgetExceeded` | `RESOURCE_EXHAUSTED` (8) |
| `CertRevoked` | `UNAUTHENTICATED` (16) — but the stream is torn down first; no per-message mapping |
| `ScopeNotFound` | `NOT_FOUND` (5) — ALWAYS, regardless of underlying cause |
