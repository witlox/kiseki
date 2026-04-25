# ADR-037: Test Infrastructure — Raft Harness and Subsystem Trait Boundaries

**Status**: Accepted
**Date**: 2026-04-25
**Deciders**: Architect (diamond workflow: analyst → architect → adversary)
**Context**: 96 remaining @integration scenarios, ADR-026 (Raft), ADR-025 (StorageAdmin)

## Problem

142/241 @integration scenarios pass. The remaining 96 are blocked by:
- 41 need in-process multi-node Raft (no test harness exists)
- 20 need wiring of existing production code (no new design)
- 35 need subsystem interfaces that aren't defined as traits

## Decision

### 1. In-process Raft test harness

Channel-based in-memory transport for openraft. No real TCP sockets.

**Architecture:**
```
RaftTestCluster
 ├── router: Arc<InMemoryRouter>         // message routing + partition sim
 │    ├── senders: RwLock<HashMap<u64, Sender<RaftRpc>>>
 │    └── blocked: RwLock<HashSet<(u64, u64)>>  // directional partition
 ├── nodes: HashMap<u64, RaftTestNode>
 │    ├── raft: Raft<LogTypeConfig, ShardStateMachine>
 │    ├── state: Arc<Mutex<ShardSmInner>>
 │    └── _dispatcher: JoinHandle  // receives RPCs, dispatches to raft
 └── config: fast election (50ms heartbeat, 150-300ms election)
```

**Key methods:**
- `RaftTestCluster::new(node_count, shard_id, tenant_id)` — bootstrap
- `write_delta(key_byte)` — write through leader
- `isolate_node(id)` / `restore_node(id)` — symmetric partition (blocks both directions, ADV-037-1)
- `block_link(from, to)` / `unblock_link(from, to)` — asymmetric partition
- `wait_for_leader(timeout)` — election verification
- `add_node(id)` / `change_membership(voters)` — membership changes
- `trigger_election(id)` / `trigger_snapshot(id)` — explicit triggers

**File locations:**
- `kiseki-raft/src/mem_transport.rs` — generic in-memory transport
- `kiseki-log/src/raft/test_cluster.rs` — concrete test cluster
- World: `raft_cluster: Option<RaftTestCluster>`

**Unblocks:** 41 scenarios (30 multi-node-raft + 11 cluster-formation)

### 2. Subsystem trait boundaries

Five new trait definitions for remaining scenarios:

**DeviceHealthOps** (5 scenarios) — `kiseki-control/src/device_health.rs`
- `subscribe_health()` → `Receiver<DeviceHealthEvent>`
- `subscribe_device_io_stats(device_id)` → `Receiver<DeviceIOStats>`
- `query_device_io_stats(device_id)` → `DeviceIOStats`

**AdvisoryOps** (10 scenarios) — `kiseki-advisory/src/ops.rs`
- `submit_hint(workflow_ref, advisory)` → `HintResult`
- `subscribe_backpressure(workflow_ref)` → `Receiver<BackpressureEvent>`
- `query_qos_headroom(workflow_ref)` → `QosHeadroom`
- `is_healthy()` → `bool`
- `set_healthy(bool)` — failure injection (ADV-037-4: F-ADV-1 outage scenario)

**GatewayLifecycleOps** (3 scenarios) — `kiseki-gateway/src/lifecycle.rs`
- `crash()` — drop ephemeral state (locks, sessions, cached KEK)
- `restart()` — reconnect to durable stores
- `is_alive()` / `active_sessions()`

**ChunkAvailabilityOps** (3 scenarios) — `kiseki-chunk/src/availability.rs`
- `inject_device_failure(device_id, mode)` — fault injection
- `trigger_repair(device_id)` → `RepairResult`
- `trigger_pool_scrub(pool)` → `RepairResult`

**InlineStoreOps** — already exists (`kiseki-common/src/inline_store.rs`).
Just needs World wiring.

### 3. Tier 2 wiring (no new design needed)

~20 scenarios need existing production code connected to step definitions.
No architect input required — implementer wires directly:
- StorageAdmin RPCs (rebalance, device, reencode, scrub)
- NFS byte-range locking (LockState::add_lock)
- S3 conditional write (If-None-Match)
- Audit event emission (AuditLog.append)
- Compliance tag inheritance (effective_compliance_tags)

## Implementation order

1. **Tier 2 wiring** (~20 scenarios) — no new infrastructure, just connecting
2. **InlineStore wiring** — trait exists, World field needed
3. **ChunkAvailabilityOps** — self-contained in kiseki-chunk
4. **GatewayLifecycleOps** — localized to InMemoryGateway
5. **DeviceHealthOps** — depends on kiseki-chunk device state
6. **Raft test harness** — largest piece, unblocks most scenarios
7. **AdvisoryOps** — most complex, integrates with gateway data path

## Consequences

### Positive
- Clear path to 241/241 scenarios
- Each subsystem trait can be implemented independently
- Raft harness reusable for kiseki-keymanager Raft groups
- No production code changes needed for Tier 2

### Negative
- Raft harness is significant implementation effort
- 5 new trait definitions add API surface
- AdvisoryOps integrates with the gateway data path (cross-cutting)
