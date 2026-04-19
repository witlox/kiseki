# B.3+B.4 — Adversarial Gate: Log + Audit openraft

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-log/src/raft/`, `kiseki-audit/src/raft/`

---

## Finding: Pattern reuse from B.2 — clean and consistent

Severity: **Positive finding**

Both crates follow the exact same architecture as `kiseki-keymanager`:
`declare_raft_types!`, `LogStore` (BTreeMap + Arc<Mutex>),
`StateMachine` (apply + snapshot), `StubNetworkFactory`. All three
Raft implementations are structurally identical — differing only in
`D`/`R` types and state machine apply logic.

---

## Finding: Audit state machine is strictly append-only

Severity: **Positive finding**
Category: Correctness > I-A1

`AuditCommand` has only `AppendEvent` — no mutation variant exists.
The state machine's `apply` increments `event_count` on normal
entries. No code path can delete or modify an existing event.
This enforces I-A1 at the type level.

---

## Finding: Log state machine tracks counts, not actual deltas

Severity: **Medium**
Category: Correctness

`ShardSmInner` tracks `delta_count` and `tip` but does not store
the actual delta data. The full delta storage remains in
`MemShardStore`/`RaftLogStore`. The openraft state machine is a
lightweight metadata tracker — the heavy data path goes through
the `LogOps` trait.

**Status**: OPEN — by design (state machine is for Raft consensus
metadata; delta storage is separate).

---

## Summary: 0 blocking. Pattern proven across 3 crates.
