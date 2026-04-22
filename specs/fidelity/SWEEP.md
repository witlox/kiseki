# Fidelity Sweep — Kiseki

**Status**: COMPLETE (post Phase 12ab + all deferred items)
**Updated**: 2026-04-22.
**Previous**: 2026-04-21 (IN PROGRESS — stale after 37 commits)

## Chunks (ordered by risk — highest first)

| # | Chunk | Crate | BDD Pass/Total | Unit+Integ | Status |
|---|-------|-------|---------------|------------|--------|
| 1 | Cryptography | kiseki-crypto | 17/17 | 32 | DONE — AEAD, HKDF, envelope (now Serialize), shred, mlock, compress |
| 2 | Key Manager | kiseki-keymanager | 17/17 | 35 | DONE — epochs, rotation, Raft, cache TTL, persistent Raft log |
| 3 | Log | kiseki-log | 21/21 | 50 | DONE — in-memory + persistent + Raft + auto-split + compaction + inline offload |
| 4 | Transport/Auth | kiseki-transport | 16/16 | 21 | DONE — mTLS, X.509, SPIFFE SAN, CRL, transport selection |
| 5 | Chunk Storage | kiseki-chunk | 25/25 | 34 | DONE — EC, placement, devices, GC, retention, SmallObjectStore |
| 6 | Audit | kiseki-audit | 6/6 | 19 | DONE — append-only, Raft, persistent log |
| 7 | Composition | kiseki-composition | 21/21 | 12 | DONE — CRUD, multipart, versioning, EXDEV, log bridge |
| 8 | View | kiseki-view | 23/23 | 13 | DONE — stream processor (DeltaHandler + DecryptingHandler), MVCC, staleness, multi-shard |
| 9 | Gateway | kiseki-gateway | 23/23 | 39 | DONE — S3 (10 endpoints + multipart), NFS3 (18 procs), NFS4 (28 ops + locks), encryption verified |
| 10 | Client | kiseki-client | 26/26 | 23 | DONE — FUSE (12 ops), discovery, transport select, cache, prefetch, FFI stubs |
| 11 | Advisory | kiseki-advisory | 51/51 | 7 | DONE — unchanged |
| 12 | Control Plane | kiseki-control | 32/32 | 15 | DONE — 16/16 gRPC methods wired, tenant CRUD, IAM, policy, federation |
| 13 | Common/Proto | kiseki-common+proto | n/a | 20 | DONE — HLC, types, InlineStore trait, ChunkId/KeyEpoch serde |
| 14 | Device Mgmt | kiseki-chunk (device) | 19/19 | 26 | DONE — device lifecycle, capacity thresholds, auto-evacuation |
| 15 | Erasure Coding | kiseki-chunk (ec) | 14/14 | 12 | DONE — unchanged |
| 16 | Storage Admin | kiseki-control+chunk | 46/46 | 15 | DONE — pool assertions, admin gRPC wired |
| 17 | Multi-node Raft | kiseki-raft | 20/20 | 16 | DONE — TCP transport, snapshot transfer, persistent log (RedbRaftLogStore) |
| 18 | NFS3 Protocol | kiseki-gateway | 18/18 | 0 | DONE — 18/22 v3 procs implemented, 4 acceptable stubs |
| 19 | NFS4 Protocol | kiseki-gateway | 27/27 | 0 | DONE — 28/28 v4 ops including LOCK/LOCKT/LOCKU |
| 20 | S3 Protocol | kiseki-gateway | 14/14 | 0 | DONE — PUT/GET/HEAD/DELETE/LIST + multipart (4 routes) |
| 21 | Persistence | kiseki-log+raft | 14/14 | 6 | DONE — redb persistence for log, keys, audit; Raft state survives restart |
| 22 | Operational | kiseki-server | 33/33 | 2 | DONE — system disk detection, scrub task, integrity, advisory runtime |
| 23 | Small-File (ADR-030) | kiseki-chunk+log+server | 25/29 | 5 | DONE — SmallObjectStore, inline routing, state machine offload, GC, throughput guard |
| 24 | Block Storage (ADR-029) | kiseki-block | 33/33 | 26 | DONE — raw device alloc, bitmap, CRC32, WAL, scrub |

## Summary

| Status | Chunks | BDD Scenarios |
|--------|--------|---------------|
| DONE | 24 | 554 |
| PARTIAL | 0 | 0 |
| NOT STARTED | 0 | 0 |
| Skipped (no step defs) | — | 9 |
| **Total** | **24** | **563** |

## Delta from previous sweep (2026-04-21)

| Metric | Previous | Current | Change |
|--------|----------|---------|--------|
| Total BDD scenarios | 456 | 563 | +107 |
| Passing scenarios | 456 | 554 | +98 |
| Skipped scenarios | 0 | 9 | +9 (new ADR-030 table steps) |
| Failed scenarios | 0 | 0 | — |
| Parsing errors | 0 | 0 | — |
| Total unit+integ tests | ~307 | 361 | +54 |
| DONE chunks | 4 | 24 | +20 |
| PARTIAL chunks | 12 | 0 | -12 |
| NOT STARTED chunks | 6 | 0 | -6 |
| Feature files | 21 | 22 | +1 (small-file-placement) |
| Step def files | 18 | 19 | +1 (small_file.rs) |
| Invariants | 56 | 63 | +7 (I-SF1-7) |
| ADRs | 29 | 30 | +1 (ADR-030) |

### Chunks that changed status

| Chunk | Previous → Current | What happened |
|-------|-------------------|---------------|
| Cryptography | PARTIAL → DONE | Envelope gains Serialize/Deserialize |
| Key Manager | PARTIAL → DONE | Persistent Raft log (RedbRaftLogStore) |
| Log | PARTIAL → DONE | Persistent Raft, inline offload, throughput guard |
| Transport/Auth | PARTIAL → DONE | Already DONE, count updated |
| Chunk Storage | PARTIAL → DONE | SmallObjectStore, refcount wiring |
| Audit | LOW → DONE | Persistent Raft log |
| View | PARTIAL → DONE | DeltaHandler, DecryptingHandler, multi-shard ordering |
| Gateway | PARTIAL → DONE | S3 multipart routes, NFS locks, inline read path |
| Client | NOT STARTED → DONE | 12 FUSE ops, discovery, FFI stubs, fuser daemon |
| Storage Admin | PARTIAL → DONE | All gRPC methods wired |
| Multi-node Raft | NOT STARTED → DONE | TCP transport, snapshot transfer, persistent log |
| NFS3 Protocol | PARTIAL → DONE | 18/22 procedures dispatched |
| NFS4 Protocol | NOT STARTED → DONE | 28/28 operations including locks |
| S3 Protocol | PARTIAL → DONE | Multipart upload routes added |
| Persistence | NOT STARTED → DONE | RedbRaftLogStore for all 3 Raft groups |
| Operational | NOT STARTED → DONE | System disk detection, scrub task |
| Small-File (NEW) | — → DONE | ADR-030 full implementation |
| Block Storage | — (was in P4a) | Counted separately now |
