# BDD Test Depth Audit — 2026-04-25

## Executive Summary

**599/599 BDD scenarios pass. Almost none test real behavior.**

The entire acceptance test suite runs against `MemShardStore` (in-memory
HashMap, no Raft, no persistence) and `InMemoryGateway` (no real storage,
no real network). The tests verify that in-memory data structures work
correctly — not that the distributed storage system works.

| Component | Test harness | Real code exercised? |
|-----------|-------------|---------------------|
| Raft consensus | `MemShardStore` | **No** — single-node, immediate commit, no election |
| Persistence (redb) | `MemShardStore` | **No** — HashMap only, no crash recovery |
| Multi-node cluster | `MemShardStore` | **No** — `raft_members` always `[NodeId(1)]` |
| Shard routing | Hardcoded `ShardId::from_u128(1)` | **No** — all writes go to same shard |
| NFS wire protocol | `NfsContext<InMemoryGateway>` | **No** — ops layer only, no socket |
| S3 wire protocol | `InMemoryGateway` | **No** — direct method calls |
| Block device I/O | In-memory ChunkStore | **No** — no `DeviceBackend` exercised |
| Encryption pipeline | `InMemoryGateway` | **Yes** — real AES-GCM via aws-lc-rs |
| Composition log emission | `CompositionStore.with_log()` | **Yes** — real delta append to MemShardStore |
| Shard split logic | `auto_split::execute_split` | **Partial** — real algorithm, fake store |
| Watermark/GC | `MemShardStore` | **Partial** — real watermark logic, fake store |
| Control plane stores | In-memory structs | **Yes** — real logic, expected in-memory |

---

## Step Classification Totals

| File | Total steps | STUB | SHALLOW | MOCK | THOROUGH |
|------|------------|------|---------|------|----------|
| cluster.rs | 46 | 12 | 0 | 34 | 0 |
| raft.rs | 86 | 17 | 7 | 62 | 0 |
| log.rs | ~80 | ~15 | ~20 | ~44 | 1 |
| composition.rs | ~90 | ~25 | ~35 | ~30 | 0 |
| gateway.rs | ~75 | ~41 | ~21 | ~13 | 0 |
| operational.rs | ~120 | ~60 | ~30 | ~20 | ~10 |
| admin.rs | ~85 | ~30 | ~25 | ~25 | ~5 |
| **Total** | **~582** | **~200** | **~138** | **~228** | **~16** |

**34% STUB** (empty body or comment-only)
**24% SHALLOW** (asserts a boolean/flag, often tautological)
**39% MOCK** (exercises real in-memory logic, but no distributed behavior)
**3% THOROUGH** (exercises real code with meaningful assertions)

---

## False-Green Categories

### Category 1: Raft consensus is never exercised (cluster.rs, raft.rs)

Every scenario about leader election, quorum, replication, membership
changes, and failover is hollow. Steps like:

- "node-1 calls raft.initialize()" → checks `ShardState::Healthy` (always true)
- "a new leader is elected" → checks `ShardState::Healthy` (never changed)
- "delta is replicated to majority" → checks `delta_count > 0` (single-node)
- "quorum lost" → sets `last_error` string directly (shard state unchanged)
- "node-2 catches up via log replay" → reads from same single-node store

**Impact**: 132 steps across 2 files. ALL of Raft behavior is untested.

### Category 2: Persistence (redb) is never exercised

Steps mentioning "local redb log", "snapshot transfer", "crash recovery",
"redb contains entries" are empty or check in-memory flags.

`RaftShardStore` and `PersistentShardStore` exist in `kiseki-log` but
are **never referenced** in acceptance tests (0 grep hits).

**Impact**: Crash recovery, snapshot, and durability guarantees untested.

### Category 3: Multi-shard routing is hardcoded

`World::new()` creates one shard: `ShardId::from_u128(1)`.
`InMemoryGateway::ensure_namespace_exists()` hardcodes `ShardId::from_u128(1)`.
`gateway_write_as()` falls back to `ShardId::from_u128(1)`.

All namespace/shard scenarios trivially pass because everything goes to
the same shard. Cross-shard EXDEV is tested by creating a second shard
with a random UUID, but routing logic is never exercised.

**Impact**: ADR-033 initial topology, shard routing, split/merge untested.

### Category 4: Tautological assertions (always true)

At least 8 steps contain assertions that can never fail:

- `then_ec_repair_attempted`: `is_none() || is_some()` — always true
- `then_validates_ref`: `== 0 || true` — always true
- `then_sre_response_ok`: `pools.len() >= 0` — always true (usize)
- `then_aggregate_only`: `total_capacity >= 0` — always true (u64)
- `then_bounded_cardinality`: `assert!(true)`
- `then_p0_alert_circuit_break`: `assert!(0u64 == 0)`
- `then_advisory_continues`: `count == 0 || count >= 0` — always true
- `then_overflow_warning`: `is_none() || is_some()` — always true

### Category 5: Error injection instead of real failures

Steps for failure scenarios (chunk write failure, delta commit failure,
quorum loss, KMS unreachable) set `w.last_error = Some("message")`
directly rather than triggering real failures through the code path.
Corresponding "then" steps check `last_error.is_some()`.

### Category 6: Audit events all TODO

12+ steps across gateway.rs, admin.rs, operational.rs contain:
```rust
// TODO: wire audit infrastructure
```

No audit event emission is tested anywhere.

### Category 7: Refcounts never verified

Steps claiming "chunk refcount is N" check `last_error.is_none()`
instead of querying actual refcount from ChunkStore. The
`given_chunk_written_with_refcount` step sets `last_chunk_id` but
never writes to `chunk_store`, so refcount queries return 0.

---

## What This Means for ADR-033/034/035 Implementation

The implementer cannot trust any existing green test to verify real
behavior. Before implementing cluster topology changes:

### Tests that must go RED first

1. **Namespace creation → multi-shard**: change World to create
   namespaces with >1 shard. Current `ShardId::from_u128(1)` hardcoding
   must break when routing is implemented.

2. **AppendDelta range validation**: add `KeyOutOfRange` rejection.
   Current tests append to any shard without range checks — those tests
   must fail when range validation is added.

3. **Shard map persistence**: current `NamespaceStore` is in-memory.
   Tests that create namespaces and restart must fail until persistence
   is wired.

4. **Leader placement**: current `auto_split.rs:107` inherits leader
   from old shard. Split tests must fail when I-L12 placement is
   enforced.

5. **Node drain**: current tests have no node state. Drain scenarios
   must fail when `DrainNode` actually transfers leadership and
   replaces voters.

### Recommended test infrastructure changes

| Current | Target | Reason |
|---------|--------|--------|
| `MemShardStore` for all tests | `PersistentShardStore` or `RaftShardStore` for integration scenarios | Exercise real persistence and/or consensus |
| `InMemoryGateway` for all tests | Real gateway with `PersistentShardStore` backend for integration | Exercise real routing |
| `ShardId::from_u128(1)` hardcoded | Dynamic shard creation via `NamespaceShardMap` | Exercise real routing |
| `last_error = Some("...")` for failures | Actual failure injection (e.g., shard maintenance mode, network partition simulation) | Exercise real failure paths |
| `// TODO: wire audit infrastructure` | At minimum, verify audit log `tip()` advances | Exercise real audit emission |

### Implementation sequence for red-green cycle

1. **Wire `PersistentShardStore`** into World as an option (feature flag
   or test mode). Existing MemShardStore tests keep passing. New tests
   use persistent store.

2. **Add range validation** to `AppendDelta`. Existing tests that don't
   set ranges will fail → fix them to use valid ranges.

3. **Replace hardcoded shard** in gateway with `NamespaceShardMap`
   routing. All gateway tests that assumed `from_u128(1)` will fail →
   fix them to create proper namespace mappings.

4. **Wire multi-shard namespace creation** in control plane tests.
   Cluster-formation scenarios go from green-but-fake to red-then-green.

5. **Add node state machine** to World. Drain/eviction scenarios go from
   green-but-stub to red-then-green.

---

## Files audited

- `crates/kiseki-acceptance/tests/acceptance.rs` (World struct)
- `crates/kiseki-acceptance/tests/steps/cluster.rs`
- `crates/kiseki-acceptance/tests/steps/raft.rs`
- `crates/kiseki-acceptance/tests/steps/log.rs`
- `crates/kiseki-acceptance/tests/steps/composition.rs`
- `crates/kiseki-acceptance/tests/steps/gateway.rs`
- `crates/kiseki-acceptance/tests/steps/operational.rs`
- `crates/kiseki-acceptance/tests/steps/admin.rs`
