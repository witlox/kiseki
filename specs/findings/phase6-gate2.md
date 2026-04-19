# Phase 6 — Adversarial Gate-2 Findings

**Reviewer**: adversary role. **Date**: 2026-04-19.

## Finding: No EC encoding/decoding or repair

Severity: **Medium**. Spec: I-C4, chunk-storage.feature §EC scenarios.
EC is a pool-level durability strategy. The `DurabilityStrategy` enum
exists but no encoding/decoding logic is implemented. Deferred to when
physical device placement exists.
**Status**: OPEN — non-blocking.

## Finding: No placement engine

Severity: **Medium**. Spec: I-C3.
Chunks are written to a named pool but no placement decision is made
across devices/nodes within the pool. Deferred to integration phase.
**Status**: OPEN — non-blocking.

## Summary: 0 blocking, 2 Medium deferred.
