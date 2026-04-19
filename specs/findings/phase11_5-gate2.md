# Phase 11.5 — Adversarial Gate-2 Findings

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-advisory`.

---

## Finding: No gRPC server or isolated tokio runtime

Severity: **High** (deferred)
Spec reference: ADR-021 §1

**Description**: The advisory runtime should run on an isolated tokio
runtime with a separate gRPC listener. Currently it's pure domain
types with no runtime. Deferred to Phase 12 integration.

**Status**: OPEN — non-blocking (domain logic correct).

---

## Finding: No arc-swap snapshot for AdvisoryLookup

Severity: **Medium**
Spec reference: ADR-021 §4, I-WA2

**Description**: `AdvisoryLookup` takes a `&WorkflowTable` reference
directly. In production, it should use an `arc-swap` snapshot with a
≤500µs deadline to ensure the data path is never blocked.

**Status**: OPEN — non-blocking (in-memory ref impl is correct).

---

## Finding: No k-anonymity bucketing for telemetry

Severity: **Medium**
Spec reference: ADR-021 §7, I-WA5

**Description**: No telemetry emission or k-anonymity bucketing is
implemented. The budget enforcer and workflow table are correct, but
the telemetry feedback path is absent.

**Status**: OPEN — non-blocking (Phase 12 integration).

---

## Summary: 0 blocking (1 High + 2 Medium, all deferred).
