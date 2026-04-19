# Infrastructure Integration Plan

**Status**: Active. **Created**: 2026-04-19.
**Owner**: implementer role.
**Prerequisite**: All 13 build phases (0-12) complete at trait/domain level.

This plan covers the four infrastructure work items that turn the
trait-level implementation into a running distributed system. Each
step gets an adversarial gate before advancing.

---

## Dependency graph

```
WI-1 (fidelity sweep) ─────────────────────────────── independent
WI-2a (trait prep) ──[adv]──┐
  WI-2b (keymanager raft) ──[adv]
  WI-2c (log raft) ─────────[adv]
  WI-2d (audit raft) ────────[adv]
WI-3 (gRPC wiring) ─────────[adv]──────────────────────┐
                                                         v
                                              WI-4 (server runtime) ──[adv]
```

WI-1 is fully parallel with everything. WI-2 and WI-3 can run in
parallel after WI-2a (trait signatures must stabilize first). WI-4
depends on both WI-2 and WI-3.

---

## WI-1: Auditor Fidelity Sweep

**Nature**: Review activity, no code changes.

| Step | Description | Output |
|------|-------------|--------|
| 1a | Inventory all crates against feature files, invariants, ADRs | `specs/fidelity/SWEEP.md` |
| 1b | Per-crate audit: scenario→test mapping, depth classification | `specs/fidelity/<crate>.md` |
| 1c | Go package audit | `specs/fidelity/go-control.md` |
| 1d | Cross-cutting: dead specs, orphan tests, unenforced invariants | `specs/fidelity/gaps.md` |
| 1e | Aggregate confidence levels per bounded context | `specs/fidelity/INDEX.md` |

**Adversary gate (WI-1-adv)**: after 1e. Challenges HIGH ratings,
validates LOW risk characterization.

**Exit criteria**: `specs/fidelity/INDEX.md` exists with all bounded
contexts rated. Adversary signs off.

---

## WI-2: Raft Integration

### WI-2a: Trait Surface Preparation

The critical breaking change. Must complete before WI-2b/2c/2d and
before WI-3.

| Change | File | What |
|--------|------|------|
| `LogOps` async | `kiseki-log/src/traits.rs` | `&mut self` → `&self`, methods return `impl Future` |
| `KeyManagerOps` return type | `kiseki-keymanager/src/epoch.rs` | `&SystemMasterKey` → `Arc<SystemMasterKey>` |
| Extract `AuditOps` trait | `kiseki-audit/src/store.rs` | New trait from concrete `AuditLog` |
| Update `MemXxxStore` | all three crates | Implement revised traits |
| Fix downstream consumers | composition, view, server, tests | Adjust to new signatures |

**Adversary gate (WI-2a-adv)**: Do new signatures preserve all
invariants? Any regression in existing tests?

### WI-2b: kiseki-keymanager Raft Store

Simplest Raft use case — tiny state (epochs × 32 bytes).

- Add `openraft`, `tokio`, `serde` to `kiseki-keymanager`
- `RaftKeyStore`: state machine for `CreateEpoch`, `RotateToEpoch`,
  `MarkMigrationComplete`
- Snapshot: serialize full epoch map
- Tests: single-node round-trip, 3-node replication, leader failover

**Adversary gate (WI-2b-adv)**: Key material in Raft log — encrypted?
Node-local log encryption? `Zeroizing` on deserialization?

### WI-2c: kiseki-log Raft Store

Most complex — per-shard Raft groups.

- Add `openraft`, `rocksdb` to `kiseki-log`
- `RaftShardStore`: SSTable-backed delta storage via RocksDB
- Raft entries: `AppendDelta`, `SetMaintenance`, `Compact`, `Truncate`
- Shard split creates new Raft group
- Tests: leader election, quorum loss → `QuorumLost`, compaction
  through Raft

**Adversary gate (WI-2c-adv)**: Shard split Raft lifecycle — orphaned
state? I-L3 immutability through consensus?

### WI-2d: kiseki-audit Raft Store

Append-only, no compaction.

- Per-tenant Raft groups, append-only state machine
- Tests: append through Raft, tenant isolation, watermark/GC boundary

**Adversary gate (WI-2d-adv)**: I-A1 append-only guarantee enforced
at state machine level?

**Exit criteria for WI-2**: All Raft stores pass tests. `MemXxxStore`
impls still pass all existing tests. Workspace green.

---

## WI-3: gRPC Server Wiring

Can run in parallel with WI-2b/2c/2d (after WI-2a completes).

| Step | Service | Language | Proto |
|------|---------|----------|-------|
| 3a | KeyManagerService | Rust | key.proto |
| 3b | ControlService | Go | control.proto |
| 3c | AuditExportService | Go | audit.proto |
| 3d | WorkflowAdvisoryService | Rust | advisory.proto |

Each service wraps the domain trait (`Arc<dyn XxxOps>`) and maps
errors to `tonic::Status` codes per the error taxonomy.

**Adversary gate (WI-3-adv)**: All 4 services reviewed together.
Proto↔domain mapping completeness, error code taxonomy, mTLS
interceptor wiring, advisory covert-channel hardening (I-WA15).

**Exit criteria**: Each service has unit tests exercising all RPCs.
Proto round-trip tests pass. `cargo test` and `go test` green.

---

## WI-4: Server Runtime Composition

Depends on WI-2 and WI-3.

| Step | Description |
|------|-------------|
| 4a | Scaffold: `#[tokio::main]`, config, CLI args |
| 4b | Data-path runtime: Raft stores + transport listener + context injection |
| 4c | Advisory isolated runtime (ADR-021 §1): second tokio runtime, arc-swap lookup |
| 4d | TCP+TLS listener with mTLS identity extraction |
| 4e | Discovery responder + node health reporting |
| 4f | Graceful shutdown: drain → flush Raft → close audit → exit |

**Adversary gate (WI-4-adv)**: Runtime isolation proof (advisory
overload test), no unauthenticated path, crash safety for committed
entries, context injection completeness.

**Exit criteria**: Server starts, binds listeners, accepts mTLS,
serves all gRPC services. Integration test: write delta via gRPC →
read back → verify audit event. Advisory overload test: data-path
latency unaffected (F-ADV-1).

---

## Tracking

| Step | Status | Commit | Adv gate | Notes |
|------|--------|--------|----------|-------|
| WI-1 | done | — | passed | Fidelity sweep: 1 HIGH, 4 MEDIUM, 8 LOW; 23% scenario coverage |
| WI-2a | done | — | passed | LogOps &self, KeyManagerOps Arc, AuditOps trait extracted |
| WI-2b | done | — | passed | RaftKeyStore: command log, state machine, replay, 6 tests |
| WI-2c | done | — | passed | RaftLogStore: per-shard command log, 3 tests |
| WI-2d | pending | — | — | Audit Raft |
| WI-3 | pending | — | — | gRPC wiring |
| WI-4 | pending | — | — | Server runtime |
