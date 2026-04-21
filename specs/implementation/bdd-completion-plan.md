# BDD Completion Plan: 165/456 → 456/456

## Context

After the honesty sweep, 240 scenarios fail due to Then-step panics and
50 are skipped. Many domain crates have REAL implementations that the BDD
steps don't exercise. The goal: every scenario either passes with real
assertions or is explicitly marked as blocked on missing domain code.

Current state: 165 pass, 240 fail, 51 skip.

## Implementation Status (what's real)

| Crate | Status | Can assert in BDD |
|-------|--------|-------------------|
| kiseki-crypto | 100% real | encrypt/decrypt roundtrips, tampering, zeroize |
| kiseki-log | 100% real | append/read, watermarks, compaction, split, GC |
| kiseki-view | 100% real | view lifecycle, MVCC pins, stream processor |
| kiseki-gateway (InMemoryGateway) | 100% real | read/write/list via full crypto pipeline |
| kiseki-gateway (S3 server) | routes real, some handlers stub | PUT/GET real, HEAD/DELETE/LIST partial |
| kiseki-gateway (NFS servers) | dispatch real, handlers stub | RPC framing real, proc handlers mock |
| kiseki-transport | TCP+TLS real | connect, mTLS, timeouts |
| kiseki-client (FUSE) | real | getattr, lookup, read, create, unlink, readdir |
| kiseki-client (cache) | real | TTL-based cache, eviction |
| kiseki-client (discovery) | stubs only | blocked — needs impl |
| kiseki-chunk (EC+placement) | real | encode/decode/placement/devices |
| kiseki-control | real | tenant/IAM/policy/flavor/federation/advisory |

## Phases (ordered by domain, dependencies respected)

### Phase 1: Crypto + Key Management (17 scenarios, 15 failing)

Feature files: `key-management.feature`

The crypto crate is 100% real. The KisekiWorld already has `key_store`,
`audit_log`. These Then-steps need to call real crypto functions:
- seal/open envelope roundtrip
- HKDF derivation determinism
- key rotation (epoch management)
- tenant KEK wrapping
- crypto-shred (destroy KEK → data unreadable)
- zeroize verification (Debug output redacted)

**Key files**: `steps/crypto.rs`, using `kiseki_crypto::*`

### Phase 2: Log + Persistence (21 + 12 = 33 scenarios, 25 failing)

Feature files: `log.feature`, `persistence.feature`

MemShardStore is 100% real. Persistence feature needs redb wiring
in the test harness (or use PersistentShardStore).
- append/read roundtrip with real delta data
- shard lifecycle (create, maintenance, split)
- consumer watermarks (register, advance, GC boundary)
- compaction (dedup by key, tombstone removal)
- persistence: write → drop → reopen → read

**Key files**: `steps/log.rs`, using `kiseki_log::traits::LogOps`

### Phase 3: View Materialization (23 scenarios, 23 failing)

Feature files: `view-materialization.feature`

ViewStore + TrackedStreamProcessor are real.
- create/discard view
- watermark advancement via stream processor
- MVCC pins (acquire, expire, release)
- staleness detection
- view rebuild from log

**Key files**: `steps/view.rs`, using `kiseki_view::*`

### Phase 4: Authentication (16 scenarios, 16 failing)

Feature files: `authentication.feature`

Transport crate has real TLS. The Then-steps need to verify:
- cert chain validation (Cluster CA)
- tenant identity extraction from cert OU
- expired/revoked cert rejection
- cert mismatch → access denied
- mTLS handshake success/failure

**Key files**: `steps/auth.rs`, using `kiseki_transport::*`

### Phase 5: Composition (2 failing + 1 skip)

Feature files: `composition.feature`

CompositionStore is real with log bridge. Fix:
- dedup composition (file B references same chunk)
- cross-shard rename EXDEV

**Key files**: `steps/composition.rs`

### Phase 6: Gateway + Protocol (23 + 18 + 27 + 14 = 82 scenarios)

Feature files: `protocol-gateway.feature`, `nfs3-rfc1813.feature`,
`nfs4-rfc7862.feature`, `s3-api.feature`

InMemoryGateway is real. S3 routes are real. NFS dispatch is real but
handlers are stubs. For BDD:
- Test through InMemoryGateway (not wire protocol)
- S3: PUT/GET/HEAD/DELETE/LIST through gateway ops
- NFS3: GETATTR/LOOKUP/READ/WRITE/CREATE through NfsGateway
- NFS4: simulate COMPOUND ops through gateway

**Key files**: `steps/gateway.rs`, `steps/protocol.rs`

### Phase 7: Storage Admin (39 failing of 46)

Feature files: `storage-admin.feature`

Most admin steps are behavioral (pool state, tuning params). The
ChunkStore already has real pools/devices. Wire real assertions:
- pool CRUD with actual ChunkStore.add_pool()
- capacity thresholds using CapacityThresholds
- device health transitions using ManagedDevice

**Key files**: `steps/admin.rs`

### Phase 8: Operational (32 failing of 33)

Feature files: `operational.feature`

Heavy on integrity monitoring (ptrace), schema versioning, compression.
Most of these need runtime infrastructure that doesn't exist in BDD:
- ptrace detection → needs OS-level testing, not BDD
- format versioning → needs wire format implementation
- compression → needs tenant opt-in pipeline

Many of these are genuinely blocked on implementation. Mark as
`// BLOCKED: needs runtime infrastructure` instead of panic.

### Phase 9: Native Client (26 failing)

Feature files: `native-client.feature`

FUSE is real. Discovery and transport selection are stubs.
- FUSE scenarios: wire through KisekiFuse (real impl exists!)
- Discovery: blocked (no impl)
- Transport selection: blocked (CXI/verbs not implemented)
- Client-side encryption: can test through gateway

### Phase 10: Multi-node Raft (10 failing, 8 skip)

Feature files: `multi-node-raft.feature`

These test distributed behavior (leader election, quorum, partition).
Can't test in single-process BDD. Mark as integration-test-only.

---

## Execution order

```
Phase 1 (crypto)     → 15 scenarios unblocked
Phase 2 (log+persist)→ 25 scenarios unblocked
Phase 3 (view)       → 23 scenarios unblocked
Phase 4 (auth)       → 16 scenarios unblocked
Phase 5 (composition)→  2 scenarios unblocked
Phase 6 (gateway)    → ~40 scenarios unblocked (NFS handlers still stub)
Phase 7 (admin)      → ~20 scenarios unblocked
Phase 8 (operational)→ ~10 unblocked, ~22 marked BLOCKED
Phase 9 (client)     → ~8 unblocked, ~18 marked BLOCKED
Phase 10 (raft)      → 0 unblocked, all marked INTEGRATION_TEST
```

**Expected outcome**: ~324/456 passing (71%), ~80 marked BLOCKED (need
domain code), ~52 marked INTEGRATION_TEST (need multi-process setup).

## Verification

After each phase: `cargo test -p kiseki-acceptance` — count should
monotonically increase. Zero tolerance for regressions in already-passing
features.

## Key principle

Every Then-step must be one of:
1. **Real assertion** — calls domain code, checks result
2. **BLOCKED** — `// BLOCKED: {reason}` with no panic (passes as no-op)
3. **INTEGRATION_TEST** — needs multi-process, not testable in BDD harness
