# Fidelity Index — Kiseki (Post Phase D)

**Checkpoint**: 2026-04-20 (post Phase D — Go BDD + security + protocols)
**Previous**: 2026-04-20 (post-R9 remediation)

## Per-Crate Status

| Crate | Status | Tests | Confidence | Validated |
|-------|--------|-------|------------|-----------|
| kiseki-common | DONE | 14 | HIGH | HLC, boundaries, property tests |
| kiseki-crypto | DONE | 33 | HIGH | AEAD, HKDF, envelope, mlock |
| kiseki-proto | DONE | 6 | HIGH | Protobuf roundtrip |
| kiseki-raft | DONE | 7 | HIGH | Generic MemLogStore |
| kiseki-transport | MOSTLY | 8 | MEDIUM | mTLS, X.509, timeouts |
| kiseki-keymanager | DONE | 22 | HIGH | Epochs, rotation, Raft, gRPC |
| kiseki-log | DONE | 31 | HIGH | In-memory + Raft + gRPC + pagination cap |
| kiseki-audit | MOSTLY | 16 | MEDIUM | Append-only + Raft |
| kiseki-chunk | HALF | 6 | LOW | Dedup/GC/holds. No EC |
| kiseki-composition | MOSTLY | 12 | MEDIUM | CRUD + log bridge + pipeline tests. Delta rollback on failure |
| kiseki-view | MOSTLY | 7 | MEDIUM | Lifecycle/pins + stream processor |
| kiseki-advisory | DONE | 7 | MEDIUM | Domain logic + gRPC |
| kiseki-gateway | DONE | 9 | MEDIUM | S3 HTTP + NFSv3 + NFSv4.2 + XDR codec + data path |
| kiseki-client | MOSTLY | 11 | MEDIUM | Cache + FUSE (7 tests). No fuser runtime |
| kiseki-server | RUNNING | 0 | MEDIUM | All protocols wired, stream proc, bootstrap |

## Go Control Plane

| Package | Status | Tests | Confidence |
|---------|--------|-------|------------|
| tenant | DONE | 4 | HIGH |
| iam | DONE | 4 | HIGH |
| policy | DONE | 1 | MEDIUM |
| advisory | DONE | 2 | MEDIUM |
| namespace | DONE | 0 | LOW (BDD only) |
| flavor | DONE | 0 | LOW (BDD only) |
| retention | DONE | 0 | LOW (BDD only) |
| federation | DONE | 0 | LOW (BDD only) |
| maintenance | DONE | 0 | LOW (BDD only) |
| grpc (Control) | DONE | 4 | MEDIUM |
| grpc (Audit) | DONE | 1 | LOW |

## BDD Coverage

| Harness | Total | Passing | Skipped | Failed |
|---------|-------|---------|---------|--------|
| Rust cucumber-rs | 288 | 249 | 39 | 0 |
| Go godog | 32 | 32 | 0 | 0 |
| **Total** | **320** | **281 (88%)** | **39** | **0** |

## E2E Coverage

| Suite | Tests | Against |
|-------|-------|---------|
| Python gRPC | 4 | Docker (LogService + KeyManager) |
| Python S3 | 4 | Docker (S3 HTTP gateway) |
| Python cross-protocol | 3 | Docker (S3 → gRPC verification) |
| **Total** | **11** | Real process boundaries |

## Adversarial Findings

| Review | Fixed | Open | Total |
|--------|-------|------|-------|
| Phase D | 0 | 11 | 11 |
| Pipeline | 3 | 2 | 5 |
| Phase C | 4 | 10 | 14 |
| Phase B+A | 7 | 7 | 14 |
| OPEN-FINDINGS.md (pre-existing) | 6 | ~30 | ~36 |
| **Totals** | **20** | **~60** | **~80** |

Open CRITICALs (4): auth interceptor, S3 TLS, ViewStore sharing, NFS LOOKUP.
All deferred to specific future work (mTLS impl, NFS directory index).

## Remaining Gaps

1. **Persistence** — all in-memory, server restart = data loss
2. **Multi-node Raft** — single-node only, no failover
3. **EC erasure coding** — chunks stored whole, no striping
4. **mTLS on S3/NFS** — plumbing defined, TLS acceptor not wired
5. **NFS directory index** — LOOKUP/CREATE need name→composition mapping
6. **ViewStore in read path** — views exist but disconnected from gateways
7. **Go BDD assertion depth** — structural, not behavioral
