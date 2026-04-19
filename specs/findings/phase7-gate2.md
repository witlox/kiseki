# Phase 7 — Adversarial Gate-2 Findings

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-composition` at `a29d48d`.

---

## Finding: No refcount integration with chunk store

Severity: **High**
Category: Correctness > Specification compliance
Location: `crates/kiseki-composition/src/composition.rs:104-131`
Spec reference: I-C2, composition.feature

**Description**: `CompositionOps::create` stores chunk references in
the composition but never calls `ChunkOps::increment_refcount`. When a
composition is deleted, it never calls `decrement_refcount`. The spec
requires that chunk refcounts track composition references — a chunk
with refcount 0 is GC-eligible.

Without this, chunk GC will either delete referenced chunks (data loss)
or never delete unreferenced chunks (storage leak).

**Suggested resolution**: Either inject a `ChunkOps` dependency into
`CompositionStore`, or emit `RefcountDelta` events that the
integration layer routes to the chunk store. The trait boundary is
the right place for this wiring — defer to Phase 12 integration.

**Status**: OPEN — blocking for integration, non-blocking for Phase 7
trait correctness (the trait contract is correct; the in-memory store
is a reference implementation that doesn't integrate with other crates).

---

## Finding: Multipart has no abort cleanup (orphan chunks)

Severity: **Medium**
Category: Robustness > Resource exhaustion
Location: `crates/kiseki-composition/src/multipart.rs:70-77`
Spec reference: composition.feature §multipart

**Description**: `MultipartUpload::abort` sets state to `Aborted` but
doesn't track which chunk IDs need refcount decrements. The aborted
upload's parts become orphan chunks with refcount > 0 that are never
GC'd.

**Suggested resolution**: On abort, emit chunk IDs for refcount
decrement. Same integration pattern as create/delete.

**Status**: OPEN — non-blocking (same integration deferral).

---

## Finding: No versioning implementation

Severity: **Medium**
Category: Correctness > Specification compliance
Location: `crates/kiseki-composition/src/composition.rs`
Spec reference: composition.feature §versioning

**Description**: The `Composition` struct has a `version: u64` field
but `CompositionOps` has no `update` method that creates a new version.
The build-phases spec lists "Object versioning" as a Phase 7 deliverable.

**Suggested resolution**: Add `update` to `CompositionOps` that bumps
the version and updates chunk refs.

**Status**: RESOLVED — `update` method added to `CompositionOps`, test added.

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| High | 1 | Deferred to integration |
| Medium | 2 | 1 blocking (versioning) |

**Blocking**: Add versioning support to `CompositionOps`.
