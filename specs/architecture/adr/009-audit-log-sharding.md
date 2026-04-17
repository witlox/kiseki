# ADR-009: Audit Log Sharding and GC

**Status**: Accepted
**Date**: 2026-04-17
**Context**: B-ADV-1 (audit log scalability)

## Decision

The audit log is **sharded per tenant** with its own archival lifecycle.

### Architecture

```
Audit subsystem:
  ├── Per-tenant audit shard (append-only, Raft-replicated)
  │   └── Contains: tenant events + relevant system events
  │   └── GC: events archived to cold storage after retention period
  │   └── Retention period: set by compliance tags (e.g., HIPAA = 6 years)
  │
  ├── System audit shard (cluster-wide operational events)
  │   └── Contains: node events, maintenance, non-tenant-scoped events
  │   └── GC: configurable retention (default 1 year)
  │
  └── Export pipeline
      └── Tenant export: filtered stream to tenant VLAN
      └── System export: to cluster admin's SIEM
```

### GC interaction with delta GC (I-L4)

- Each tenant audit shard tracks its own watermark per data shard
- Delta GC checks the relevant tenant audit shard's watermark
- A stalled tenant audit shard blocks delta GC only for that tenant's
  data shards (not cluster-wide)

## Rationale

- Single global audit log is a cluster-wide GC bottleneck (B-ADV-1)
- Per-tenant sharding: stalled export for one tenant doesn't block others
- Audit retention aligns with compliance (HIPAA 6yr, GDPR varies)
- Archived events move to cold storage (bulk-nvme pool) after active retention

### GC safety valve and backpressure (analyst backpass contention 2)

**Default behavior (safety valve)**: if a tenant's audit export stalls
for > configurable threshold (default 24 hours), data shard GC proceeds
anyway. The audit gap is logged, and the compliance team is notified.
Storage exhaustion is worse than an auditable gap.

**Per-tenant configurable**: tenants can enable **audit backpressure** mode.
When enabled, if the audit export falls behind, write throughput for that
tenant is throttled (reducing GC pressure at the source). This preserves
audit completeness at the cost of write performance.

| Mode | GC behavior | Write impact | Use case |
|---|---|---|---|
| Safety valve (default) | GC proceeds after timeout | None | Most tenants |
| Backpressure (opt-in) | GC waits; writes throttled | Slower writes | Strict compliance |

## Consequences

- More audit shards to manage (one per tenant + one system)
- Audit Raft groups are lightweight (small append-only logs)
- Archival pipeline is a background process
- Safety valve prevents storage exhaustion from stalled audit export
- Backpressure mode available for tenants with strict audit requirements
