# Monitoring & Observability

Kiseki provides three observability pillars: metrics (Prometheus),
structured logging (tracing), and distributed traces (OpenTelemetry).
All three are tenant-aware, respecting the zero-trust boundary between
cluster admin and tenant admin (ADR-015).

---

## Prometheus metrics

Every `kiseki-server` node exposes Prometheus metrics in text exposition
format on the metrics HTTP port.

### Endpoint

```
GET http://<node>:9090/metrics
```

### Registered metrics

| Metric name | Type | Labels | Description |
|-------------|------|--------|-------------|
| `kiseki_raft_commit_latency_seconds` | Histogram | `shard` | Raft commit latency per shard. Buckets: 100us to 1s. |
| `kiseki_raft_entries_total` | Counter | (none) | Total Raft entries applied on this node. |
| `kiseki_chunk_write_bytes_total` | Counter | (none) | Total chunk bytes written. |
| `kiseki_chunk_read_bytes_total` | Counter | (none) | Total chunk bytes read. |
| `kiseki_chunk_ec_encode_seconds` | Histogram | `strategy` | EC encode latency. Buckets: 100us to 50ms. |
| `kiseki_gateway_requests_total` | Counter | `method`, `status` | Gateway request count by method (GET, PUT, DELETE, etc.) and HTTP status. |
| `kiseki_gateway_request_duration_seconds` | Histogram | `method` | Gateway request duration. Buckets: 1ms to 5s. |
| `kiseki_pool_capacity_total_bytes` | Gauge | `pool` | Total capacity per pool in bytes. |
| `kiseki_pool_capacity_used_bytes` | Gauge | `pool` | Used capacity per pool in bytes. |
| `kiseki_transport_connections_active` | Gauge | (none) | Active transport connections. |
| `kiseki_transport_connections_idle` | Gauge | (none) | Idle transport connections. |
| `kiseki_shard_delta_count` | Gauge | `shard` | Current delta count per shard. |
| `kiseki_key_rotation_total` | Counter | (none) | Key rotations performed (system + tenant). |
| `kiseki_crypto_shred_total` | Counter | (none) | Crypto-shred operations performed. |

### Metric scoping (zero-trust)

Per ADR-015, metric scoping respects the zero-trust boundary:

- **Cluster admin sees**: Aggregated metrics, per-node metrics, system
  health. Per-tenant metrics are anonymized (tenant_id replaced with
  opaque hash) unless the cluster admin has approved access for that
  tenant.
- **Tenant admin sees**: Their own tenant's metrics via the tenant audit
  export.
- **No metric exposes**: File names, directory structure, data content,
  or access patterns attributable to a specific tenant (without
  approval).

### Metric cardinality

Metric cardinality is bounded by design. Label values are drawn from
fixed sets (shard IDs, pool names, HTTP methods, strategy names). There
are no unbounded label values such as file paths, tenant IDs, or user
identifiers in metrics labels.

---

## Structured logging

Kiseki uses the `tracing` crate for structured logging. Every log event
is a structured record with typed fields.

### Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `RUST_LOG` | `info` | Filter directive. Supports per-module granularity. |
| `KISEKI_LOG_FORMAT` | `text` | Output format: `text` (human-readable) or `json` (structured). |

### Filter examples

```bash
# Default: info-level for all Kiseki modules
RUST_LOG=kiseki=info

# Debug for the Raft subsystem, info for everything else
RUST_LOG=kiseki_raft=debug,kiseki=info

# Trace-level for the chunk subsystem (very verbose)
RUST_LOG=kiseki_chunk=trace,kiseki=info

# Warnings only (quiet)
RUST_LOG=warn
```

### JSON output format

In production, set `KISEKI_LOG_FORMAT=json` for structured log
aggregation (ELK, Loki, Datadog, etc.):

```json
{
  "timestamp": "2026-04-23T14:30:00.123Z",
  "level": "INFO",
  "target": "kiseki_raft",
  "message": "Raft leader elected",
  "shard": "shard-0001",
  "node_id": 1,
  "term": 42
}
```

### Log levels

| Level | Usage |
|-------|-------|
| `ERROR` | Unrecoverable failures, invariant violations, data loss events. |
| `WARN` | Recoverable issues, degraded state, approaching capacity limits. |
| `INFO` | Significant state changes: leader election, key rotation, shard split, node join/leave. |
| `DEBUG` | Detailed operational events: individual RPCs, cache hits/misses, EC operations. |
| `TRACE` | Wire-level detail: Raft message contents, HKDF inputs, bitmap operations. |

### Security in logs

- Tenant-identifying fields (tenant_id, namespace) are present for
  correlation.
- Content fields (file names, chunk plaintext, key material) are never
  logged (I-K8).
- Logs ship to the same audit/observability pipeline.

---

## Distributed tracing (OpenTelemetry)

Kiseki uses OpenTelemetry for distributed tracing across the full
write/read path.

### Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | (none) | OTLP gRPC endpoint. Example: `http://jaeger:4317`. When not set, tracing is disabled. |
| `OTEL_SERVICE_NAME` | `kiseki-server` | Service name in traces. |
| `OTEL_TRACES_SAMPLER_ARG` | `1.0` | Sampling rate (1.0 = 100%, 0.1 = 10%). Reduce in production for high-throughput workloads. |

### Trace propagation

Every write/read path carries a trace ID via OpenTelemetry context
propagation. Traces span:

```
client -> gateway -> composition -> log -> chunk -> view
```

For the native client path:

```
client (FUSE) -> transport -> composition -> log -> chunk
```

### Jaeger integration

The development Docker Compose stack includes Jaeger for trace
visualization:

- **Jaeger UI**: `http://localhost:16686`
- **OTLP gRPC receiver**: `localhost:4317`

### Trace scoping

Traces respect the zero-trust boundary:

- **Tenant-scoped traces** are visible only to the tenant admin (via
  tenant audit export).
- **Cluster admin** sees system-level spans. No tenant content appears
  in span attributes visible to the cluster admin.
- Trace overhead is approximately 1-2% on the data path (acceptable for
  production).

---

## Event store

The admin dashboard maintains an in-memory event store for diagnostic
events. Events are categorized and severity-tagged.

### Event categories

| Category | Events |
|----------|--------|
| `node` | Node join, node leave, node unreachable, node recovered. |
| `shard` | Shard created, shard split, shard maintenance entered/exited. |
| `device` | Device added, device failed, SMART warning, evacuation started/completed. |
| `tenant` | Tenant created, tenant deleted, quota changed. |
| `security` | Auth failure, cert revocation, crypto-shred. |
| `admin` | Maintenance mode toggle, backup requested, scrub requested, tuning parameter change. |
| `gateway` | Protocol errors, connection surge, rate limiting. |
| `raft` | Leader election, membership change, snapshot transfer. |

### Event severities

| Severity | Description |
|----------|-------------|
| `info` | Normal operations. |
| `warning` | Attention needed, but system is operating. |
| `error` | Failure requiring investigation. |
| `critical` | Immediate action required (data at risk, quorum lost). |

### Event API

```bash
# All events from the last 3 hours
curl http://node1:9090/ui/api/events

# Errors from the last hour
curl 'http://node1:9090/ui/api/events?severity=error&hours=1'

# Device events, last 50
curl 'http://node1:9090/ui/api/events?category=device&limit=50'

# Security events from the last 24 hours
curl 'http://node1:9090/ui/api/events?category=security&hours=24'
```

### Historical metrics API

```bash
# Metric snapshots from the last 3 hours
curl http://node1:9090/ui/api/history

# Last 6 hours
curl 'http://node1:9090/ui/api/history?hours=6'
```

The history endpoint returns time-series data points suitable for
charting. The default retention is 3 hours in memory. For longer
retention, use Prometheus.

---

## Grafana integration

For production monitoring with alerting and long-term storage, configure
Prometheus to scrape Kiseki metrics and visualize with Grafana.

### Prometheus scrape configuration

```yaml
scrape_configs:
  - job_name: 'kiseki'
    scrape_interval: 15s
    static_configs:
      - targets:
          - 'node1:9090'
          - 'node2:9090'
          - 'node3:9090'
    metrics_path: '/metrics'
```

### Recommended Grafana dashboards

**Cluster overview dashboard**:

- Cluster health (up/down per node)
- Total Raft entries/sec (rate of `kiseki_raft_entries_total`)
- Gateway request rate (rate of `kiseki_gateway_requests_total`)
- Gateway latency p50/p99 (`kiseki_gateway_request_duration_seconds`)
- Pool utilization (`kiseki_pool_capacity_used_bytes` /
  `kiseki_pool_capacity_total_bytes`)

**Per-node dashboard**:

- Raft commit latency histogram
  (`kiseki_raft_commit_latency_seconds`)
- Chunk read/write throughput
- Transport connection count
- Shard delta count per shard

**Capacity dashboard**:

- Pool fill percentage over time
- Pool capacity trend (linear projection for capacity planning)
- Delta count growth rate (shard split prediction)

**Key management dashboard**:

- Key rotation count over time (`kiseki_key_rotation_total`)
- Crypto-shred count (`kiseki_crypto_shred_total`)

### Alerting rules

Recommended Prometheus alerting rules:

```yaml
groups:
  - name: kiseki
    rules:
      - alert: KisekiNodeDown
        expr: up{job="kiseki"} == 0
        for: 1m
        labels:
          severity: critical

      - alert: KisekiPoolCapacityWarning
        expr: >
          kiseki_pool_capacity_used_bytes / kiseki_pool_capacity_total_bytes > 0.85
        for: 5m
        labels:
          severity: warning

      - alert: KisekiPoolCapacityCritical
        expr: >
          kiseki_pool_capacity_used_bytes / kiseki_pool_capacity_total_bytes > 0.92
        for: 1m
        labels:
          severity: critical

      - alert: KisekiGatewayLatencyHigh
        expr: >
          histogram_quantile(0.99, rate(kiseki_gateway_request_duration_seconds_bucket[5m])) > 1
        for: 5m
        labels:
          severity: warning

      - alert: KisekiRaftCommitLatencyHigh
        expr: >
          histogram_quantile(0.99, rate(kiseki_raft_commit_latency_seconds_bucket[5m])) > 0.1
        for: 5m
        labels:
          severity: warning
```
