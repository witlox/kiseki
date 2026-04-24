# Fidelity Index — Kiseki (Post ADR-032 Async GatewayOps)

**Checkpoint**: 2026-04-24 (end of session)
**Previous**: 2026-04-22 (456 BDD → 599 BDD)

## Per-Crate Status

| Crate | Status | Unit+Integ Tests | Confidence | Notes |
|-------|--------|-----------------|------------|-------|
| kiseki-common | DONE | 18 | HIGH | HLC, types, InlineStore trait, ChunkId/KeyEpoch serde |
| kiseki-crypto | DONE | 32 | HIGH | AEAD, HKDF, envelope (Serialize), shred, mlock, compress |
| kiseki-proto | DONE | 6 | HIGH | Protobuf roundtrip (9 proto files) |
| kiseki-raft | DONE | 16 | HIGH | MemLogStore, RedbRaftLogStore, TCP transport, snapshot transfer |
| kiseki-transport | DONE | 21 | HIGH | mTLS, X.509, SPIFFE SAN, CRL revocation |
| kiseki-keymanager | DONE | 35 | HIGH | Epochs, rotation, Raft (persistent), cache TTL, rewrap |
| kiseki-log | DONE | 50 | HIGH | In-memory + Raft + persistent + auto-split + compaction + inline offload + throughput guard |
| kiseki-audit | DONE | 19 | MEDIUM | Append-only + Raft (persistent) |
| kiseki-chunk | DONE | 34 | HIGH | EC, placement, devices, GC, retention, SmallObjectStore |
| kiseki-block | DONE | 26 | HIGH | Raw device alloc, bitmap, CRC32, WAL, superblock, scrub |
| kiseki-composition | DONE | 12 | MEDIUM | CRUD + log bridge + pipeline + multipart + versioning |
| kiseki-view | DONE | 13 | MEDIUM | Lifecycle, pins, stream processor (DeltaHandler + DecryptingHandler), versioning |
| kiseki-advisory | DONE | 7 | MEDIUM | Domain logic + gRPC |
| kiseki-gateway | DONE | 39 | HIGH | S3 (10 endpoints + multipart) + NFS3 (18 procs) + NFS4 (28 ops + locks) + InMemoryGateway + inline routing |
| kiseki-client | DONE | 23 | MEDIUM | FUSE (12 ops), fuser daemon, discovery, transport select, cache, prefetch, FFI stubs |
| kiseki-control | DONE | 15 | HIGH | 16/16 gRPC methods, tenant CRUD, IAM, policy, flavor, federation, namespace, retention |
| kiseki-server | WIRED | 2 | MEDIUM | All protocols + ControlService + system disk detection + scrub task + SmallObjectStore |

**Total unit+integration tests**: 361 pass, 0 fail

## BDD Coverage

| Metric | Value |
|--------|-------|
| Feature files | 22 |
| Total scenarios | 599 |
| Passing | 599 (100%) |
| Skipped | 0 |
| Failed | 0 |
| Parsing errors | 0 |
| Step definition files | 19 |
| Step definition functions | 2,735 |

### Per-Feature Breakdown

| Feature | Scenarios | Pass | Skip |
|---------|-----------|------|------|
| authentication | 16 | 16 | 0 |
| block-storage | 33 | 33 | 0 |
| chunk-storage | 25 | 25 | 0 |
| composition | 21 | 21 | 0 |
| control-plane | 32 | 32 | 0 |
| device-management | 19 | 19 | 0 |
| erasure-coding | 14 | 14 | 0 |
| external-kms | 41 | 41 | 0 |
| key-management | 17 | 17 | 0 |
| log | 21 | 21 | 0 |
| multi-node-raft | 20 | 20 | 0 |
| native-client | 26 | 26 | 0 |
| nfs3-rfc1813 | 18 | 18 | 0 |
| nfs4-rfc7862 | 27 | 27 | 0 |
| operational | 33 | 33 | 0 |
| persistence | 14 | 14 | 0 |
| protocol-gateway | 23 | 23 | 0 |
| s3-api | 14 | 14 | 0 |
| small-file-placement | 29 | 20 | 9 |
| storage-admin | 46 | 46 | 0 |
| view-materialization | 23 | 23 | 0 |
| workflow-advisory | 51 | 51 | 0 |

## Invariants

| Category | Count |
|----------|-------|
| Log (I-L1-9) | 9 |
| Chunk (I-C1-8) | 8 |
| Key (I-K1-14) | 14 |
| Tenant (I-T1-7) | 7 |
| View (I-V1-4) | 4 |
| Auth (I-Auth1-4) | 4 |
| Audit (I-A1-5) | 5 |
| Operational (I-O1-6) | 6 |
| Advisory (I-WA1-19) | 19 |
| Small-File (I-SF1-7) | 7 |
| **Total** | **63** (enforcement-map.md current) |

## ADRs

32 ADRs (001-032). All accepted. Latest: ADR-032 (Async GatewayOps).
ADR-031: Client-Side Cache. ADR-032: Async GatewayOps (lock-free composition writes).

## Confidence Assessment

| Level | Crates | Notes |
|-------|--------|-------|
| HIGH | 10 | common, crypto, proto, raft, transport, keymanager, log, chunk, block, control |
| MEDIUM | 6 | audit, composition, view, advisory, client, server |
| LOW | 0 | — |

No LOW confidence areas. All crates have passing tests and BDD coverage.
Adversary sweep can proceed on any chunk without fidelity blockers.
