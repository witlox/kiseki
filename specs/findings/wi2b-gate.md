# WI-2b — Adversarial Gate: Raft Key Store

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-keymanager/src/raft_store.rs`

---

## Finding: Key material stored as plaintext in command log

Severity: **High**
Category: Security > Cryptographic correctness
Location: `raft_store.rs:42` — `KeyCommand::CreateEpoch.key_material`

**Description**: The `KeyCommand::CreateEpoch` stores key material as
`Vec<u8>` in the command log. The doc comment says "in production:
encrypted with node-local key" but no encryption is implemented. If
the log is persisted to disk (as it will be with Raft), key material
is written in plaintext.

**Suggested resolution**: Add a `NodeLocalKey` that encrypts key
material before logging and decrypts after reading. The state machine
`apply` method decrypts; the `rotate` method encrypts before
`apply_command`. For the reference impl, a no-op cipher is acceptable
with a clear doc comment.

**Status**: OPEN — blocking for production use, acceptable for
reference impl with documentation.

---

## Finding: State machine and log are separate locks — not atomic

Severity: **Medium**
Category: Correctness > Concurrency
Location: `raft_store.rs:159-173`

**Description**: `apply_command` locks the log, appends, drops the log
lock, then locks the state machine and applies. Between the two locks,
another thread could read stale state. In production Raft, this
doesn't matter (Raft serializes applies), but the reference impl
should note this.

**Status**: OPEN — non-blocking (acceptable for single-leader Raft
semantics; noted in docs).

---

## Summary: 1 High (documented acceptable for ref impl), 1 Medium.
No blocking findings for the reference implementation.
