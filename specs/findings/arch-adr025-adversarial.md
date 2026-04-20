# Architecture Adversarial Review: ADR-025 (Storage Admin API)

**Date**: 2026-04-20. **Reviewer**: Adversary (architecture mode).

## CRITICAL (9)

### ADR025-C1: No per-tenant resource usage observability (chargeback broken)
- PoolStatus is aggregate — cannot attribute IOPS/capacity to tenants
- **Resolution**: Add per-tenant usage to ControlService (tenant-admin scoped)

### ADR025-C2: No per-device I/O stats
- Cannot diagnose load skew, device-level latency spikes
- **Resolution**: Add DeviceIOStats streaming RPC

### ADR025-C3: No Raft group / shard health observability
- Cannot see shard replication health, leader stability, commit lag
- **Resolution**: Add GetShardHealth RPC

### ADR025-C4: No safeguards for live EC re-encoding
- SetPoolDurability on pool with data — migration or error?
- **Resolution**: EC params immutable per pool; new writes only. Add I-C6.

### ADR025-C5: Compaction rate can be set dangerously low
- Guard rail specified (min 10) but enforcement not explicit
- **Resolution**: Protobuf-level validation, audit event on change

### ADR025-C6: inline_threshold change migration behavior undefined
- Old deltas keep old threshold; new deltas use new — document this
- **Resolution**: Add I-L9: inline payload immutable after write

### ADR025-C7: RemoveDevice without evacuation = data loss
- No precondition check on RemoveDevice
- **Resolution**: Reject unless device state is Removed. Add I-D5.

### ADR025-C8: Cluster admin can starve tenants via pool modification
- SetPoolDurability/EvacuateDevice affect tenant data without approval
- **Resolution**: Audit to tenant shard; add I-T4c

### ADR025-C9: No audit trail for tuning parameter changes
- HIPAA/GDPR compliance requires config change audit
- **Resolution**: Add TuningParameterChanged event, add I-A6

## HIGH (8)

### ADR025-H1: Streaming API state accumulation (memory leak risk)
- Disconnected clients → bounded buffer with drop-oldest semantics

### ADR025-H2: RebalancePool lacks cancellation/progress
- Add operation ID, GetRebalanceProgress, CancelRebalance RPCs

### ADR025-H3: SplitShard no safety check during high write load
- Reject if split already in progress; advisory if write rate high

### ADR025-H4: SetShardMaintenance semantics unclear
- Document: read-only mode, TTL, audit events

### ADR025-H5: No read-only SRE role
- Define cluster-admin, sre-on-call, sre-incident-response roles

### ADR025-H6: Multi-tenancy leakage via pool statistics
- Single-tenant pool stats reveal tenant IO patterns
- Document: pool stats are aggregate, inference is outside contract

### ADR025-H7: DeviceHealth stream leaks placement topology
- Device IDs not exposed to tenants; admin-only

### ADR025-H8: IOStats per pool leaks tenant behavior
- Same as H6 for streaming metrics

## MEDIUM (6)

### ADR025-M1: Parameter inheritance precedence undefined
- Document: effective_value = workload ?? tenant ?? pool ?? cluster

### ADR025-M2: EvacuateDevice progress/timeout undefined
- Return operation_id, add progress RPC, 24h warning, 7d escalation

### ADR025-M3: Scrub/repair semantics underspecified
- Async with operation_id, priority ordering, concurrent limits

### ADR025-M4: DrainNode operation missing
- Add DrainNode RPC that evacuates all devices on a node

### ADR025-M5: Quota enforcement during rebalance
- Rebalance respects capacity thresholds, never pushes to ReadOnly

### ADR025-M6: CancelEvacuation semantics undefined
- Already-moved chunks stay; device returns to Degraded/Healthy

## LOW (3)

### ADR025-L1: No error code mapping
### ADR025-L2: API versioning not specified
### ADR025-L3: CLI mapping incomplete
