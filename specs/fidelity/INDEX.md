# Fidelity Index — Kiseki (Post F1-F10 + ADR-028)

**Checkpoint**: 2026-04-22
**Previous**: 2026-04-21 (post Phase G + ADR-027 + honesty sweep)

## Per-Crate Status

| Crate | Status | Unit Tests | Confidence | Notes |
|-------|--------|------------|------------|-------|
| kiseki-common | DONE | 14 | HIGH | HLC, types, property tests, versioning |
| kiseki-crypto | DONE | 22 | HIGH | AEAD, HKDF, envelope, shred, mlock, compress |
| kiseki-proto | DONE | 6 | HIGH | Protobuf roundtrip (9 proto files) |
| kiseki-raft | DONE | 7 | HIGH | MemLogStore, TCP transport |
| kiseki-transport | DONE | 13 | MEDIUM | mTLS, X.509, SPIFFE, CRL revocation |
| kiseki-keymanager | DONE | 34 | HIGH | Epochs, rotation, Raft, cache TTL, rewrap worker |
| kiseki-log | DONE | 31 | HIGH | In-memory + Raft + gRPC + persistent + auto-split + compaction |
| kiseki-audit | DONE | 16 | MEDIUM | Append-only + Raft |
| kiseki-chunk | DONE | 23 | HIGH | EC encode/decode, placement, devices, GC, retention holds |
| kiseki-composition | DONE | 12 | MEDIUM | CRUD + log bridge + pipeline + multipart |
| kiseki-view | DONE | 13 | MEDIUM | Lifecycle, pins, stream processor, versioning |
| kiseki-advisory | DONE | 7 | MEDIUM | Domain logic + gRPC |
| kiseki-gateway | DONE | 11 | MEDIUM | S3 HTTP + NFS3 (22 procs) + NFS4 + XDR + InMemoryGateway pipeline |
| kiseki-client | DONE | 23 | MEDIUM | Cache, FUSE, transport select, batching, prefetch |
| kiseki-control | DONE | 15 | HIGH | Tenant, IAM, policy, flavor, federation, namespace, retention, advisory, StorageAdminService |
| kiseki-server | WIRED | 0 | MEDIUM | All protocols + ControlService gRPC registered |

**Total Rust unit tests**: 307+ pass, 0 fail

## BDD Coverage

| Metric | Value |
|--------|-------|
| Total scenarios | 456 |
| Passing (real assertions) | 456 (100%) |
| Failing | 0 |
| Skipped | 0 |

### All 19 features at 100%

| Feature | Scenarios |
|---------|-----------|
| Authentication | 16/16 |
| Chunk Storage | 25/25 |
| Composition | 21/21 |
| Control Plane | 32/32 |
| Device Management | 19/19 |
| Erasure Coding | 14/14 |
| Key Management | 17/17 |
| Log | 21/21 |
| Multi-node Raft | 18/18 |
| Native Client | 26/26 |
| NFSv3 RFC 1813 | 18/18 |
| NFSv4.2 RFC 7862 | 27/27 |
| Operational | 33/33 |
| Persistence | 12/12 |
| Protocol Gateway | 21/21 |
| S3 API | 14/14 |
| Storage Admin | 46/46 |
| View Materialization | 23/23 |
| Workflow Advisory | 51/51 |

### Pending (ADR-028, not yet wired)

| Feature | Scenarios | Status |
|---------|-----------|--------|
| External KMS Providers | 45 | Feature file written, step definitions pending |

## E2E Coverage

| Suite | Tests |
|-------|-------|
| Server health + log roundtrip | 4 |
| S3 gateway (PUT/GET/HEAD/DELETE) | 4 |
| Cross-protocol | 3 |
| Persistence (Docker restart) | 1 |
| Multi-node cluster (3-node) | 4 |
| Control plane (gRPC) | 3 |
| **Total** | **19** |

## ADR Status

| Status | Count |
|--------|-------|
| Accepted | 28 |
| Proposed | 0 |
| **Total** | **28** |

## Adversarial Reviews

| Review | Findings | Status |
|--------|----------|--------|
| ADR-028 gate | 2H 5M 1L | All resolved in ADR |
| F1-F10 gate-2 | 2C 7H 12M | All fixed (commit ae523f3) |
| Phase G gate-2 | 4C 5H 4M 3L | Blocking fixes applied |
| ADR-027 gate-1 | 5C 3H 4M 2L | Accepted with fixes |
| Phase D | 11 | Deferred |
| Pipeline | 5 | 3 fixed |
| Phase C | 14 | 4 fixed |
| Phase B+A | 14 | 7 fixed |

## Key Milestones

1. **F1-F10 Complete**: All missing domain features built (NFS3/NFS4/S3, key rotation, log split, view versioning, client, admin API, auth, operational)
2. **456/456 BDD**: All scenarios pass with real domain assertions, 0 panic stubs
3. **ADR-027**: Go control plane fully replaced with Rust (kiseki-control)
4. **ADR-028 Accepted**: External Tenant KMS Providers (5 backends, 45 new scenarios)
5. **Crate-graph firewall**: kiseki-control depends only on kiseki-common + kiseki-proto

## Next: Production Readiness Plan

See `specs/implementation/production-readiness-plan.md`:
Q1 (quality gate) → Q2 (step audit) → P1-P4 (persistence) → I1-I2 (e2e + multi-node)
