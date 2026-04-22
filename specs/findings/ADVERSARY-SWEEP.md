# Adversary Sweep — Kiseki

**Status**: COMPLETE
**Started**: 2026-04-22
**Fidelity baseline**: 24/24 chunks DONE, 554/563 BDD, no LOW areas

## Attack Surface Summary

| Interface | Port | Auth | Risk |
|-----------|------|------|------|
| gRPC data-path | 9100 | mTLS (optional) | MEDIUM |
| gRPC advisory | 9101 | mTLS (optional) | MEDIUM |
| S3 gateway | 9000 | None (MVP) | HIGH |
| NFS server | 2049 | AUTH_NONE (MVP) | HIGH |
| Raft transport | 9300 | None (plaintext TCP) | CRITICAL |
| Control plane | 9100 | mTLS, no per-method authz | MEDIUM |

## Chunks (priority: CRITICAL interfaces first)

| # | Chunk | Focus | Status |
|---|-------|-------|--------|
| 1 | Raft transport security | Plaintext TCP, unbounded alloc, no auth | DONE |
| 2 | Encryption boundary | seal/open envelope, key derivation, nonce reuse | DONE |
| 3 | Chunk storage integrity | EC, GC accounting, placement, refcount | DONE |
| 4 | Gateway auth + tenant isolation | S3/NFS no-auth, gRPC no per-method authz | DONE |
| 5 | Control plane authorization | TOCTOU, quota bypass, IAM gaps | DONE |
| 6 | Input validation + DoS | NFS XDR, proto deser, frame limits | DONE |
| 7 | Key management | Epoch rotation, cache TTL, shred propagation | DONE |
| 8 | Cross-cutting | Dependencies, unsafe, debug leaks | DONE |
