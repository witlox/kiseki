# 100% Feature Completion Plan: 185/456 → 456/456

**STATUS: COMPLETE** (2026-04-22)

456/456 scenarios passing with real domain assertions. 0 failures, 0 skipped.
Achieved via F1-F10 feature implementation + adversarial gate-2 + BDD wiring.
See `specs/implementation/production-readiness-plan.md` for the next phase.

## Context (historical)

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

---

## Addendum: Duplication Analysis + Revised Execution Order

**Date**: 2026-04-21 (added after partial H1/H7/H11 execution)

### Problem identified

The original H1-H12 phases have significant overlap:
- H1 (crypto) and H11 (chunk pipeline) both touch encryption
- H4 (NFS handlers), H5 (S3 handlers), and H6 (gateway integration)
  are really one phase — wire protocols through the pipeline
- H7 (admin) overlaps with existing device/pool code from Phase G
- BDD steps written so far create inline crypto roundtrips instead of
  exercising the actual system pipeline end-to-end

### Root cause

The pieces exist individually (crypto, EC, placement, stores, gateway)
but there's no single integrated write/read path:
```
write: client → gateway → composition → encrypt → EC encode → place → store
read:  store → gather fragments → EC decode → decrypt → gateway → client
```

### Revised execution order (merging overlapping phases)

**R1: Integrated Write/Read Pipeline** (merges H11 + H6 + H1 partial)
- Build `kiseki-gateway/src/pipeline.rs` — single function that chains:
  write: plaintext → seal_envelope → EC encode → place fragments → store
  read: gather fragments → EC decode → open_envelope → plaintext
- Wire InMemoryGateway.write() and .read() through this pipeline
- All crypto, EC, placement happens inside the pipeline
- BDD steps call gateway.write/read and assert plaintext roundtrip
- **Unlocks**: chunk-storage, key-management, gateway, composition scenarios
- **Est. scenarios**: ~60

**R2: NFS + S3 Protocol Handlers** (merges H4 + H5)
- NFS3 procedure handlers call pipeline via NfsGateway
- NFS4 COMPOUND ops call pipeline via NfsGateway
- S3 HEAD/DELETE/LIST handlers call pipeline via S3Gateway
- BDD steps assert protocol-specific response fields
- **Depends on**: R1 (pipeline must exist)
- **Est. scenarios**: ~40

**R3: View + Stream Processor** (H3 unchanged)
- Object versioning
- Staleness enforcement
- Stream processor crash recovery
- **Depends on**: R1 (deltas must flow through pipeline)
- **Est. scenarios**: ~15

**R4: Log + Raft** (merges H2 + H12)
- Auto-split at hard ceiling
- Multi-node Raft test harness
- Persistence BDD steps (redb round-trip)
- **Depends on**: R1 (deltas come from pipeline writes)
- **Est. scenarios**: ~25

**R5: Admin API** (H7 reduced — remove overlap with Phase G)
- Pool/shard/device admin gRPC service
- Per-tenant billing
- Observability streaming
- **Independent of R1-R4**
- **Est. scenarios**: ~30

**R6: Auth + Identity** (H8 unchanged)
- SPIFFE SVID, IdP, gateway auth
- **Independent**
- **Est. scenarios**: ~10

**R7: Native Client** (H9 unchanged)
- Discovery, transport selection, FUSE pipeline, batching, prefetch
- **Depends on**: R1 (client calls pipeline)
- **Est. scenarios**: ~26

**R8: Operational** (H10 unchanged)
- Integrity monitoring, format versioning, compression
- **Mostly independent**
- **Est. scenarios**: ~33

### Revised execution diagram

```
R1 (pipeline) ──→ R2 (NFS+S3) ──→ done
      │
      ├──→ R3 (view)
      ├──→ R4 (log+raft)
      └──→ R7 (client)

R5 (admin) ──→ done (independent)
R6 (auth) ──→ done (independent)
R8 (operational) ──→ done (independent)
```

R1 is THE critical path. Everything else follows.

### Original H1-H12 tracking (for completion audit)

| Phase | Status | Notes |
|-------|--------|-------|
| H1 Crypto | PARTIAL | shred.rs + cache.rs created, 6 BDD scenarios fixed. Remaining: inline assertions need pipeline. |
| H2 Log | NOT STARTED | |
| H3 View | PARTIAL | 9 scenarios fixed with real ViewStore assertions |
| H4 NFS | NOT STARTED | |
| H5 S3 | NOT STARTED | |
| H6 Gateway | NOT STARTED | Merged into R1 |
| H7 Admin | PARTIAL | 9 more scenarios with pool/state assertions |
| H8 Auth | PARTIAL | 7 scenarios with cert/identity assertions |
| H9 Client | NOT STARTED | |
| H10 Operational | NOT STARTED | |
| H11 Chunk | PARTIAL | 5 scenarios with crypto+EC assertions. Needs pipeline. |
| H12 Raft | NOT STARTED | |
