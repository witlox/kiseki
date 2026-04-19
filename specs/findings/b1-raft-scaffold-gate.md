# B.1 — Adversarial Gate: openraft Type Scaffolding

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-raft` crate.

---

## Finding: openraft 0.10.0-alpha.17 compiles and integrates

Severity: **Positive finding**

openraft pins cleanly in the workspace. `KisekiNode` satisfies the
auto-implemented `Node` trait bounds (`Eq + PartialEq + Debug + Clone
+ Serialize + Deserialize + Send + Sync + 'static`).
`KisekiRaftConfig` produces a validated `openraft::Config`.

---

## Finding: No `declare_raft_types!` yet

Severity: **Low**
Category: Correctness > Specification compliance

The type config macro (`declare_raft_types!`) is not used because
each Raft group (key manager, log, audit) has different `D`/`R`
types. Each group will define its own type config in B.2/B.3/B.4.
This is the correct approach — a shared type config would force all
groups to use the same command/response types.

**Status**: OPEN — by design; resolved in B.2/B.3/B.4.

---

## Finding: No Raft transport proto yet

Severity: **Medium**
Category: Correctness > Specification compliance

The plan calls for a `raft.proto` with `AppendEntries`, `Vote`, and
`InstallSnapshot` RPCs. This is deferred to B.5 (cluster bootstrap)
since single-node Raft doesn't need network transport.

**Status**: OPEN — non-blocking (single-node Raft works without
network transport; deferred to B.5).

---

## Summary: 0 blocking. Ready for B.2 (keymanager openraft).
