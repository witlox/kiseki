# Phase 13d: Remaining @integration Scenarios (in progress)

## Current state (2026-04-25)

133/241 @integration scenarios passing, 105 failing, 3 skipped.
1028 crate unit tests passing. Clippy clean.

## Completed streams

| Stream | Target | Result |
|--------|--------|--------|
| Stream A | operational.rs — key epoch | Done (real KeyManagerOps::rotate) |
| Stream C | admin.rs — StorageAdmin auth | Already passing |
| Stream D | gateway.rs — multipart upload | Done (real gateway.start/upload/complete) |
| Stream E | Phase B audit (read-only) | Done — 0 stubs, 14 weak/structural |

## Remaining streams

### Stream B: persistence (14 scenarios) — NEXT
- Wire PersistentShardStore into World
- Implement restart simulation (drop + reopen from same path)
- Touches: acceptance.rs, steps/protocol.rs

### Deferred (separate session, needs architect input)

| Item | Scenarios | Blocker |
|------|-----------|---------|
| Multi-node Raft | 30 | In-process TCP Raft harness |
| ADR-035 node lifecycle | ~10 | Depends on Raft harness |
| Device health subscription | 3 | New subsystem |
| Telemetry/metrics | 8 | New subsystem |
| FUSE/mmap | 6 | New subsystem |
| Gateway TCP transport | 2 | Partial — needs transport layer |
| Scattered (crash recovery, KMS, advisory) | ~46 | Various |

## 105 failing by infrastructure blocker

| Blocker | Count | Production code | Complexity |
|---------|-------|-----------------|------------|
| Multi-node Raft | 30 | Exists (openraft) | Heavy |
| Persistence/redb | 14 | Exists (PersistentShardStore) | Moderate |
| Key epoch (Background step) | 8 | Wired (rotate loop) | Done — 3 now skip at later step |
| Scattered | 53 | Various | Various |

## Prior plan debt

| Source | Item | Status |
|--------|------|--------|
| BDD Phase B | 37 PARTIAL crate tests | Verified — 0 stubs |
| ADR-032 Step 8 | KISEKI_RAFT_THREADS docs | Not done (low priority) |
| ADR-032 Step 8 | Concurrent S3 PUT test | Not done (needs cluster) |
| Implementer plan | Wire PersistentShardStore | Stream B (next) |
| Implementer plan | ADR-035 node lifecycle | Deferred |
