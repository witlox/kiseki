# Architecture Decision Records

All architectural decisions are recorded as ADRs in
[`specs/architecture/adr/`](https://github.com/witlox/kiseki/tree/main/specs/architecture/adr).

---

## ADR index

| ADR | Title | Status |
|---|---|---|
| [ADR-001](../../specs/architecture/adr/001-pure-rust-no-mochi.md) | Pure Rust, No Mochi Dependency | Accepted |
| [ADR-002](../../specs/architecture/adr/002-two-layer-encryption-model-c.md) | Two-Layer Encryption Model (C) | Accepted |
| [ADR-003](../../specs/architecture/adr/003-system-dek-derivation.md) | System DEK Derivation (Not Storage) | Accepted |
| [ADR-004](../../specs/architecture/adr/004-schema-versioning-and-upgrade.md) | Schema Versioning and Rolling Upgrades | Accepted |
| [ADR-005](../../specs/architecture/adr/005-ec-and-chunk-durability.md) | Erasure Coding and Chunk Durability | Accepted |
| [ADR-006](../../specs/architecture/adr/006-inline-data-threshold.md) | Inline Data Threshold | Accepted |
| [ADR-007](../../specs/architecture/adr/007-system-key-manager-ha.md) | System Key Manager HA via Raft | Accepted |
| [ADR-008](../../specs/architecture/adr/008-native-client-discovery.md) | Native Client Fabric Discovery | Accepted |
| [ADR-009](../../specs/architecture/adr/009-audit-log-sharding.md) | Audit Log Sharding and GC | Accepted |
| [ADR-010](../../specs/architecture/adr/010-retention-hold-enforcement.md) | Retention Hold Enforcement Before Crypto-Shred | Accepted |
| [ADR-011](../../specs/architecture/adr/011-crypto-shred-cache-ttl.md) | Crypto-Shred Cache Invalidation and TTL | Accepted |
| [ADR-012](../../specs/architecture/adr/012-stream-processor-isolation.md) | Stream Processor Tenant Isolation | Accepted |
| [ADR-013](../../specs/architecture/adr/013-posix-semantics-scope.md) | POSIX Semantics Scope | Accepted |
| [ADR-014](../../specs/architecture/adr/014-s3-api-scope.md) | S3 API Compatibility Scope | Accepted |
| [ADR-015](../../specs/architecture/adr/015-observability.md) | Observability Contract | Accepted |
| [ADR-016](../../specs/architecture/adr/016-backup-and-dr.md) | Backup and Disaster Recovery | Accepted |
| [ADR-017](../../specs/architecture/adr/017-dedup-refcount-access-control.md) | Dedup Refcount Metadata Access Control | Accepted |
| [ADR-018](../../specs/architecture/adr/018-runtime-integrity-monitor.md) | Runtime Integrity Monitor | Accepted |
| [ADR-019](../../specs/architecture/adr/019-gateway-deployment-model.md) | Gateway Deployment Model | Accepted |
| [ADR-020](../../specs/architecture/adr/020-workflow-advisory.md) | Workflow Advisory & Client Telemetry | Accepted |
| [ADR-021](../../specs/architecture/adr/021-advisory-architecture.md) | Workflow Advisory Architecture | Accepted |
| [ADR-022](../../specs/architecture/adr/022-storage-backend-redb.md) | Storage Backend -- redb (Pure Rust) | Accepted |
| [ADR-023](../../specs/architecture/adr/023-protocol-rfc-compliance.md) | Protocol RFC Compliance Scope | Accepted |
| [ADR-024](../../specs/architecture/adr/024-device-management-and-capacity.md) | Device Management, Storage Tiers, and Capacity Thresholds | Accepted |
| [ADR-025](../../specs/architecture/adr/025-storage-admin-api.md) | Storage Administration API | Accepted |
| [ADR-026](../../specs/architecture/adr/026-raft-topology.md) | Raft Topology -- Per-Shard on Fabric (Strategy A) | Accepted |
| [ADR-027](../../specs/architecture/adr/027-single-language-rust-only.md) | Single-Language Implementation -- Rust Only | Accepted |
| [ADR-028](../../specs/architecture/adr/028-external-tenant-kms-providers.md) | External Tenant KMS Providers | Accepted |
| [ADR-029](../../specs/architecture/adr/029-raw-block-device-allocator.md) | Raw Block Device Allocator | Accepted |
| [ADR-030](../../specs/architecture/adr/030-dynamic-small-file-placement.md) | Dynamic Small-File Placement and Metadata Capacity Management | Accepted |
| [ADR-031](../../specs/architecture/adr/031-client-side-cache.md) | Client-Side Cache | Accepted |

---

## ADR template

New ADRs follow this structure:

```markdown
# ADR-NNN: Title

**Status**: Proposed | Accepted | Superseded by ADR-XXX
**Date**: YYYY-MM-DD
**Context**: Why this decision is needed.

## Decision

What was decided and why.

## Consequences

What changes as a result. Trade-offs accepted.

## Alternatives considered

What else was evaluated and why it was rejected.
```

---

## Key decisions by topic

### Language and architecture
- ADR-001: Pure Rust (no Mochi dependency)
- ADR-027: Single-language Rust (Go control plane replaced)
- ADR-022: redb as storage backend (pure Rust, no RocksDB)

### Encryption
- ADR-002: Two-layer encryption model (system DEK + tenant KEK)
- ADR-003: HKDF-based DEK derivation (not per-chunk storage)
- ADR-011: Crypto-shred cache invalidation TTL
- ADR-028: External tenant KMS providers (Vault, KMIP, AWS KMS, PKCS#11)

### Consensus and replication
- ADR-007: System key manager HA via Raft
- ADR-026: Per-shard Raft groups on fabric (Strategy A)
- ADR-009: Audit log sharding and GC

### Storage
- ADR-005: Erasure coding and chunk durability
- ADR-006: Inline data threshold
- ADR-029: Raw block device allocator
- ADR-030: Dynamic small-file placement

### Protocols and access
- ADR-008: Native client fabric discovery
- ADR-013: POSIX semantics scope
- ADR-014: S3 API compatibility scope
- ADR-019: Gateway deployment model
- ADR-023: Protocol RFC compliance scope

### Operations
- ADR-015: Observability contract
- ADR-016: Backup and disaster recovery
- ADR-024: Device management and capacity thresholds
- ADR-025: Storage administration API

### Advisory
- ADR-020: Workflow advisory and client telemetry
- ADR-021: Workflow advisory architecture

### Client
- ADR-031: Client-side cache
