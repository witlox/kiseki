# Fidelity Index — Kiseki (Revised)

**Checkpoint**: 2026-04-19 (post-BDD wiring)
**Previous**: 2026-04-19 (pre-BDD, overstated 23% scenario coverage)

## Honest Assessment

The previous fidelity index overstated coverage. This revision
reflects what actually works (tested + integrated) vs what merely
compiles.

## Per-Crate Status

| Crate | Status | Tests | Confidence | Validated |
|-------|--------|-------|------------|-----------|
| kiseki-common | DONE | 13 | HIGH | HLC ordering, boundary tests, property tests |
| kiseki-crypto | DONE | 33 | HIGH | AEAD, HKDF, envelope, chunk ID, mlock |
| kiseki-proto | DONE | 6 | HIGH | Protobuf roundtrip |
| kiseki-raft | DONE | 7 | HIGH | Generic MemLogStore |
| kiseki-transport | MOSTLY | 8 | MEDIUM | mTLS, X.509, timeouts. No CRL test |
| kiseki-keymanager | MOSTLY | 22 | MEDIUM | Epochs, rotation, Raft path. No gRPC test |
| kiseki-log | HALF | 20 | LOW | In-memory semantics. Raft scaffolded only |
| kiseki-audit | HALF | 9 | LOW | Append-only in-memory. Raft scaffolded only |
| kiseki-chunk | HALF | 6 | LOW | Dedup/GC/holds. No crypto, no EC |
| kiseki-composition | HALF | 7 | LOW | CRUD/EXDEV. Not integrated with log/chunks |
| kiseki-view | HALF | 7 | LOW | Lifecycle/pins. No stream processor |
| kiseki-advisory | HALF | 7 | LOW | Domain logic. Runtime not built |
| kiseki-gateway | STUB | 0 | NONE | Trait only |
| kiseki-client | STUB | 4 | NONE | Cache only |
| kiseki-server | SCAFFOLD | 0 | NONE | Boots + 2 RPCs |

## BDD Coverage

| Harness | Total | Passing | Skipped | Failed |
|---------|-------|---------|---------|--------|
| Rust cucumber-rs | 288 | 2 | 286 | 0 |
| Go godog | 32 | 1 | 31 | 0 |
| **Total** | **320** | **3 (0.9%)** | **317** | **0** |

## Invariant Enforcement: 15 of 56 (27%)

## What's missing for functional system

1. Cross-context integration (composition -> log -> chunk -> view)
2. Raft for log + audit (scaffolded, not wired like keymanager)
3. Data-path gRPC (no write/read over network)
4. Protocol implementations (NFS, S3, FUSE empty)
5. EC encoding (enum only)
6. Stream processor (view has no delta consumption)
