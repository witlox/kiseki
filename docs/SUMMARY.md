# Summary

[Introduction](README.md)

---

# User Guide

- [Getting Started](guide/getting-started.md)
- [S3 API](guide/s3-api.md)
- [NFS Access](guide/nfs-access.md)
- [FUSE Mount](guide/fuse-mount.md)
- [Python SDK](guide/python-sdk.md)
- [Client Cache & Staging](guide/client-cache.md)

# Administration

- [Deployment](admin/deployment.md)
- [Configuration Reference](admin/configuration.md)
- [Cluster Management](admin/cluster-management.md)
- [Admin Dashboard](admin/dashboard.md)
- [Backup & Recovery](admin/backup-recovery.md)
- [Monitoring & Observability](admin/monitoring.md)
- [Key Management](admin/key-management.md)

# Architecture

- [System Overview](architecture/overview.md)
- [Bounded Contexts](architecture/bounded-contexts.md)
- [Data Flow](architecture/data-flow.md)
- [Encryption Model](architecture/encryption.md)
- [Raft Consensus](architecture/raft.md)
- [Transport Layer](architecture/transports.md)
- [Client-Side Cache (ADR-031)](architecture/client-cache.md)

# Security

- [Security Model](security/model.md)
- [Authentication](security/authentication.md)
- [Tenant Isolation](security/tenant-isolation.md)

# Operations

- [Troubleshooting](operations/troubleshooting.md)
- [Performance Tuning](operations/performance.md)
- [Performance Benchmarks](performance/README.md)
- [Capacity Planning](operations/capacity.md)

# API Reference

- [gRPC Services](api/grpc.md)
- [REST & Admin API](api/rest.md)
- [CLI Reference](api/cli.md)
- [Environment Variables](api/environment.md)

# Decisions

- [Architecture Decision Records](decisions/index.md)
  - [ADR-001: Pure Rust, No Mochi](decisions/adr/001-pure-rust-no-mochi.md)
  - [ADR-002: Two-Layer Encryption](decisions/adr/002-two-layer-encryption-model-c.md)
  - [ADR-003: System DEK Derivation](decisions/adr/003-system-dek-derivation.md)
  - [ADR-004: Schema Versioning](decisions/adr/004-schema-versioning-and-upgrade.md)
  - [ADR-005: EC and Chunk Durability](decisions/adr/005-ec-and-chunk-durability.md)
  - [ADR-006: Inline Data Threshold](decisions/adr/006-inline-data-threshold.md)
  - [ADR-007: System Key Manager HA](decisions/adr/007-system-key-manager-ha.md)
  - [ADR-008: Native Client Discovery](decisions/adr/008-native-client-discovery.md)
  - [ADR-009: Audit Log Sharding](decisions/adr/009-audit-log-sharding.md)
  - [ADR-010: Retention Hold Enforcement](decisions/adr/010-retention-hold-enforcement.md)
  - [ADR-011: Crypto-Shred Cache TTL](decisions/adr/011-crypto-shred-cache-ttl.md)
  - [ADR-012: Stream Processor Isolation](decisions/adr/012-stream-processor-isolation.md)
  - [ADR-013: POSIX Semantics Scope](decisions/adr/013-posix-semantics-scope.md)
  - [ADR-014: S3 API Scope](decisions/adr/014-s3-api-scope.md)
  - [ADR-015: Observability](decisions/adr/015-observability.md)
  - [ADR-016: Backup and DR](decisions/adr/016-backup-and-dr.md)
  - [ADR-017: Dedup Refcount Access Control](decisions/adr/017-dedup-refcount-access-control.md)
  - [ADR-018: Runtime Integrity Monitor](decisions/adr/018-runtime-integrity-monitor.md)
  - [ADR-019: Gateway Deployment Model](decisions/adr/019-gateway-deployment-model.md)
  - [ADR-020: Workflow Advisory](decisions/adr/020-workflow-advisory.md)
  - [ADR-021: Advisory Architecture](decisions/adr/021-advisory-architecture.md)
  - [ADR-022: Storage Backend redb](decisions/adr/022-storage-backend-redb.md)
  - [ADR-023: Protocol RFC Compliance](decisions/adr/023-protocol-rfc-compliance.md)
  - [ADR-024: Device Management](decisions/adr/024-device-management-and-capacity.md)
  - [ADR-025: Storage Admin API](decisions/adr/025-storage-admin-api.md)
  - [ADR-026: Raft Topology](decisions/adr/026-raft-topology.md)
  - [ADR-027: Single-Language Rust](decisions/adr/027-single-language-rust-only.md)
  - [ADR-028: External KMS Providers](decisions/adr/028-external-tenant-kms-providers.md)
  - [ADR-029: Raw Block Allocator](decisions/adr/029-raw-block-device-allocator.md)
  - [ADR-030: Small-File Placement](decisions/adr/030-dynamic-small-file-placement.md)
  - [ADR-031: Client-Side Cache](decisions/adr/031-client-side-cache.md)
  - [ADR-032: Async GatewayOps](decisions/adr/032-async-gateway-ops.md)
