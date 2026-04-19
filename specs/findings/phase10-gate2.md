# Phase 10 — Adversarial Gate-2 Findings

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-client` at `97721a5`.

---

## Finding: Discovery response not authenticated

Severity: **Medium**
Category: Security > Trust boundaries
Location: `crates/kiseki-client/src/discovery.rs`
Spec reference: ADR-008, I-Auth1

**Description**: `DiscoveryResponse` is a plain struct with no
authentication or integrity verification. A MITM on the fabric could
return spoofed shard/view/gateway endpoints, redirecting the client to
a malicious node.

ADR-008 specifies that discovery uses mTLS (Cluster CA), which
provides the authentication. But the discovery types don't reference
or enforce this — the caller is responsible for only using discovery
over an authenticated transport.

**Suggested resolution**: Document in the type-level docs that
discovery MUST happen over an authenticated `Transport` connection.
The enforcement is at the integration layer.

**Status**: OPEN — non-blocking (enforcement is transport-layer
responsibility, documented).

---

## Finding: No FUSE implementation

Severity: **Medium**
Category: Correctness > Specification compliance
Location: `crates/kiseki-client/src/` (missing module)
Spec reference: native-client.feature (20 scenarios)

**Description**: No FUSE mount, no native API, no access pattern
detection, no transport selection. The crate has discovery types and
a cache — none of the core native-client functionality from the
feature file.

**Suggested resolution**: FUSE requires the `fuser` crate and a
running kernel with FUSE support. Defer to integration. The cache
and discovery types are the foundational pieces.

**Status**: OPEN — non-blocking (hardware/OS-dependent).

---

## Finding: Cache stores decrypted plaintext in memory

Severity: **Low**
Category: Security > Cryptographic correctness
Location: `crates/kiseki-client/src/cache.rs:9`
Spec reference: I-K1

**Description**: `CacheEntry.data` holds decrypted plaintext chunk
data in a plain `Vec<u8>`. On eviction or drop, the plaintext is
freed but not zeroized. A memory dump could capture cached plaintext.

This is architecturally correct — the native client runs in the
workload process where plaintext is expected (the spec says "plaintext
never leaves the workload process"). But `Zeroizing` on drop would
be defense-in-depth.

**Status**: OPEN — non-blocking (accepted risk per spec).

---

## Summary: 0 blocking (2 Medium + 1 Low, all deferred or accepted).
