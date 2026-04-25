# ADR-036: Add Shard Management Methods to LogOps Trait

**Status**: Accepted
**Date**: 2026-04-25
**Deciders**: Architect
**Context**: ADR-033 (shard topology), ADR-034 (merge), persistence testing

## Problem

`LogOps` currently covers only the data path (append, read, health,
maintenance, truncate, compact). Shard lifecycle operations — creating
shards, updating key ranges, transitioning shard state — are defined
only on `MemShardStore` as concrete methods.

This prevents:
1. Using `PersistentShardStore` or `RaftShardStore` through the `LogOps`
   trait for integration tests that need shard management
2. The control plane from managing shards through a trait boundary
   (namespace creation, split, merge all need `create_shard`)
3. `api-contracts.md` lists `SplitShard` and `MergeShard` as Log context
   commands, but they have no trait-level entry point

## Decision

Add three methods to the `LogOps` trait:

```rust
/// Create a new shard with the given parameters.
fn create_shard(
    &self,
    shard_id: ShardId,
    tenant_id: OrgId,
    node_id: NodeId,
    config: ShardConfig,
);

/// Update a shard's key range (used during split/merge).
fn update_shard_range(
    &self,
    shard_id: ShardId,
    range_start: [u8; 32],
    range_end: [u8; 32],
);

/// Transition a shard's lifecycle state (ADR-034).
fn set_shard_state(&self, shard_id: ShardId, state: ShardState);
```

These are synchronous (not async) because they operate on local state
only — no Raft consensus needed for shard metadata mutations (those
go through the control plane Raft group, not the shard's Raft group).

### What stays off the trait

- `set_shard_config()` — test-only helper for lowering split thresholds
- `should_split()` — internal to split evaluator
- `split_shard()` — orchestrator-level, uses multiple trait methods
- `register_consumer()`, `advance_watermark()` — consumer watermark management

## Consequences

### Positive
- World can use `Arc<dyn LogOps + Send + Sync>` for all log store backends
- PersistentShardStore usable in integration tests without MemShardStore downcasting
- Control plane can manage shards through the trait boundary
- Consistent with api-contracts.md listing shard operations as Log commands

### Negative
- LogOps trait surface grows from 6 to 9 methods
- All three implementations must implement the new methods (trivial —
  MemShardStore already has them, PersistentShardStore delegates to mem,
  RaftShardStore delegates to its state machine)

### Implementation

1. Add methods to `LogOps` trait in `traits.rs`
2. Implementations already exist — just move to trait impl blocks
3. Change World `log_store` type: `Arc<MemShardStore>` → `Arc<dyn LogOps + Send + Sync>`
4. All callers that used `log_store.create_shard()` etc. continue to work
