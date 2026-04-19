# Phase 3 — Adversarial Gate-2 Findings

**Reviewer**: adversary role.
**Date**: 2026-04-19.
**Scope**: `kiseki-log` — all source, tests, and build config at `fdc691f`.

---

## Finding: No Raft integration — single-node only

Severity: **High**
Category: Correctness > Specification compliance
Location: `crates/kiseki-log/src/store.rs` (entire file)
Spec reference: I-L1, I-L2

**Description**: `MemShardStore` assigns sequence numbers locally and
stores deltas in a `Vec`. No Raft consensus means I-L2 (durable on
majority before ack) is not enforced. The commit message explicitly
defers openraft integration.

**Impact**: The Log cannot be used in a multi-node deployment without
Raft. However, the `LogOps` trait contract is correct — the
`MemShardStore` is a valid single-node implementation. Production
requires swapping to a Raft-backed store.

**Suggested resolution**: Add `openraft` integration as a follow-up
before Phase 12 (integration). The `LogOps` trait is the stable
boundary.

**Status**: OPEN — non-blocking for Phase 3 scope (trait is correct;
implementation is single-node reference).

---

## Finding: No compaction implementation

Severity: **Medium**
Category: Correctness > Specification compliance
Location: `crates/kiseki-log/src/traits.rs`
Spec reference: I-L7, log.feature §compaction scenarios

**Description**: The `LogOps` trait does not include `compact_shard`,
and no compaction logic exists. Two log.feature scenarios cover
compaction (automatic and admin-triggered). The spec data model
includes `CompactShardRequest` in the `LogOps` trait.

**Suggested resolution**: Add `compact_shard` to `LogOps` and
implement header-only merge compaction in `MemShardStore`. Key
semantics: merge by `(hashed_key, sequence)`, newer supersedes older,
tombstones removed when all consumers advanced past them, payloads
carried opaquely (I-L7).

**Status**: RESOLVED — `compact_shard` added to `LogOps` trait. Header-only merge compaction implemented: keeps latest per `hashed_key`, removes old tombstones past watermark, re-sorts by sequence. Two tests added.

---

## Finding: Split midpoint computation is naive

Severity: **Medium**
Category: Correctness > Edge cases
Location: `crates/kiseki-log/src/store.rs:130-134`
Spec reference: I-L6

**Description**: The split midpoint is computed by averaging each byte
of `range_start` and `range_end`. This naive averaging doesn't account
for carry between bytes. For example,
`avg(0x00, 0xFF) = 0x7F` per byte, but the true midpoint of
`[0x0000, 0xFFFF]` is `0x7FFF`, not `0x7F7F`.

**Practical risk**: Split skew — one shard gets more keys than the
other. Not a correctness violation but a performance concern.

**Suggested resolution**: Use proper big-integer midpoint: add the two
32-byte values with carry, then shift right by 1. Or use a simpler
scheme: split at the first byte that differs.

**Status**: OPEN — non-blocking (skewed splits are inefficient but
not incorrect).

---

## Finding: Shard byte_size estimate is rough

Severity: **Low**
Category: Correctness > Implicit coupling
Location: `crates/kiseki-log/src/store.rs:231`
Spec reference: I-L6

**Description**: Byte size is computed as `payload_size + 128` (header
estimate). The actual header size depends on the number of chunk_refs
and the hashed_key. This could cause the byte-size split threshold
to trigger earlier or later than expected.

**Suggested resolution**: Compute actual serialized header size when
a proper serialization format is chosen.

**Status**: OPEN — non-blocking.

---

## Finding: `gc_floor` field tracked but never exposed

Severity: **Low**
Category: Correctness > Implicit coupling
Location: `crates/kiseki-log/src/store.rs:28`

**Description**: `MemShard` tracks a `gc_floor` field that records the
last GC boundary, but it's never exposed through `ShardInfo` or any
public API. Downstream consumers (e.g., stream processors requesting
deltas below the GC floor) would get an empty result set without a
clear indication that the data was GC'd.

**Suggested resolution**: Add `gc_floor` to `ShardInfo` and return
`LogError` when `ReadDeltasRequest.from < gc_floor`.

**Status**: OPEN — non-blocking.

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| High | 1 | No (deferred) |
| Medium | 2 | 1 blocking |
| Low | 2 | No |

**Blocking item**: Missing `compact_shard` in the `LogOps` trait.
The High finding (no Raft) is explicitly deferred and non-blocking
for Phase 3 scope.

**Recommendation**: Add `compact_shard` to the trait and implement
header-only merge compaction, then Phase 3 can close.
