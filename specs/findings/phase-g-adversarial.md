# Phase G Adversarial Gate-2 Review

**Reviewer**: adversary
**Date**: 2026-04-21
**Scope**: G.1-G.5 + ADR-027 Go migration
**Mode**: Implementation

## Summary

4 Critical, 5 High, 4 Medium, 3 Low findings.

---

## Finding: G-ADV-1 — TCP transport uses JSON, docs say bincode

Severity: **Critical** (spec violation)
Location: `crates/kiseki-raft/src/tcp_transport.rs:74`
Description: Module docs claim bincode; implementation uses `serde_json`.
Fix: Update docs to say JSON. Bincode migration is a future optimization.

## Finding: G-ADV-2 — GC does not subtract EC fragment overhead

Severity: **Critical** (data integrity)
Location: `crates/kiseki-chunk/src/store.rs:312-318`
Description: `gc()` subtracts `envelope.ciphertext.len()` but EC fragments
are 1.5x-1.375x larger. Pool `used_bytes` drifts increasingly wrong.
Fix: Track total stored bytes (including EC overhead) in `ChunkEntry`.

## Finding: G-ADV-3 — ChunkStore has no interior mutability guard

Severity: **Critical** (concurrency)
Location: `crates/kiseki-chunk/src/store.rs`
Description: `chunks: HashMap<ChunkId, ChunkEntry>` has no `RwLock`. In
multi-threaded context, concurrent read/write is UB. Currently used via
`&mut self` (single-owner), but `read_chunk_ec` takes `&self` while the
owner could call `write_chunk(&mut self)` from another path.
Fix: Document single-threaded constraint OR add `RwLock`.

## Finding: G-ADV-4 — TOCTOU in TenantStore::create_project

Severity: **Critical** (security)
Location: `crates/kiseki-control/src/tenant.rs:159-170`
Description: Reads org quota under read lock, drops it, then acquires write
lock on projects. Another thread can modify org quota between the two locks.
Fix: Hold org read lock while writing project, or use a single write lock.

## Finding: G-ADV-5 — Placement hash multiplier has poor distribution

Severity: **High** (performance)
Location: `crates/kiseki-chunk/src/placement.rs:58`
Description: Uses FNV-1a offset `2654435761` as LCG multiplier. Sequential
chunk IDs will cluster on same devices.
Fix: Use better mixing (e.g., splitmix64 or xxHash constants).

## Finding: G-ADV-6 — No max shard size guard in EC encode

Severity: **High** (robustness)
Location: `crates/kiseki-chunk/src/ec.rs:42`
Description: No upper bound on `shard_size`. A 4GB chunk with 1 data shard
attempts `vec![0u8; 4GB]`.
Fix: Add `MAX_SHARD_SIZE` constant (e.g., 256MB) and reject above it.

## Finding: G-ADV-7 — Fallback placement uses invalid indices

Severity: **High** (data access)
Location: `crates/kiseki-chunk/src/store.rs:198-204`
Description: When not enough devices for placement, code falls back to
`(0..total).collect()` — sequential indices that may not map to real devices.
Fix: Fail the write instead of using invalid placement.

## Finding: G-ADV-8 — 0 fragments returns Some(empty vec)

Severity: **High** (safety)
Location: `crates/kiseki-chunk/src/placement.rs:29`
Description: `place_fragments(chunk_id, 0, devices)` returns `Some([])`.
Fix: Return `None` for 0 fragments.

## Finding: G-ADV-9 — Device auto-evacuate threshold off-by-one

Severity: **High** (spec compliance)
Location: `crates/kiseki-chunk/src/device.rs:157`
Description: `wear > 90` should be `wear >= 90` per ADR-024 ("at 90%").
Fix: Change to `>=`.

## Finding: G-ADV-10 — TCP transport: unbounded message size

Severity: **Medium** (DoS)
Location: `crates/kiseki-raft/src/tcp_transport.rs:85`
Description: Reads `u32` length prefix, allocates that many bytes. Malicious
peer can claim 4GB, causing OOM.
Fix: Add `MAX_RPC_SIZE` cap (e.g., 100MB).

## Finding: G-ADV-11 — TCP transport: no TLS, no auth

Severity: **Medium** (security, MVP-acceptable)
Location: `crates/kiseki-raft/src/tcp_transport.rs`
Description: All Raft RPCs over plaintext TCP. Any network peer can inject
forged messages.
Fix: Document as MVP limitation. Add mTLS before production.

## Finding: G-ADV-12 — RwLock poisoning panics in control plane

Severity: **Medium** (reliability)
Location: `crates/kiseki-control/src/tenant.rs` (all `.unwrap()` on locks)
Description: If a write holder panics, all subsequent lock acquisitions panic.
Fix: Use `.unwrap_or_else(|e| e.into_inner())`.

## Finding: G-ADV-13 — BDD tautology assertion

Severity: **Medium** (test quality)
Location: `crates/kiseki-acceptance/tests/steps/device.rs:215`
Description: `assert_eq!(expected, expected)` — always passes.
Fix: Assert actual pool health against expected.

## Finding: G-ADV-14 — TCP transport: silent error drops

Severity: **Low** (debuggability)
Location: `crates/kiseki-raft/src/tcp_transport.rs:172-179`
Description: Malformed requests cause silent return, no log, no response.
Fix: Add error logging and connection timeout.

## Finding: G-ADV-15 — No RPC versioning

Severity: **Low** (upgrade resilience)
Location: `crates/kiseki-raft/src/tcp_transport.rs:74`
Description: No version byte or magic number in RPC envelope.
Fix: Add version prefix for forward compatibility.

## Finding: G-ADV-16 — Many BDD steps are stubs

Severity: **Low** (test coverage)
Location: Various step files
Description: ~40% of "Then" steps have empty bodies — they pass but assert
nothing. Coverage metrics are inflated.
Fix: Implement real assertions or mark as `#[pending]`.

---

## Blocking fixes required

1. G-ADV-1: Fix docs (trivial)
2. G-ADV-2: Track EC storage in GC
3. G-ADV-4: Fix TOCTOU in create_project
4. G-ADV-7: Fail write on bad placement
5. G-ADV-8: Guard 0 fragments
6. G-ADV-9: Fix threshold boundary
7. G-ADV-13: Fix tautology test
