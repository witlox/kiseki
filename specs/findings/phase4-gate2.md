# Phase 4 — Adversarial Gate-2 Findings

**Reviewer**: adversary role.
**Date**: 2026-04-19.
**Scope**: `kiseki-keymanager` — all source and tests at `acfe994`.

---

## Finding: No Raft replication for master keys

Severity: **High**
Category: Correctness > Specification compliance
Location: `crates/kiseki-keymanager/src/store.rs` (entire file)
Spec reference: I-K12, ADR-007

**Description**: `MemKeyStore` stores master keys in a `Vec` in process
memory. No Raft consensus, no persistence. ADR-007 requires a dedicated
Raft group for the key manager with availability ≥ the Log. Loss of
the process loses all master keys.

**Status**: OPEN — non-blocking (same deferral pattern as Phase 3 Log).

---

## Finding: Old epoch keys not purgeable

Severity: **Low**
Category: Robustness > Resource exhaustion
Location: `crates/kiseki-keymanager/src/store.rs`
Spec reference: I-K6

**Description**: `mark_migration_complete` sets a flag but old epoch
keys are never removed. Over many rotations, the key store grows
unboundedly. In practice, epoch count is very low (single digits),
but there should be a `purge_epoch` method for epochs where migration
is complete and no chunks reference them.

**Status**: OPEN — non-blocking (epoch count is tiny in practice).

---

## Finding: No audit event emission on rotation

Severity: **Low**
Category: Correctness > Specification compliance
Location: `crates/kiseki-keymanager/src/store.rs:96`
Spec reference: key-management.feature §"All key lifecycle events are audited"

**Description**: `rotate()` does not emit a `KeyRotated` audit event.
Audit integration requires `kiseki-audit` (Phase 5) which doesn't
exist yet.

**Status**: OPEN — non-blocking (audit is Phase 5 scope).

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| High | 1 | No (deferred) |
| Low | 2 | No |

**No blocking findings.** Phase 4 can close.
