# Phase 9 — Adversarial Gate-2 Findings

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-gateway` at `97721a5`.

---

## Finding: No implementation behind GatewayOps trait

Severity: **Medium**
Category: Correctness > Specification compliance
Location: `crates/kiseki-gateway/src/ops.rs`
Spec reference: protocol-gateway.feature (16 scenarios)

**Description**: `GatewayOps` is a trait with no implementation. NFS
and S3 modules are empty stubs. No tests exist for this crate. The
trait signature is correct but nothing exercises it.

**Suggested resolution**: The trait is the stable contract. Protocol
implementations require heavy dependencies (NFS protocol library, S3
signature verification). Defer concrete implementations to when
protocol-specific dependencies are integrated. Add at least one mock
test that exercises the trait.

**Status**: OPEN — non-blocking (trait-only phase, no implementation
possible without protocol deps).

---

## Finding: WriteRequest carries plaintext with no size bound

Severity: **Low**
Category: Robustness > Resource exhaustion
Location: `crates/kiseki-gateway/src/ops.rs:38`
Spec reference: none

**Description**: `WriteRequest.data: Vec<u8>` has no size bound. A
malicious or buggy protocol client could send an arbitrarily large
write that consumes all gateway memory.

**Suggested resolution**: Enforce chunk-size bounds at the gateway
protocol parsing layer (NFS/S3 message size limits).

**Status**: OPEN — non-blocking.

---

## Summary: 0 blocking (1 Medium + 1 Low, both deferred).
