# Phase 0 — Adversarial Gate-2 Findings

**Reviewer**: adversary role.
**Date**: 2026-04-19.
**Scope**: `kiseki-common`, `kiseki-proto` — all source, tests, and
build configuration committed at `726351c`.

---

## Finding: HLC monotonicity violation at saturation boundary

Severity: **Critical**
Category: Correctness > Specification compliance
Location: `crates/kiseki-common/src/time.rs:50-59` (`tick`), lines 82-114 (`merge`)
Spec reference: I-T5 ("HLC is authoritative for ordering and causality"), I-T7

**Description**: When `physical_ms = u64::MAX` and `logical = u32::MAX`,
both `tick()` and `merge()` use `saturating_add(1)` on the physical
component, which stays at `u64::MAX`, then reset `logical` to 0. The
resulting clock `(u64::MAX, 0, node)` is *strictly less* than the input
`(u64::MAX, u32::MAX, node)` in the induced total order.

This violates the documented contract: "tick never produces a clock ≤
its input" and "merge returns a clock strictly greater than both
inputs."

**Evidence**: Standalone reproduction:
```
Input:  (phys=u64::MAX, logical=u32::MAX)
Output: (phys=u64::MAX, logical=0)
Monotonic: false
```

The proptest suite does not catch this because the probability of
sampling `(u64::MAX, u32::MAX)` simultaneously from uniform
distributions is ~2^-96.

**Practical risk**: Near-zero. `u64::MAX` ms since Unix epoch is year
~584,942,417 CE. However, the property test *claims* strict
monotonicity holds for all inputs, which is false.

**Suggested resolution**: Return `Result<Self, HlcExhausted>` when both
components are saturated. Since `panic` and `unwrap` are denied by
lint, this is the only clean option. Add a deterministic boundary test.

**Status**: RESOLVED — `tick`/`merge` now return `Result<Self, HlcExhausted>`. Deterministic boundary tests added.

---

## Finding: Proptest does not cover saturation boundaries

Severity: **High**
Category: Correctness > Missing negatives
Location: `crates/kiseki-common/tests/hlc_properties.rs`
Spec reference: I-T5

**Description**: The HLC property tests rely on uniform random sampling
across `u64` and `u32` ranges. Saturation boundaries
(`u64::MAX`, `u32::MAX`, `0`) are never reliably exercised. This masks
the monotonicity bug above and any future boundary issues.

**Evidence**: 6 proptest cases, all pass, none cover the saturated corner.

**Suggested resolution**: Add explicit boundary tests for
`(u64::MAX, u32::MAX)`, `(u64::MAX, 0)`, `(0, u32::MAX)`, and `(0, 0)`.
These should be deterministic `#[test]` functions, not proptest strategies.

**Status**: RESOLVED — 7 deterministic boundary tests added covering saturation, near-saturation, and zero corners.

---

## Finding: Proto ChunkId allows variable-length bytes

Severity: **Medium**
Category: Security > Input validation
Location: `specs/architecture/proto/kiseki/v1/common.proto` — `ChunkId`
Spec reference: I-K10 (chunk ID is sha256 or HMAC-SHA256, both 32 bytes)

**Description**: The proto `ChunkId` message uses `bytes value = 1`
with no length constraint. The domain type `ids::ChunkId` enforces
`[u8; 32]`. The roundtrip test uses a 4-byte `ChunkId` without
flagging the length mismatch. Any future conversion layer between proto
and domain types must validate length, but no compile-time or
test-time check enforces this today.

**Evidence**: `roundtrip.rs:61-66` constructs `ChunkId { value: vec![0x00, 0x11, 0x22, 0x33] }` — 4 bytes, not 32.

**Suggested resolution**: Add a conversion function
`kiseki_proto::v1::ChunkId → kiseki_common::ChunkId` with a
`TryFrom` that rejects non-32-byte values. Update the roundtrip test
to use 32-byte values. (Full conversion layer is Phase 1+ work, but
the test should not use invalid lengths.)

**Status**: OPEN — non-blocking (no conversion code exists yet, but
the test is misleading).

---

## Finding: Proto nonce/auth_tag fields lack length validation

Severity: **Medium**
Category: Security > Input validation
Location: `specs/architecture/proto/kiseki/v1/common.proto` — `Envelope`, `DeltaPayload`
Spec reference: I-K7 (authenticated encryption everywhere)

**Description**: AES-256-GCM requires a 12-byte nonce and a 16-byte
authentication tag. The proto definitions accept arbitrary-length
`bytes` for both `nonce` and `auth_tag`. The roundtrip test uses
correct lengths but nothing enforces them.

**Evidence**: Proto fields `bytes nonce`, `bytes auth_tag` with no
constraints. A malformed envelope with a 0-byte nonce would
deserialize without error.

**Suggested resolution**: Document expected lengths in proto comments.
Enforce in the conversion layer (`TryFrom<proto::Envelope> for
domain::Envelope`). The conversion layer is Phase 1 work; for Phase 0,
add proto comments and use correct lengths in tests.

**Status**: OPEN — non-blocking (enforcement is Phase 1 scope).

---

## Finding: Unbounded String fields in domain types

Severity: **Low**
Category: Robustness > Resource exhaustion
Location: `crates/kiseki-common/src/tenancy.rs:52` (`ComplianceTag::Custom`),
  `crates/kiseki-common/src/time.rs:146` (`WallTime::timezone`),
  `crates/kiseki-common/src/advisory.rs:101` (`PoolDescriptor::opaque_label`)
Spec reference: none — missing spec

**Description**: `ComplianceTag::Custom(String)`,
`WallTime::timezone: String`, and `PoolDescriptor::opaque_label: String`
accept unbounded strings. If constructed from untrusted proto input, a
multi-GB string could cause OOM.

**Practical risk**: Low for Phase 0 — these types are not yet
constructed from wire data. All boundaries (gRPC, proto) impose their
own message-size limits.

**Suggested resolution**: Add bounded-string newtypes or validation at
the proto-to-domain conversion boundary (Phases 1+). No Phase 0 change
needed.

**Status**: OPEN — non-blocking (deferred to conversion layer).

---

## Finding: `zeroize` dependency unused

Severity: **Low**
Category: Correctness > Implicit coupling
Location: `crates/kiseki-common/Cargo.toml:14`
Spec reference: I-K8

**Description**: `zeroize` is listed as a dependency of
`kiseki-common` but no type in the crate derives `Zeroize` or uses
`Zeroizing<T>`. The `KeyEpoch` type (which carries epoch numbers, not
key material) does not need it, and actual key-bearing types will live
in `kiseki-crypto` (Phase 1).

**Suggested resolution**: Remove the `zeroize` dependency from
`kiseki-common` unless a concrete Phase 0 type needs it. Re-add in
Phase 1 for `kiseki-crypto`.

**Status**: RESOLVED — `zeroize` removed from `kiseki-common/Cargo.toml`. Will be added to `kiseki-crypto` in Phase 1.

---

## Finding: `SequenceNumber` has no overflow-safe arithmetic

Severity: **Low**
Category: Correctness > Edge cases
Location: `crates/kiseki-common/src/ids.rs:92-95`
Spec reference: I-L1 (gap-free, monotonic)

**Description**: `SequenceNumber(u64)` has `Ord` and `PartialOrd` but
no `checked_next()` or `saturating_increment()` method. Downstream
code will need to increment sequence numbers; without a safe API, each
caller must remember to use checked arithmetic. The lint
`clippy::unwrap_used = "deny"` protects against `.unwrap()` but not
against silent wrapping (which is not default in release mode due to
`overflow-checks = true` in `Cargo.toml`).

**Practical risk**: Near-zero — `u64` sequence space is ~1.8×10^19.
Overflow checks are enabled in all profiles. But a convenience method
would centralize the semantics.

**Suggested resolution**: Add `SequenceNumber::checked_next() ->
Option<Self>` for downstream use.

**Status**: RESOLVED — `SequenceNumber::checked_next()` added.

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| Critical | 1 | Yes |
| High | 1 | Yes |
| Medium | 2 | No |
| Low | 3 | No |

**Blocking items**: The HLC monotonicity violation (Critical) and the
test gap that masks it (High) must be resolved before Phase 0 can close.

**Recommendation**: Fix the HLC saturation handling, add boundary
tests, then re-verify. The Medium and Low findings can be tracked and
resolved in Phase 1 when the proto-to-domain conversion layer is built.
