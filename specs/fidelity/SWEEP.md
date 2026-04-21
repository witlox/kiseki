# Fidelity Sweep — Kiseki

**Status**: IN PROGRESS (post Phase G + ADR-027 + honesty sweep)
**Updated**: 2026-04-21.

## Chunks (ordered by risk — highest first)

| # | Chunk | Crate | BDD Pass/Total | Unit Tests | Status |
|---|-------|-------|---------------|------------|--------|
| 1 | Cryptography | kiseki-crypto | 9/17 | 19 | PARTIAL — shred + cache done, rotation/re-encryption pending |
| 2 | Key Manager | kiseki-keymanager | 9/17 | 34 | PARTIAL — epochs + Raft, need rotation worker |
| 3 | Log | kiseki-log | 12/21 | 31 | PARTIAL — in-memory + persistent, need auto-split |
| 4 | Transport/Auth | kiseki-transport | 7/16 | 8 | PARTIAL — mTLS real, need IdP/SPIFFE |
| 5 | Chunk Storage | kiseki-chunk | 9/25 | 23 | PARTIAL — EC + placement + devices, need full pipeline wiring |
| 6 | Audit | kiseki-audit | (audit steps are TODOs) | 16 | LOW — audit infrastructure not wired into BDD |
| 7 | Composition | kiseki-composition | 20/21 | 12 | HIGH — nearly complete |
| 8 | View | kiseki-view | 9/23 | 7 | PARTIAL — stream processor real, need versioning |
| 9 | Gateway | kiseki-gateway | 8/21 | 9 | PARTIAL — pipeline real, need NFS lock/auth/advisory |
| 10 | Client | kiseki-client | 0/26 | 11 | NOT STARTED — discovery + transport selection needed |
| 11 | Advisory | kiseki-advisory | 51/51 | 7 | DONE |
| 12 | Control Plane | kiseki-control | 32/32 | 5 | DONE (ADR-027 Rust-only) |
| 13 | Common/Proto | kiseki-common+proto | (foundation) | 20 | HIGH |
| 14 | Device Mgmt | kiseki-chunk (device) | 19/19 | 5 | DONE |
| 15 | Erasure Coding | kiseki-chunk (ec) | 14/14 | 12 | DONE |
| 16 | Storage Admin | kiseki-control+chunk | 16/46 | 0 | PARTIAL — pool assertions, need admin gRPC |
| 17 | Multi-node Raft | kiseki-raft | 0/18 | 7 | NOT STARTED — need distributed harness |
| 18 | NFS3 Protocol | kiseki-gateway | 6/18 | 0 | PARTIAL — dispatch real, handlers stub |
| 19 | NFS4 Protocol | kiseki-gateway | 0/27 | 0 | NOT STARTED — handler stubs |
| 20 | S3 Protocol | kiseki-gateway | 4/14 | 0 | PARTIAL — PUT/GET real, HEAD/DELETE/LIST partial |
| 21 | Persistence | kiseki-log+raft | 0/12 | 6 | NOT STARTED — background step missing |
| 22 | Operational | kiseki-server | 0/33 | 0 | NOT STARTED — integrity/versioning/compression |

## Summary

| Status | Chunks | BDD Scenarios |
|--------|--------|---------------|
| DONE | 4 | 116 |
| PARTIAL (>50%) | 5 | ~57 |
| PARTIAL (<50%) | 7 | ~52 |
| NOT STARTED | 6 | ~130 |
| **Total** | **22** | **456** |
