# Phase 1 — Adversarial Gate-2 Findings

**Reviewer**: adversary role.
**Date**: 2026-04-19.
**Scope**: `kiseki-crypto` — all source, tests, and build config at `aec17f9`.

---

## Finding: No mlock on key material memory pages

Severity: **High**
Category: Security > Cryptographic correctness
Location: `crates/kiseki-crypto/src/keys.rs` (entire file)
Spec reference: I-K8, `.claude/coding/rust.md` §"FIPS Crypto"

**Description**: The coding standard mandates "Key material: wrapped in
`zeroize::Zeroizing<T>`, mlock'd pages." `Zeroizing<T>` is used
correctly, but no `mlock` / `MADV_DONTDUMP` protection is applied to
the pages holding `SystemMasterKey` or `TenantKek` material. Without
`mlock`, key material can be swapped to disk; without `MADV_DONTDUMP`,
it can appear in core dumps.

The `lib.rs` already has `#![allow(unsafe_code)]` and the coding
standard lists `kiseki-crypto` as allowed for `mlock/madvise` unsafe.

**Evidence**: `grep -r mlock crates/kiseki-crypto/` returns no hits.

**Suggested resolution**: Add a `mem_protect` module that calls
`libc::mlock` on key material pages and `libc::madvise(MADV_DONTDUMP)`.
Handle `RLIMIT_MEMLOCK` exhaustion gracefully (warn + continue, do
not fail). Apply at `SystemMasterKey::new` and `TenantKek::new`.

**Status**: RESOLVED — `mlock`/`munlock` + `MADV_DONTDUMP` applied in `mem_protect.rs`. Key types call mlock at construction, munlock at drop. Non-fatal on `RLIMIT_MEMLOCK` exhaustion.

---

## Finding: Decompression bomb — unbounded output size

Severity: **High**
Category: Robustness > Resource exhaustion
Location: `crates/kiseki-crypto/src/compress.rs:66-70`
Spec reference: none — missing spec for decompression limits

**Description**: `decrypt_and_decompress` calls
`decoder.read_to_end(&mut plaintext)` with no size limit. A
maliciously crafted compressed payload (e.g., a deflate bomb) could
decompress to gigabytes, causing OOM. In production, chunks have
bounded size, but the crypto library itself applies no bound.

**Evidence**: No `take()` wrapper or size limit on the `DeflateDecoder`.

**Suggested resolution**: Accept a `max_plaintext_size: usize` parameter
and wrap the decoder with `.take(max_plaintext_size as u64 + 1)`,
returning an error if the limit is exceeded.

**Status**: RESOLVED — `decrypt_and_decompress` now takes `max_plaintext_size` parameter with `.take()` bound. Test added for size limit enforcement.

---

## Finding: Padding overflow silently disables side-channel protection

Severity: **Medium**
Category: Security > Cryptographic correctness
Location: `crates/kiseki-crypto/src/compress.rs:47-51`
Spec reference: I-K14

**Description**: `checked_next_multiple_of(pad_alignment)` can return
`None` if the padded length would overflow `usize`. The fallback
`.unwrap_or(compressed.len())` sends the data through unpadded,
silently defeating the CRIME/BREACH mitigation that padding exists to
provide. A compressed payload near `usize::MAX` would lose padding
protection with no error.

**Practical risk**: Near-zero (chunks are bounded far below `usize::MAX`),
but a silent security downgrade is unacceptable in a crypto library.

**Suggested resolution**: Return `CryptoError::CompressionFailed` when
`checked_next_multiple_of` returns `None`.

**Status**: RESOLVED — returns `CryptoError::CompressionFailed` on overflow instead of silently skipping.

---

## Finding: Key material stack copies not zeroized

Severity: **Medium**
Category: Security > Cryptographic correctness
Location: `crates/kiseki-crypto/src/keys.rs:22`, `envelope.rs:108`
Spec reference: I-K8

**Description**: `SystemMasterKey::new(material: [u8; 32], ...)` takes
key material by value. The caller's stack copy is "moved" but Rust does
not guarantee zeroing of the source location. Similarly,
`Zeroizing::new(*tenant_kek.material())` at `envelope.rs:108` creates
a temporary `[u8; 32]` on the stack via dereference that is not zeroized
on drop.

This is an inherent limitation of Rust's `zeroize` approach — the
library can only zero the `Zeroizing<T>` wrapper itself, not prior
stack copies.

**Practical risk**: Low in release builds (optimizer may reuse the stack
slot). Higher in debug builds where stack frames are not reused.

**Suggested resolution**: Document as a known limitation. For
defense-in-depth, accept `&[u8; 32]` instead of `[u8; 32]` in `new()`
to avoid the stack copy. The caller is then responsible for the source.

**Status**: OPEN — non-blocking (inherent Rust limitation).

---

## Finding: Unwrapped chunk_id not verified against envelope

Severity: **Low**
Category: Correctness > Implicit coupling
Location: `crates/kiseki-crypto/src/envelope.rs:150-160`
Spec reference: ADR-003

**Description**: `unwrap_tenant` extracts `(epoch, chunk_id)` from the
tenant-wrapped material but only uses the `epoch` — the `chunk_id`
from the unwrapped material is ignored. The function then calls
`open_envelope` which derives the DEK from `envelope.chunk_id`.

If an attacker substitutes `tenant_wrapped_material` from a different
envelope (same tenant KEK, different chunk_id), the epoch mismatch or
DEK mismatch would be caught by the AEAD authentication tag. So this
is not exploitable. However, an explicit check would provide
defense-in-depth and a clearer error message.

**Suggested resolution**: After unwrapping, verify that the unwrapped
`chunk_id` matches `envelope.chunk_id`. Return
`CryptoError::InvalidEnvelope` on mismatch.

**Status**: RESOLVED — unwrapped `chunk_id` now verified against `envelope.chunk_id` with explicit error on mismatch.

---

## Finding: HKDF info string does not include epoch

Severity: **Low**
Category: Security > Cryptographic correctness (defense-in-depth)
Location: `crates/kiseki-crypto/src/hkdf.rs:22`
Spec reference: ADR-003

**Description**: The HKDF info string is `"kiseki-chunk-dek-v1"` and
does not include the system key epoch. The epoch is implicitly encoded
by which master key is selected. If the same master key bytes were
somehow re-used across two different epochs (operational error), the
derived DEKs would be identical, violating epoch isolation.

**Practical risk**: Extremely low — master keys are generated from a
CSPRNG, so byte-for-byte reuse is impossible without deliberate action.

**Suggested resolution**: Include the epoch in the info parameter:
`format!("kiseki-chunk-dek-v1-epoch-{}", epoch)`. This is
defense-in-depth, not a correctness requirement.

**Status**: OPEN — non-blocking.

---

## Finding: Envelope fields are all public

Severity: **Low**
Category: Correctness > Specification compliance
Location: `crates/kiseki-crypto/src/envelope.rs:22-39`
Spec reference: I-K7

**Description**: All `Envelope` fields are `pub`, allowing callers to
construct envelopes directly or mutate fields (e.g., swap `chunk_id`
without re-encrypting). The AEAD authentication tag catches any
post-construction tampering on decrypt, but the type system does not
prevent construction of invalid envelopes.

**Suggested resolution**: Make fields `pub(crate)` and add accessor
methods. Defer to when downstream crates need to construct `Envelope`
from proto or storage formats (Phase 3+).

**Status**: OPEN — non-blocking (deferred to conversion layer).

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| High | 2 | Yes |
| Medium | 2 | No |
| Low | 3 | No |

**Blocking items**: `mlock` key protection and decompression bomb bound
must be resolved before Phase 1 can close.

**Recommendation**: Fix both High findings, then re-verify. The Medium
and Low findings are tracked for resolution in later phases or as
defense-in-depth improvements.
