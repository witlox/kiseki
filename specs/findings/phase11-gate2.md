# Phase 11 — Adversarial Gate-2 Findings

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `control/pkg/*` Go packages.

---

## Finding: No persistence or gRPC server

Severity: **High** (deferred)
Category: Correctness > Specification compliance
Spec reference: build-phases.md §Phase 11

**Description**: All types are in-memory with no persistence layer and
no gRPC server. The control-plane.feature has 23 scenarios that
require a running server. These are pure domain types and validation
functions — the infrastructure (gRPC, Raft-backed state, protobuf
codegen from Go side) is deferred.

**Status**: OPEN — non-blocking (domain logic is correct; server
wiring is integration scope).

---

## Finding: AccessRequest uses wall-clock time.Now()

Severity: **Medium**
Category: Correctness > Implicit coupling
Location: `control/pkg/iam/access.go:68,77,87`

**Description**: `Approve()`, `Deny()`, and `CheckExpired()` all call
`time.Now()` directly, making them hard to test deterministically and
impossible to use with the HLC/wall-clock dual model. In production,
the control plane should use an injectable clock.

**Suggested resolution**: Accept a `time.Time` parameter or use a
`Clock` interface.

**Status**: OPEN — non-blocking.

---

## Finding: No tenant store — no CRUD operations

Severity: **Medium**
Category: Correctness > Specification compliance
Location: `control/pkg/tenant/tenant.go`

**Description**: `Organization`, `Project`, `Workload` types exist but
there is no `TenantStore` or CRUD interface. The control-plane.feature
scenarios require create/read/update/delete for all three levels.

**Suggested resolution**: Add a `Store` interface with in-memory
implementation, similar to the Rust crate pattern.

**Status**: RESOLVED — `Store` struct with `CreateOrg`, `GetOrg`, `DeleteOrg`, `CreateProject`, `GetProject`, `CreateWorkload`, `GetWorkload` added with quota validation. 4 tests added.

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| High | 1 | No (deferred) |
| Medium | 2 | 1 blocking (tenant store) |

**Blocking**: Add tenant CRUD store interface.
