# Pipeline Wiring Adversarial Review (Composition → Log → View)

**Date**: 2026-04-20. **Reviewer**: Adversary role.

## CRITICAL (1)

### PIPE-ADV-1: Delta emission is fire-and-forget — silent data loss
- **Location**: `crates/kiseki-composition/src/log_bridge.rs:41`
- **Issue**: `let _ = log.append_delta(req)` silently discards errors. No retry, no missing-delta detection.
- **Impact**: Composition exists in local store but log has no record — I-X3 violated (mutation history not reconstructible)
- **Resolution**: Return error to caller or queue for retry

## HIGH (2)

### PIPE-ADV-2: HLC logical counter always 0
- **Location**: `crates/kiseki-composition/src/log_bridge.rs:52`
- **Issue**: `logical: 0` on every timestamp — same-millisecond mutations get identical HLC
- **Impact**: Causal ordering broken within same millisecond (I-L1 relies on Raft sequence only)
- **Resolution**: Use proper HLC with monotonic logical counter

### PIPE-ADV-3: composition_hash_key uses non-deterministic DefaultHasher
- **Location**: `crates/kiseki-composition/src/composition.rs:233-244`
- **Issue**: `DefaultHasher` is randomly seeded per process — hashed_keys differ across restarts
- **Impact**: Shard routing non-deterministic, compaction merge ordering unstable (I-O5)
- **Resolution**: Use sha256 or fixed-seed hash for deterministic routing

## MEDIUM (1)

### PIPE-ADV-4: Multipart start/parts don't emit deltas
- **Location**: `crates/kiseki-composition/src/composition.rs:230-232`
- **Issue**: Only finalize emits a delta — partial uploads have no log trail
- **Status**: Acceptable for MVP (multipart state is ephemeral by design)

## LOW (1)

### PIPE-ADV-5: Stream processor throughput capped at 10k deltas/sec
- **Location**: `crates/kiseki-view/src/stream_processor.rs:64`
- **Issue**: 1000 deltas per poll × 10 polls/sec = 10k/sec cap
- **Status**: Acceptable — tunable, production would use event-driven
