# Phase 8 — Adversarial Gate-2 Findings

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-view` at `45255bd`.

---

## Finding: No staleness tracking or alerting

Severity: **Medium**
Category: Correctness > Specification compliance
Location: `crates/kiseki-view/src/view.rs`
Spec reference: I-K9, I-V3, view-materialization.feature §staleness

**Description**: The `ViewDescriptor` has a `ConsistencyModel` with
`BoundedStaleness { max_staleness_ms }` but `advance_watermark` and
`get_view` never check whether the view has fallen behind its
staleness bound. No `StalenessViolation` error is ever returned.

**Suggested resolution**: Compare the view's watermark timestamp
against `now - max_staleness_ms` on read operations. Return
`ViewError::StalenessViolation` when the bound is exceeded.

**Status**: RESOLVED — `check_staleness` added to `MaterializedView`, `last_advanced_ms` tracked on watermark advance, `advance_watermark` now takes `now_ms`. Test added.

---

## Finding: No stream processor — delta consumption not implemented

Severity: **Medium**
Category: Correctness > Specification compliance
Location: `crates/kiseki-view/src/` (missing module)
Spec reference: view-materialization.feature §stream processor

**Description**: The stream processor that consumes deltas from the
Log and materializes the view is not implemented. `advance_watermark`
is called externally but the actual delta-consumption logic (decrypt
payload, apply to materialized state) is absent.

**Suggested resolution**: This is the core of view materialization and
requires integration with `kiseki-log` and `kiseki-crypto`. Defer to
Phase 12 integration.

**Status**: OPEN — non-blocking (integration scope).

---

## Finding: Pin ID overflow not handled

Severity: **Low**
Category: Correctness > Edge cases
Location: `crates/kiseki-view/src/view.rs:158-159`

**Description**: `next_pin_id` is a `u64` that increments without
overflow check. At `u64::MAX`, the next increment would wrap to 0 in
release mode. Practical risk is nil (2^64 pins) but violates the
overflow-checks policy.

**Status**: OPEN — non-blocking.

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| Medium | 2 | 1 blocking (staleness) |
| Low | 1 | No |

**Blocking**: Staleness tracking on read operations.
