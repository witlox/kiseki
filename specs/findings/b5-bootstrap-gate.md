# B.5 — Adversarial Gate: Single-Node Raft Bootstrap

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-keymanager/src/raft/openraft_store.rs`, integration test.

---

## Finding: End-to-end Raft path proven

Severity: **Positive finding**

`OpenRaftKeyStore::new()` → `Raft::new()` → `initialize()` →
`client_write(CreateEpoch)` → state machine `apply()` → read from
shared state. 7 integration tests exercise bootstrap, rotation,
multi-epoch access, HKDF interop, migration marking, and health.
All writes go through Raft consensus (single-node quorum = self).

---

## Finding: Error handling discards Raft error details

Severity: **Low**
Category: Robustness

`OpenRaftKeyStore` maps all Raft errors to `KeyManagerError::Unavailable`,
discarding the underlying `RaftError` details. Fine for the reference
impl but production should log/surface the specific error.

**Status**: OPEN — non-blocking.

---

## Finding: No shutdown handling

Severity: **Low**
Category: Robustness

`OpenRaftKeyStore` doesn't call `raft.shutdown()` on drop. The Raft
core task leaks if the store is dropped without explicit shutdown.
For single-node in-memory, this is harmless.

**Status**: OPEN — non-blocking.

---

## Summary: 0 blocking. Stage B complete.
