# ADR-036: LogOps Trait — Shard Management and Consumer Watermarks

**Status**: Accepted (revised)
**Date**: 2026-04-25
**Deciders**: Architect
**Adversarial review**: 2026-04-25 (1M, 2L findings, all resolved)
**Context**: ADR-033 (shard topology), ADR-034 (merge), persistence testing, I-L4 (GC watermarks)

## Problem

`LogOps` covered only the data path (append, read, health, maintenance,
truncate, compact — 6 methods). Shard lifecycle operations and consumer
watermark tracking were concrete methods on `MemShardStore` only.

This prevented:
1. Using `PersistentShardStore` through the trait for integration tests
2. Control plane managing shards through a trait boundary
3. Stream processors registering consumers through a trait boundary

## Decision

Widen `LogOps` from 6 to 11 methods in two groups:

### Group 1: Shard lifecycle (sync, 3 methods)

```rust
fn create_shard(&self, shard_id, tenant_id, node_id, config);
fn update_shard_range(&self, shard_id, range_start, range_end);
fn set_shard_state(&self, shard_id, state);
```

Synchronous — shard metadata is local state (control plane Raft handles
distributed coordination separately).

### Group 2: Consumer watermarks (async, 2 methods)

```rust
async fn register_consumer(&self, shard_id, consumer, position) -> Result<(), LogError>;
async fn advance_watermark(&self, shard_id, consumer, position) -> Result<(), LogError>;
```

Async because on a Raft-backed store, watermark advancement is a state
machine mutation through consensus (ADV-036-1). Consumer progress is
durable state that survives restarts (I-L4).

**Ordering dependency**: callers advance watermarks BEFORE calling
`truncate_log`. Documented in trait doc (ADV-036-3).

### What stays off the trait

- `set_shard_config()` — test-only helper for lowering split thresholds
- `should_split()` — pure function on `ShardInfo`, already in `auto_split::check_split`
- `split_shard()` — **deprecated** in favor of `auto_split::execute_split(&dyn LogOps, &SplitPlan)` (ADV-036-2)

### Orchestrators use trait primitives

Split and merge orchestrators compose trait methods:
- `auto_split::execute_split` takes `&dyn LogOps` (refactored from `&MemShardStore`)
- `merge::copy_phase` already takes `&dyn LogOps`

This makes them backend-agnostic without adding orchestration to the trait.

## Consequences

### Positive
- World can use `Arc<dyn LogOps + Send + Sync>` for all backends
- PersistentShardStore usable in integration tests
- Consumer tracking works through trait boundary (I-L4)
- Split/merge orchestrators are backend-agnostic

### Negative
- Trait surface grows from 6 to 11 methods
- All implementations must add 5 new methods (trivial — code exists)
- `MemShardStore::split_shard` deprecated (callers migrate to `execute_split`)

### Implementation

1. Add 5 methods to `LogOps` trait in `traits.rs`
2. Implement in all 4 backends (MemShardStore, PersistentShardStore, RaftLogStore, RaftShardStore)
3. Refactor `auto_split::execute_split` signature: `&MemShardStore` → `&dyn LogOps`
4. Change World `log_store`: `Arc<MemShardStore>` → `Arc<dyn LogOps + Send + Sync>`
5. Deprecate `MemShardStore::split_shard`
