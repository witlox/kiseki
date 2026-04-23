# ADR-015: Observability Contract

**Status**: Accepted
**Date**: 2026-04-17
**Context**: A-ADV-7 (observability)

## Decision

OpenTelemetry-native observability with tenant-aware metric scoping.

### Metrics (Prometheus-compatible, via OpenTelemetry)

| Context | Key metrics |
|---|---|
| Log | delta_append_latency, raft_commit_latency, shard_count, shard_size, compaction_duration, election_count |
| Chunk | write_latency, read_latency, dedup_hit_rate, gc_chunks_collected, repair_count, pool_utilization |
| Composition | create_latency, delete_count, multipart_in_progress, refcount_operations |
| View | materialization_lag_ms, staleness_violation_count, rebuild_progress, pin_count |
| Gateway | request_latency (p50/p99/p999), requests_per_sec, error_rate, active_connections |
| Client | fuse_latency, transport_type, cache_hit_rate, prefetch_effectiveness |
| Key Mgr | derive_latency, rotation_in_progress, kms_reachability, cache_hit_rate |
| Control | tenant_count, namespace_count, quota_utilization, federation_sync_lag |

### Zero-trust metric scoping

- **Cluster admin sees**: aggregated metrics, per-node metrics, system health.
  Per-tenant metrics are **anonymized** (tenant_id replaced with opaque hash)
  unless cluster admin has approved access for that tenant.
- **Tenant admin sees**: their own tenant's metrics via tenant audit export.
- **No metric exposes**: file names, directory structure, data content, or
  access patterns attributable to a specific tenant (without approval).

### Distributed tracing

- Every write/read path carries a trace ID (OpenTelemetry context propagation)
- Traces span: client → gateway → composition → log → chunk → view
- Tenant-scoped traces are visible only to the tenant admin
- Cluster admin sees system-level spans (no tenant content in span attributes)

### Structured logging

- JSON structured logs, one line per event
- Log levels: ERROR, WARN, INFO, DEBUG, TRACE
- Tenant-identifying fields are present but content fields are encrypted
- Logs ship to the same audit/observability pipeline

## Consequences

- OpenTelemetry SDK in both Rust and Go codebases
- Metric cardinality must be bounded (no unbounded label values)
- Tracing overhead ~1-2% on data path (acceptable for production)
