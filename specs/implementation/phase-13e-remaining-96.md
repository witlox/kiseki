# Phase 13e: Execution Plan — Remaining 96 @integration Scenarios

**Source**: ADR-037 (test infrastructure), adversary findings accepted
**Current state**: 142/241 passing, 96 failing, 3 skipped

## Execution waves

Each wave is independently committable. Waves 1-3 can run in parallel
via agents (different step files). Waves 4-7 are sequential.

### Wave 1: Tier 2 wiring — admin.rs (~8 scenarios)

**Touches**: `steps/admin.rs`, `kiseki-control/src/storage_admin.rs`
**No new traits needed.**

| Scenario | Wire to |
|---|---|
| Add devices to pool | `StorageAdminService::add_device()` |
| RemoveDevice blocked if data | `StorageAdminService::remove_device()` guard |
| RemoveDevice succeeds after evacuation | Same, after evacuation check |
| Rebalance progress observable | `start_rebalance()` + progress query |
| Rebalance cancellable (×2) | `cancel_rebalance()` |
| ReencodePool migrates EC | `reencode_pool()` |
| SRE incident scrub | `require_sre()` + scrub |
| Shard health shows replication | `shard_health()` fields |
| Shard health detects degraded | Same with Merging/Retiring state |
| SplitShard rejected if in progress | `is_busy()` guard |
| Per-device stats reveal load skew | Device stats query |
| Streaming events bounded buffer | Bounded channel verification |

### Wave 2: Tier 2 wiring — gateway.rs (~5 scenarios)

**Touches**: `steps/gateway.rs`

| Scenario | Wire to |
|---|---|
| S3 conditional write (If-None-Match) | `gateway.write()` with conflict check |
| NFS byte-range locking | `LockState::add_lock()` |
| NFS state lost after crash | GatewayLifecycleOps (Wave 5) |
| Gateway crash — client reconnects | GatewayLifecycleOps (Wave 5) |

### Wave 3: Tier 2 wiring — other step files (~7 scenarios)

**Touches**: `steps/operational.rs`, `steps/composition.rs`, `steps/log.rs`

| Scenario | Wire to | File |
|---|---|---|
| Audit event emission (×2) | `AuditLog.append()` | operational.rs |
| Force-init audit log | `AuditLog.append()` | block.rs |
| Compliance tag inheritance | `effective_compliance_tags()` | composition.rs |
| Split during compaction | State guard + concurrent op check | log.rs |
| Merge delta readable from merged | Complete merge test | log.rs |
| Inline data below threshold | InlineStore wiring | log.rs |

### Wave 4: ChunkAvailabilityOps (3 scenarios)

**New file**: `kiseki-chunk/src/availability.rs`
**Touches**: `steps/gateway.rs`, `steps/chunk.rs`

1. TDD: implement `ChunkAvailabilityOps` trait + `ChunkStore` impl
   - `inject_device_failure()` with HashMap<String, DeviceFailureMode>
   - `trigger_repair()` verifies EC parity
   - `trigger_pool_scrub()` iterates chunks
2. BDD: wire step definitions
   - "Gateway cannot reach Chunk Storage" → inject failure → verify read error
   - "Admin-triggered chunk repair" → trigger repair → verify result
   - "Device failure triggers chunk repair" → inject → auto-repair

### Wave 5: GatewayLifecycleOps (3 scenarios)

**New file**: `kiseki-gateway/src/lifecycle.rs`
**Touches**: `steps/gateway.rs`, `mem_gateway.rs`

1. TDD: implement on `InMemoryGateway`
   - `crash()`: clear composition namespace cache, lock table, multipart
     state, set alive=false
   - `restart()`: re-init ephemeral state from durable log_store/chunk_store
   - `is_alive()`, `active_sessions()`
2. BDD: wire step definitions
   - "Gateway crash" → `gateway.crash()`
   - "Client reconnects" → `gateway.restart()` + verify writes work
   - "NFS state lost" → verify locks/sessions cleared after crash

### Wave 6: DeviceHealthOps (5 scenarios)

**New file**: `kiseki-control/src/device_health.rs`
**Touches**: `steps/admin.rs`

1. TDD: implement trait with `tokio::sync::mpsc` bounded channels
   - `DeviceHealthEvent`, `DeviceIOStats` structs
   - `subscribe_health()` → bounded receiver (cap 10,000)
   - `subscribe_device_io_stats()` → periodic stats
   - Stream overflow warning when buffer full
2. BDD: wire subscription steps in admin.rs

### Wave 7: Raft test harness (41 scenarios)

**New files**:
- `kiseki-raft/src/mem_transport.rs` — generic in-memory transport
- `kiseki-log/src/raft/test_cluster.rs` — RaftTestCluster

**Touches**: `steps/raft.rs`, `steps/cluster.rs`, `acceptance.rs`

1. TDD: InMemoryRouter + InMemoryNetwork + InMemoryNetworkFactory
   - Channel-based dispatch (mpsc::unbounded)
   - Blocked link set for partition simulation
   - Fast config (50ms heartbeat, 150-300ms election)
2. TDD: RaftTestCluster
   - Bootstrap N nodes with seed initialization
   - write_delta / read_deltas_from through leader
   - isolate_node / restore_node (symmetric, ADV-037-1)
   - block_link / unblock_link (asymmetric)
   - add_node / change_membership / add_learner
   - trigger_election / trigger_snapshot
   - wait_for_leader with timeout
3. BDD: Replace 41 `todo!()` steps in raft.rs + cluster.rs
   - Background: `RaftTestCluster::new(3, shard_id, tenant_id)`
   - Replication: write through leader, wait for majority commit
   - Election: isolate leader, wait_for_leader on remaining
   - Quorum: isolate 2/3, verify write fails, restore, verify resumes
   - Membership: add_learner → change_membership → verify
   - Drain: higher-level orchestration using cluster + control plane

### Wave 8: AdvisoryOps (10 scenarios)

**New file**: `kiseki-advisory/src/ops.rs`
**Touches**: `steps/gateway.rs`, `steps/operational.rs`, `mem_gateway.rs`

1. TDD: implement trait
   - `submit_hint()` → integrates with BudgetEnforcer
   - `subscribe_backpressure()` → bounded mpsc channel
   - `query_qos_headroom()` → bucketed QoS response
   - `set_healthy(bool)` → failure injection (ADV-037-4)
2. Wire into InMemoryGateway: `advisory: Option<Arc<dyn AdvisoryOps>>`
3. BDD: wire step definitions
   - Workflow hints from S3/NFS headers
   - Backpressure telemetry subscription
   - QoS headroom query
   - Advisory outage → data path continues (I-WA1)

## Parallelization strategy

```
Wave 1 (admin.rs)  ──┐
Wave 2 (gateway.rs) ──┼── parallel agents, different step files
Wave 3 (other)     ──┘
         │
         ▼
Wave 4 (ChunkAvailability)  ── sequential, new crate code
         │
         ▼
Wave 5 (GatewayLifecycle)   ── sequential, touches gateway
         │
         ▼
Wave 6 (DeviceHealth)       ── sequential, new crate code
         │
         ▼
Wave 7 (Raft harness)       ── sequential, largest piece
         │
         ▼
Wave 8 (AdvisoryOps)        ── sequential, cross-cutting
```

## Verification per wave

After each wave:
1. `cargo check --workspace`
2. `cargo test --workspace --exclude kiseki-acceptance`
3. `cargo test -p kiseki-acceptance --test acceptance` — count passing
4. `cargo clippy --workspace --all-targets -- -D warnings`

## Expected progression

| After wave | Passing | Delta |
|---|---|---|
| Current | 142 | — |
| Wave 1-3 | ~162 | +20 |
| Wave 4 | ~165 | +3 |
| Wave 5 | ~168 | +3 |
| Wave 6 | ~173 | +5 |
| Wave 7 | ~214 | +41 |
| Wave 8 | ~224 | +10 |
| Remaining | ~241 | scattered fixes |
