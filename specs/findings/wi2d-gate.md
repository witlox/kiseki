# WI-2d — Adversarial Gate: Raft Audit Store

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-audit/src/raft_store.rs`

---

## Finding: Append-only guarantee enforced by API, not by type

Severity: **Medium**
Category: Correctness > Specification compliance
Location: `raft_store.rs:18-30`
Spec reference: I-A1

**Description**: The `AuditCommand` enum has only `AppendEvent` — no
delete or mutate variant exists. This enforces I-A1 (append-only) at
the API level. However, the `Inner` struct holds `shards` and `log` in
a `Mutex<Inner>` — any code with `&self` access could theoretically
add a new command variant and mutate state.

In practice, the `Inner` type is private and `RaftAuditStore` only
exposes `AuditOps` methods (all append). The compile-time guarantee is
that no `AuditCommand` variant can cause deletion. This is correct
but should be documented: **the append-only invariant is enforced by
the exhaustive `AuditCommand` enum having no mutation variants.**

**Status**: OPEN — non-blocking (correctly enforced, needs documentation).

---

## Finding: No replay capability

Severity: **Medium**
Category: Correctness > Implicit coupling
Location: `raft_store.rs` (missing `replay` method)

**Description**: Unlike `RaftKeyStore` which has a `replay()` method,
`RaftAuditStore` has no way to rebuild state from the command log. For
Raft snapshot restore, the state machine must be rebuildable from the
log. The `event_type_from_str` function was removed during clippy
cleanup but would be needed for replay.

**Suggested resolution**: Add `replay()` and restore
`event_type_from_str` (gated behind the replay path).

**Status**: RESOLVED — `replay()` added with `event_type_from_str` deserialization. Test added.

---

## Finding: Timestamp lost in command serialization

Severity: **Low**
Category: Correctness > Specification compliance
Location: `raft_store.rs:116-121`

**Description**: The `AuditCommand::AppendEvent` does not carry the
event's `DeltaTimestamp`. The full `AuditEvent` (with timestamp) is
applied directly to the shard state, but the serialized command log
only has `event_type`, `actor`, and `description`. On replay, the
timestamp would be lost.

Same pattern as WI-2c — the command should carry the full timestamp
for faithful replay.

**Status**: OPEN — non-blocking (replay not yet implemented).

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| Medium | 2 | 1 blocking (replay) |
| Low | 1 | No |

**Blocking**: Add `replay()` to `RaftAuditStore`.
