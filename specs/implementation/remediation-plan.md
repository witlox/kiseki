# Remediation Plan — Honest Path to Functional System

**Created**: 2026-04-19. **Status**: Planned.

This plan addresses all gaps identified in the post-audit assessment.
Ordered by: CI fix (blocking) → foundation repair → data path →
integration → protocol → polish.

---

## Phase R0: Fix CI (immediate, blocks everything)

| # | What | Why |
|---|------|-----|
| R0.1 | Fix clippy errors in test code | `make` fails, CI is broken |
| R0.2 | Run `make` to green | Can't merge anything until CI passes |

**Exit**: `make` passes locally. Push succeeds with CI green.

---

## Phase R1: Test the "resolved" adversarial findings

13 findings marked "RESOLVED" — do they actually have tests?

| Finding | Claimed fix | Has test? |
|---------|-----------|-----------|
| HLC monotonicity (phase0) | `Result<Self, HlcExhausted>` | YES — 7 boundary tests |
| Proptest boundaries (phase0) | Boundary tests added | YES — deterministic tests |
| Zeroize removed (phase0) | Dep removed | N/A (removal) |
| SequenceNumber::checked_next (phase0) | Method added | NO — no test calls it |
| mlock (phase1) | mem_protect.rs | NO — no test verifies mlock |
| Decompression bomb (phase1) | max_plaintext_size | YES — size limit test |
| Padding overflow (phase1) | Error on overflow | NO — no test for overflow |
| Unwrapped chunk_id check (phase1) | Explicit check added | NO — no test for mismatch |
| Tenant CRUD store (phase11) | Store added | YES — 4 Go tests |
| Compaction (phase3) | compact_shard added | YES — 2 tests |
| Versioning (phase7) | update() added | YES — 1 test |
| Staleness check (phase8) | check_staleness added | YES — 1 test |
| Audit replay (wi2d) | replay() added | YES — 1 test |

**Action**: Write tests for the 4 "resolved" findings that lack them:
- `SequenceNumber::checked_next()` unit test
- mlock verification test (check return value)
- Padding overflow test (compressed size near usize::MAX)
- Unwrapped chunk_id mismatch test

---

## Phase R2: Validate open adversarial findings are tracked

67 open findings across 21 documents. Most are marked "non-blocking"
and "deferred." The danger is they get forgotten.

**Action**:
- Create `specs/findings/OPEN-FINDINGS.md` — single index of all open
  findings, grouped by priority
- For each: does a BDD scenario exist that would catch the regression?
  If not, note which scenario it maps to
- Accept that "deferred" means "this BDD scenario stays skipped"

---

## Phase R3: Cross-context integration (the data path)

This is the fundamental gap. The 6 "half done" crates work in
isolation but aren't connected.

| # | What | BDD scenarios it enables |
|---|------|------------------------|
| R3.1 | Composition → Log: create emits delta | composition.feature: ~5 scenarios |
| R3.2 | Composition → Chunk: create increments refcount | chunk-storage.feature: ~3 scenarios |
| R3.3 | Composition → Chunk: delete decrements refcount | chunk-storage.feature: ~2 scenarios |
| R3.4 | Chunk → Crypto: write encrypts, read decrypts | chunk-storage.feature: ~4 scenarios |
| R3.5 | View → Log: stream processor consumes deltas | view.feature: ~3 scenarios |
| R3.6 | Audit: domain ops emit audit events | operational.feature: ~5 scenarios |

**Approach**: BDD-first.
1. Pick a scenario (e.g., "Write a chunk with content-addressed ID")
2. Write the BDD step with real end-to-end assertion
3. It fails (red) because composition doesn't call chunk
4. Wire the integration
5. It passes (green)
6. Next scenario

**Exit**: At least 20 BDD scenarios passing (up from 3).

---

## Phase R4: Wire Raft for log + audit

Keymanager has working Raft (OpenRaftKeyStore). Log and audit have
openraft traits that compile but no Raft::new() instantiation.

| # | What |
|---|------|
| R4.1 | Create `OpenRaftLogStore` following OpenRaftKeyStore pattern |
| R4.2 | Create `OpenRaftAuditStore` following same pattern |
| R4.3 | Integration test: write delta through Raft, read back |
| R4.4 | Integration test: append audit event through Raft |

**Exit**: I-L2 (Raft durability) tested. I-A1 (audit through Raft) tested.

---

## Phase R5: Data-path gRPC

No way to write/read over the network. Server has 0 data-path
services wired.

| # | What |
|---|------|
| R5.1 | Add LogService proto definition |
| R5.2 | Implement LogGrpc in kiseki-log |
| R5.3 | Register in kiseki-server runtime |
| R5.4 | Integration test: gRPC write → read round-trip |

**Exit**: Can write a delta via gRPC and read it back.

---

## Phase R6: Test adversarial "resolved" fixes (from R1)

Write the 4 missing tests identified in R1.

---

## Phase R7: Protocol implementations (NFS, S3, FUSE)

These are the Stage D items. Each enables ~20 BDD scenarios.

| # | What |
|---|------|
| R7.1 | S3 gateway (GetObject, PutObject) |
| R7.2 | NFS gateway (read, write, readdir) |
| R7.3 | FUSE client mount |
| R7.4 | Python e2e test suite |

---

## Phase R8: Go control plane gRPC + proto codegen

| # | What |
|---|------|
| R8.1 | Go protobuf codegen from specs/architecture/proto/ |
| R8.2 | ControlService gRPC server |
| R8.3 | AuditExportService gRPC server |
| R8.4 | Wire more godog steps for control-plane.feature |

---

## Phase R9: Infrastructure debt

| # | What |
|---|------|
| R9.1 | Lefthook pre-commit hooks |
| R9.2 | Client bindings (PyO3 stub) |
| R9.3 | Docker compose for local dev |
| R9.4 | Validate specs/failure-modes.md against code |
| R9.5 | Validate specs/assumptions.md — mark invalidated ones |
| R9.6 | Validate specs/cross-context/interactions.md |

---

## Phase R10: Polish

| # | What |
|---|------|
| R10.1 | Update README to match reality |
| R10.2 | Update implementation plan tracking tables |
| R10.3 | Final fidelity sweep |
| R10.4 | Open findings index maintained |

---

## Tracking

| Phase | Status | BDD scenarios passing |
|-------|--------|----------------------|
| R0 | pending | 3 |
| R1 | pending | 3 |
| R2 | pending | 3 |
| R3 | pending | target: 20+ |
| R4 | pending | target: 25+ |
| R5 | pending | target: 30+ |
| R6 | pending | 30+ |
| R7 | pending | target: 80+ |
| R8 | pending | target: 90+ |
| R9 | pending | 90+ |
| R10 | pending | final |
