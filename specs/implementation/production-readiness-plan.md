# Production Readiness Plan: Quality Gate → Persistence → Integration

## Context

The F1-F10 feature plan is **complete**. All 456 BDD scenarios pass
with real domain assertions. 307+ unit tests green. Server binary
compiles and boots. Docker configs exist for 1-node and 3-node.

The codebase is feature-complete at the *in-memory* level — every
domain concept is implemented with HashMap-backed stores. The next
step is hardening: update stale indexes, re-enable CI, replace
in-memory stores with real persistence, and validate via e2e tests.

---

## Phase Q1: Quality Gate (housekeeping)

**Goal**: Clean up stale artifacts, update fidelity index, re-enable CI.

1. **Update fidelity INDEX.md** — 205/456 → 456/456, update unit test
   count, mark all features at 100%, clear "Features at 0%" section
2. **Re-triage OPEN-FINDINGS.md** — many findings marked "RESOLVED"
   inline but still listed; remove resolved, re-categorize remaining
3. **Re-enable CI** — rename `.github/workflows/ci.yml.disabled` →
   `ci.yml`, verify it runs (fmt, clippy, test, deny, proto coverage)
4. **Clippy clean** — fix remaining warnings across workspace
5. **Update 100pct-completion-plan.md** — mark as COMPLETE

**Files**:
- `specs/fidelity/INDEX.md`
- `specs/findings/OPEN-FINDINGS.md`
- `specs/implementation/100pct-completion-plan.md`
- `.github/workflows/ci.yml.disabled` → `.github/workflows/ci.yml`

**Exit**: CI green, fidelity index current, findings triaged.

---

## Phase Q2: Step Audit (Task #53)

**Goal**: Verify all 456 passing scenarios exercise correct code paths.

Systematic review of each step file to confirm:
- Given steps set up real domain state (not just flags)
- When steps call real domain methods (not no-ops)
- Then steps assert on real return values (not just `is_ok()`)

Flag steps that are "technically passing but behaviorally shallow"
(e.g., `assert!(result.is_ok())` instead of checking actual values).

**Files**: All 18 files in `crates/kiseki-acceptance/tests/steps/`

**Exit**: Audit report with list of shallow steps. Fix or document each.

---

## Phase P1: Persistence — Log (redb)

**Goal**: Wire redb-backed `PersistentShardStore` into the data path.

The scaffolding exists (`kiseki-log/src/persistent.rs` via
`kiseki-raft`). Wire it so the server binary uses redb when
`KISEKI_DATA_DIR` is set, in-memory otherwise.

**Key files**:
- `crates/kiseki-log/src/persistent.rs` (extend)
- `crates/kiseki-server/src/runtime.rs` (conditional store selection)
- `crates/kiseki-raft/src/lib.rs` (redb integration)

**Exit**: Delta write→restart→read survives with redb. E2e persistence
test passes against Docker.

---

## Phase P2: Persistence — Key Manager

**Goal**: Key epochs survive restart via redb.

**Key files**:
- `crates/kiseki-keymanager/src/raft_store.rs` (wire redb backend)
- `crates/kiseki-keymanager/src/store.rs` (persistent variant)

**Exit**: Key rotation→restart→epoch preserved. E2e test.

---

## Phase P3: Persistence — Audit

**Goal**: Audit events survive restart.

**Key files**:
- `crates/kiseki-audit/src/store.rs` (persistent variant)

**Exit**: Audit append→restart→replay works.

---

## Phase P4: Persistence — Chunk (pool files)

**Goal**: Chunk data persisted to pool files on disk.

This is the largest persistence task — chunks are currently
HashMap<ChunkId, ChunkEntry>. Need a file-backed store with
offset tracking per pool device.

**Key files**:
- `crates/kiseki-chunk/src/store.rs` (add file-backed impl)
- `crates/kiseki-chunk/src/pool.rs` (device → file mapping)

**Exit**: Chunk write→restart→read returns correct data. GC
reclaims disk space.

---

## Phase I1: E2E Validation

**Goal**: Run the 19 existing e2e tests against Docker deployment.

1. `docker compose up` with single node
2. Run `tests/e2e/` suite
3. Fix any failures (likely: auth, timing, port conflicts)
4. Run 3-node cluster e2e tests

**Exit**: All 19 e2e tests green against Docker.

---

## Phase I2: Multi-Node Raft

**Goal**: Distributed Raft consensus tested end-to-end.

1. Use `docker-compose.3node.yml`
2. Write→read across nodes
3. Leader failure → election → recovery
4. Verify delta replication

**Exit**: 3-node Raft cluster handles write→failover→read.

---

## Execution order

```
Q1 (quality gate) ──→ Q2 (step audit) ──→ P1 (log persistence)
                                           ├── P2 (key persistence)
                                           ├── P3 (audit persistence)
                                           └── P4 (chunk persistence)
                                                    ↓
                                              I1 (e2e validation)
                                                    ↓
                                              I2 (multi-node raft)
```

## Verification

After each phase:
- `make verify` (fmt, clippy, deny, test, arch-check)
- `cargo test --test acceptance` (456/456 maintained)
- Phase-specific e2e test where applicable

## Estimated effort

| Phase | Sessions |
|-------|----------|
| Q1 Quality gate | 1 |
| Q2 Step audit | 1-2 |
| P1 Log persistence | 2 |
| P2 Key persistence | 1 |
| P3 Audit persistence | 1 |
| P4 Chunk persistence | 2-3 |
| I1 E2E validation | 1-2 |
| I2 Multi-node Raft | 2-3 |
| **Total** | **~12-15** |
