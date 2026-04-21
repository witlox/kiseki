# Fidelity Index — Kiseki (Post Phase G + ADR-027 + Honesty Sweep)

**Checkpoint**: 2026-04-21
**Previous**: 2026-04-20 (post Phase F)

## Per-Crate Status

| Crate | Status | Unit Tests | Confidence | Notes |
|-------|--------|------------|------------|-------|
| kiseki-common | DONE | 14 | HIGH | HLC, types, property tests |
| kiseki-crypto | DONE | 19 | HIGH | AEAD, HKDF, envelope, shred, mlock |
| kiseki-proto | DONE | 6 | HIGH | Protobuf roundtrip |
| kiseki-raft | DONE | 7 | HIGH | MemLogStore, TCP transport |
| kiseki-transport | DONE | 8 | MEDIUM | mTLS, X.509, timeouts |
| kiseki-keymanager | DONE | 34 | HIGH | Epochs, rotation, Raft, cache TTL |
| kiseki-log | DONE | 31 | HIGH | In-memory + Raft + gRPC + persistent |
| kiseki-audit | DONE | 16 | MEDIUM | Append-only + Raft |
| kiseki-chunk | DONE | 23 | HIGH | EC encode/decode, placement, devices, GC |
| kiseki-composition | DONE | 12 | MEDIUM | CRUD + log bridge + pipeline |
| kiseki-view | DONE | 7 | MEDIUM | Lifecycle, pins, stream processor |
| kiseki-advisory | DONE | 7 | MEDIUM | Domain logic + gRPC |
| kiseki-gateway | DONE | 9 | MEDIUM | S3 HTTP + NFS3 + NFS4 + XDR + InMemoryGateway pipeline |
| kiseki-client | PARTIAL | 11 | LOW | Cache + FUSE. No discovery |
| kiseki-control | DONE | 5 | HIGH | Tenant, IAM, policy, flavor, federation, namespace, retention, advisory |
| kiseki-server | WIRED | 0 | MEDIUM | All protocols + ControlService gRPC registered |

**Total Rust unit tests**: 221 pass, 0 fail

## BDD Coverage (Honest)

| Metric | Value |
|--------|-------|
| Total scenarios | 456 |
| Passing (real assertions) | 205 (44%) |
| Failing (panic backlog) | 200 |
| Skipped (missing steps) | 51 |

### Features at 100%

| Feature | Scenarios |
|---------|-----------|
| Control Plane | 32/32 |
| Device management | 19/19 |
| Erasure coding | 14/14 |
| Workflow Advisory | 51/51 |

### Features with significant coverage

| Feature | Pass/Total |
|---------|-----------|
| Composition | 20/21 |
| Log | 12/21 |
| View | 9/23 |
| Protocol Gateway | 8/21 |
| Key Management | 9/17 |
| Authentication | 7/16 |
| Chunk Storage | 9/25 |
| NFSv3 | 6/18 |
| S3 API | 4/14 |
| Storage Admin | 16/46 |

### Features at 0%

| Feature | Total | Blocker |
|---------|-------|---------|
| Native Client | 26 | Discovery not implemented |
| Operational | 33 | Runtime infrastructure |
| Protocol Gateway (advanced) | 13 | NFS lock state, advisory integration |
| Multi-node Raft | 18 | Distributed testing harness |
| Persistence | 12 | Background step definition |
| NFSv4.2 | 27 | Handler stubs |

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
| Accepted | 24 |
| Proposed | 3 (ADR-025 partial) |
| **Total** | **27** |

## Adversarial Reviews

| Review | Findings | Status |
|--------|----------|--------|
| Phase G gate-2 | 4C 5H 4M 3L | Blocking fixes applied |
| ADR-027 gate-1 | 5C 3H 4M 2L | Accepted with fixes |
| Phase D | 11 | Deferred |
| Pipeline | 5 | 3 fixed |
| Phase C | 14 | 4 fixed |
| Phase B+A | 14 | 7 fixed |
| Pre-existing (OPEN-FINDINGS.md) | ~67 | Partially stale, needs re-triage |

## Key Architecture Decisions This Session

1. **ADR-027 Accepted**: Go control plane replaced with Rust (kiseki-control)
2. **Lefthook removed**: Pre-commit via `make check` + CI
3. **BDD honesty sweep**: 1125 empty stubs → panic!("not yet implemented")
4. **InMemoryGateway pipeline**: Real encrypt→EC→store→decrypt in BDD
5. **Crate-graph firewall**: kiseki-control depends only on kiseki-common + kiseki-proto

## Remaining Gaps (100% Completion Plan)

See `specs/implementation/100pct-completion-plan.md` for R1-R8 phases.

Critical path: R1 (integrated pipeline, mostly done) → R2 (NFS+S3 handlers)
→ then R3 (view), R4 (log+raft), R5 (admin), R6 (auth), R7 (client), R8 (operational).

~25 new modules needed, ~24 sessions estimated for full completion.
