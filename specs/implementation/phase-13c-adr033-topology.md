# Phase 13c: ADR-033 Shard Topology Integration (completed 2026-04-25)

## Goal

Wire shard topology (ADR-033) into the real integrated code paths so
@integration BDD scenarios exercise gateway→composition→log chain.

## What was built

### Production code (TDD)
- `kiseki-control/src/shard_topology.rs`: ShardRange, NamespaceShardMap,
  ShardTopologyConfig, compute_initial_shards(), compute_shard_ranges(),
  route_to_shard(), check_ratio_floor(), NamespaceShardMapStore
- `kiseki-gateway/src/mem_gateway.rs`: shard_map field (RwLock), routing
  in write(), clear/set_shard_map() for stale cache simulation
- `kiseki-gateway/src/error.rs`: GatewayError::KeyOutOfRange
- `kiseki-composition/src/log_bridge.rs`: emit_delta returns Result
- `kiseki-log/src/shard.rs`: Merging/Retiring states, ShardBusy error
- `kiseki-log/src/merge.rs`: merge orchestrator (prepare, copy, abort)
- `kiseki-log/src/store.rs`: set_shard_state(), set_shard_config()

### BDD scenarios (19 green, THOROUGH depth)
- 12 ADR-033 topology scenarios in cluster-formation.feature
- 7 ADR-034 merge/split scenarios in log.feature

## Integration path verified

```
gateway.write()
  → comps.create(ns_id) → shard_id from namespace
  → composition_hash_key(ns_id, comp_id) → hashed_key
  → shard_map_store.route(ns_id, hashed_key) → correct shard_id
  → log_bridge::emit_delta(shard_id, ...) → Result<SequenceNumber, LogError>
  → log_store.append_delta() → validates range → KeyOutOfRange if wrong
  → gateway returns GatewayError::KeyOutOfRange
```

## Lesson learned

First attempt wired topology at MOCK depth (calling domain functions
directly in step definitions, bypassing gateway). Caught and reverted.
Second attempt wires through real integrated path. Rule: @integration
steps call gateway.write() or equivalent, errors flow from real operations.
