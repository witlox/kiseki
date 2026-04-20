# Fidelity Index — Kiseki (R10 Update)

**Checkpoint**: 2026-04-20 (post-R9 remediation)
**Previous**: 2026-04-19 (post-BDD wiring)

## Per-Crate Status

| Crate | Status | Tests | Confidence | Validated |
|-------|--------|-------|------------|-----------|
| kiseki-common | DONE | 14 | HIGH | HLC ordering, boundary tests, property tests, checked_next |
| kiseki-crypto | DONE | 33 | HIGH | AEAD, HKDF, envelope, chunk ID, mlock, padding overflow |
| kiseki-proto | DONE | 6 | HIGH | Protobuf roundtrip |
| kiseki-raft | DONE | 7 | HIGH | Generic MemLogStore, used by 3 stores |
| kiseki-transport | MOSTLY | 8 | MEDIUM | mTLS, X.509, timeouts. No CRL test |
| kiseki-keymanager | DONE | 22 | HIGH | Epochs, rotation, Raft, gRPC service |
| kiseki-log | MOSTLY | 31 | MEDIUM | In-memory + Raft + gRPC data path. 7 Raft + 4 gRPC tests |
| kiseki-audit | MOSTLY | 16 | MEDIUM | Append-only + Raft. 7 Raft integration tests |
| kiseki-chunk | HALF | 6 | LOW | Dedup/GC/holds. No crypto integration, no EC |
| kiseki-composition | HALF | 7 | LOW | CRUD/EXDEV. Not integrated with log |
| kiseki-view | HALF | 7 | LOW | Lifecycle/pins. No stream processor |
| kiseki-advisory | HALF | 7 | LOW | Domain logic. gRPC service wired |
| kiseki-gateway | MOSTLY | 6 | MEDIUM | Full encrypt/decrypt data path, S3+NFS, tenant isolation |
| kiseki-client | STUB | 4 | NONE | Cache only. PyO3 stub. |
| kiseki-server | RUNNING | 0 | LOW | Boots, 2 gRPC services (KeyManager + Log) |

## Go Control Plane

| Package | Status | Tests | Confidence |
|---------|--------|-------|------------|
| tenant | DONE | 4 | HIGH |
| iam | DONE | 4 | HIGH |
| policy | DONE | 1 | MEDIUM |
| advisory | DONE | 2 | MEDIUM |
| grpc (ControlService) | DONE | 4 | MEDIUM |
| grpc (AuditExportService) | DONE | 1 | LOW |

## BDD Coverage

| Harness | Total | Passing | Skipped | Failed |
|---------|-------|---------|---------|--------|
| Rust cucumber-rs | 288 | 249 | 39 | 0 |
| Go godog | 32 | ~10 | ~22 | 0 |
| **Total** | **320** | **~259 (81%)** | **~61** | **0** |

## Test Counts

| Language | Unit/Integration | gRPC | Raft | BDD |
|----------|-----------------|------|------|-----|
| Rust | 125 | 4 | 21 | 249 |
| Go | 15 | 4 | — | ~10 |
| **Total** | **140** | **8** | **21** | **~259** |

## What's been completed (R0-R9)

1. CI green, lefthook pre-commit hooks
2. All adversarial findings tested or tracked
3. Cross-context BDD integration (249/288 scenarios)
4. Raft for log + audit + keymanager (21 integration tests)
5. LogService gRPC data path (write/read over network)
6. Gateway encrypt/decrypt data path (S3 + NFS)
7. Go control plane gRPC (ControlService + AuditExportService)
8. Docker compose for local dev
9. Proto codegen for both Rust and Go

## Remaining gaps

1. EC encoding (enum only, no real erasure coding)
2. Stream processor (view has no delta consumption)
3. Protocol wire-level implementations (NFS RPC, S3 HTTP)
4. FUSE mount (feature flag only)
5. mTLS interceptor on all gRPC services
6. Per-tenant dedup policy lookup from control plane
7. ReadDeltas pagination (unbounded response)
8. Client discovery protocol
