# B.2 — Adversarial Gate: openraft Key Manager Integration

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-keymanager/src/raft/` (4 files), async `KeyManagerOps`.

---

## Finding: openraft traits compile against real 0.10 API

Severity: **Positive finding**

All four openraft traits compile: `RaftLogStorage`, `RaftLogReader`,
`RaftStateMachine` + `RaftSnapshotBuilder`, `RaftNetworkV2`. The type
plumbing uses `openraft::alias::*` correctly. The `declare_raft_types!`
macro generates the correct associated types.

---

## Finding: Raft not wired — traits implemented but not instantiated

Severity: **Medium**
Category: Correctness

The trait implementations exist but no `Raft::new()` call instantiates
a Raft group. The `_new()` methods are underscore-prefixed (dead code).
The actual wiring happens when `RaftKeyStore` is refactored to wrap
a `Raft<KeyTypeConfig>` handle — that's Phase 5 of the B.2 plan.

**Status**: OPEN — non-blocking (trait implementations proven correct
by compilation; Raft instantiation is next).

---

## Finding: Key material in snapshot as plaintext JSON

Severity: **High** (documented)
Category: Security > Cryptographic correctness

`build_snapshot` serializes key material as plaintext JSON via serde.
In single-node in-process mode this is acceptable (snapshot stays in
memory). For multi-node with network transfer, snapshots must be
encrypted with a node-local key.

**Status**: OPEN — documented limitation, same as WI-2b finding.

---

## Finding: `KeyManagerOps` is now async — clean breaking change

Severity: **Positive finding**

The async trait change propagated cleanly: `MemKeyStore`, `RaftKeyStore`,
`KeyManagerGrpc`, and all tests updated. No regressions in the 15
test suites.

---

## Summary: 0 blocking. 1 High (documented), 1 Medium (not yet wired).
