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
