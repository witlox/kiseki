# 100% Feature Completion Plan: 185/456 → 456/456

## Context

185 scenarios pass with real assertions. 271 remain (220 fail, 51 skip).
Every failing Then-step has `panic!("not yet implemented")` — honest backlog.
Goal: 100% of 456 scenarios passing with real domain code.

## Gap Analysis

| Category | Scenarios | Description |
|----------|-----------|-------------|
| BDD_ONLY | ~85 | Wire assertions to EXISTING domain code |
| DOMAIN_CODE | ~130 | New Rust modules needed first |
| INTEGRATION | ~25 | Multi-process/distributed testing |
| SKIPPED | ~31 | Missing step definitions |

## Phases (dependency-ordered, each self-contained)

### H1: Core Crypto Pipeline (18 scenarios)

**New modules:**
- `kiseki-crypto/src/shred.rs` — destroy tenant KEK, invalidate wrappings
- `kiseki-keymanager/src/rotation.rs` — system/tenant KEK rotation + re-wrapping
- `kiseki-keymanager/src/cache.rs` — key cache with TTL

**BDD:** `steps/crypto.rs` — seal/open/shred/rotate assertions

### H2: Log Shard Lifecycle (10 scenarios)

**New modules:**
- `kiseki-log/src/auto_split.rs` — automatic splitting at hard ceiling

**BDD:** `steps/log.rs` — split, compaction, GC, phase marker assertions

### H3: View Completion (15 scenarios)

**New modules:**
- `kiseki-view/src/versioning.rs` — object version tracking
- Extend `view.rs` — staleness enforcement with SLO

**BDD:** `steps/view.rs` — versioning, staleness, recovery assertions

### H4: NFS Protocol Handlers (30 scenarios)

**Extend:**
- `nfs3_server.rs` — real handlers for 11 procedures
- `nfs4_server.rs` — real handlers for 18 COMPOUND operations

**BDD:** `steps/protocol.rs` — assert NFS response fields

### H5: S3 Handler Completion (10 scenarios)

**Extend:**
- `s3_server.rs` — HEAD, DELETE, LIST with real data

**BDD:** `steps/protocol.rs` — assert HTTP responses

### H6: Protocol Gateway Integration (23 scenarios)

**Extend:**
- `nfs_ops.rs` — wire NFS through InMemoryGateway
- Gateway auth (Kerberos/AUTH_SYS, SigV4)

**BDD:** `steps/gateway.rs` — full encrypt/decrypt pipeline

### H7: Storage Admin API (37 scenarios)

**New modules:**
- `kiseki-control/src/pool_admin.rs` — pool CRUD, thresholds
- `kiseki-control/src/shard_admin.rs` — shard listing, split, scrub
- `kiseki-control/src/device_admin.rs` — device lifecycle, drain
- `kiseki-control/src/billing.rs` — per-tenant usage
- `kiseki-control/src/grpc/admin_service.rs` — StorageAdminService gRPC

**BDD:** `steps/admin.rs` — real admin operations

### H8: Authentication + Identity (10 scenarios)

**New modules:**
- `kiseki-transport/src/spiffe.rs` — SPIFFE SVID URI parsing
- `kiseki-control/src/idp.rs` — IdP token validation (OIDC)

**BDD:** `steps/auth.rs` — identity extraction assertions

### H9: Native Client (26 scenarios)

**New modules:**
- `kiseki-client/src/discovery.rs` — seed-based discovery
- `kiseki-client/src/transport_select.rs` — CXI/verbs/TCP fallback
- `kiseki-client/src/batching.rs` — write coalescing
- `kiseki-client/src/prefetch.rs` — readahead detection

**BDD:** `steps/client.rs` — FUSE/cache/discovery assertions

### H10: Operational Infrastructure (33 scenarios)

**New modules:**
- `kiseki-server/src/integrity.rs` — ptrace detection, core dump blocking
- `kiseki-common/src/versioning.rs` — delta format version negotiation
- `kiseki-crypto/src/compression.rs` — compress-then-encrypt pipeline

**BDD:** `steps/operational.rs` — integrity/versioning/compression

### H11: Chunk Storage Full Pipeline (21 scenarios)

**Extend:**
- `kiseki-chunk/src/store.rs` — encrypt → EC → place → store → read pipeline

**BDD:** `steps/chunk.rs` — full pipeline assertions

### H12: Multi-node Raft + Persistence (30 scenarios)

**New infrastructure:**
- `kiseki-raft/src/test_harness.rs` — multi-node in-process harness
- Persistence step defs (redb round-trip in BDD)

**BDD:** `steps/raft.rs` — real Raft operations

---

## Execution order

```
H1 (crypto)  ──→ H11 (chunk pipeline) ──→ H6 (gateway)
     │                                          │
     └──→ H2 (log) ──→ H12 (raft+persist)      │
                                                 │
H3 (view) ──────────────────────────────────→ H6
H4 (NFS) + H5 (S3) ────────────────────────→ H6
H7 (admin) ─────────────────────────────────→ done
H8 (auth) ──→ H9 (client) ──→ H10 (operational)
```

## Estimated effort

| Phase | New modules | Scenarios | Sessions |
|-------|-------------|-----------|----------|
| H1 Crypto | 3 | 18 | 2 |
| H2 Log | 1 | 10 | 1 |
| H3 View | 1 | 15 | 1 |
| H4 NFS | extend 2 | 30 | 3 |
| H5 S3 | extend 1 | 10 | 1 |
| H6 Gateway | extend 2 | 23 | 2 |
| H7 Admin | 5 | 37 | 3 |
| H8 Auth | 2 | 10 | 1 |
| H9 Client | 4 | 26 | 3 |
| H10 Operational | 3 | 33 | 3 |
| H11 Chunk | extend 1 | 21 | 2 |
| H12 Raft | 1 harness | 30 | 2 |
| **Total** | **~25** | **271** | **~24** |

## Verification

After each phase:
1. `cargo test -p kiseki-acceptance` — count increases monotonically
2. `cargo test --workspace` — 0 unit test regressions
3. No empty bodies, no panic→comment conversions
4. Every Then-step calls real domain code
