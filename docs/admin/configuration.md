# Configuration Reference

Kiseki is configured entirely through environment variables. There are
no configuration files to manage. Every tunable parameter has a
sensible default. Variables are grouped by function below.

---

## Network addresses

| Variable | Default | Description |
|----------|---------|-------------|
| `KISEKI_DATA_ADDR` | `0.0.0.0:9100` | Listen address for data-path gRPC (log, chunk, composition, view, discovery). |
| `KISEKI_ADVISORY_ADDR` | `0.0.0.0:9101` | Listen address for the Workflow Advisory gRPC service. Runs on a dedicated tokio runtime, isolated from the data path (ADR-021). |
| `KISEKI_S3_ADDR` | `0.0.0.0:9000` | Listen address for the S3 HTTP gateway. |
| `KISEKI_NFS_ADDR` | `0.0.0.0:2049` | Listen address for the NFS gateway (v3 + v4.2). |
| `KISEKI_METRICS_ADDR` | `0.0.0.0:9090` | Listen address for Prometheus metrics (`/metrics`), health endpoint (`/health`), and admin dashboard (`/ui`). |
| `KISEKI_RAFT_ADDR` | `0.0.0.0:9300` | Listen address for Raft consensus traffic between nodes. |

All addresses accept the `host:port` format. Use `0.0.0.0` to bind to
all interfaces or a specific IP to restrict to one network.

---

## Cluster membership

| Variable | Default | Description |
|----------|---------|-------------|
| `KISEKI_NODE_ID` | (required) | Unique integer identifier for this node within the cluster. Must be stable across restarts. |
| `KISEKI_RAFT_PEERS` | (required) | Comma-separated list of `id=host:port` pairs for all Raft voters. Example: `1=node1:9300,2=node2:9300,3=node3:9300`. Must be identical on every node. |
| `KISEKI_BOOTSTRAP` | `false` | When `true`, the node creates an initial shard on first start. Set to `true` on exactly one node during initial cluster formation, then set back to `false`. |

---

## Storage

| Variable | Default | Description |
|----------|---------|-------------|
| `KISEKI_DATA_DIR` | `/var/lib/kiseki` | Root directory for all persistent state. Contains Raft log (`raft/log.redb`), key epochs (`keys/epochs.redb`), chunk metadata (`chunks/meta.redb`), and inline small-file content (`small/objects.redb`). Must reside on a low-latency device (NVMe or SSD strongly recommended; HDD triggers a boot warning). |

### Data directory layout

```
KISEKI_DATA_DIR/
  raft/log.redb            Raft log entries (bounded by snapshot policy)
  keys/epochs.redb         Key epoch metadata (<10 MB)
  chunks/meta.redb         Chunk extent index (scales with file count)
  small/objects.redb        Small-file encrypted content (capacity-managed)
```

---

## TLS / mTLS

| Variable | Default | Description |
|----------|---------|-------------|
| `KISEKI_CA_PATH` | (none) | Path to the Cluster CA certificate (PEM). Required for production. When set, all gRPC connections require mTLS. |
| `KISEKI_CERT_PATH` | (none) | Path to this node's TLS certificate (PEM), signed by the Cluster CA. |
| `KISEKI_KEY_PATH` | (none) | Path to this node's TLS private key (PEM). Never logged, printed, or transmitted. |
| `KISEKI_CRL_PATH` | (none) | Path to a CRL file (PEM) for certificate revocation. Reloaded periodically. Optional; if not set, CRL checking is disabled. |

When `KISEKI_CA_PATH` is not set, the server runs without TLS. This is
acceptable for development but must not be used in production.

---

## Client-side cache (ADR-031)

These variables configure the native client cache on compute nodes
running `kiseki-client-fuse`.

| Variable | Default | Description |
|----------|---------|-------------|
| `KISEKI_CACHE_MODE` | `organic` | Cache operating mode. One of: `pinned` (staging-driven, eviction-resistant), `organic` (LRU with usage-weighted retention), `bypass` (no caching). Mode is per session, not per file. |
| `KISEKI_CACHE_DIR` | `$KISEKI_DATA_DIR/cache` | Directory for L2 cache pools on local NVMe. Each client process creates an isolated pool with a unique `pool_id`. |
| `KISEKI_CACHE_L1_MAX` | `1073741824` (1 GB) | Maximum bytes for the in-memory L1 cache (decrypted plaintext chunks). Bounded by process memory. |
| `KISEKI_CACHE_L2_MAX` | `107374182400` (100 GB) | Maximum bytes for the on-disk L2 cache on local NVMe. Per-process, per-tenant isolation via pool directories. |
| `KISEKI_CACHE_META_TTL_MS` | `5000` (5 seconds) | Metadata TTL in milliseconds. File-to-chunk-list mappings are served from cache within this window. After expiry, mappings are re-fetched from canonical. This is the sole freshness window: chunk data itself has no TTL because chunks are immutable (I-C1). |
| `KISEKI_CACHE_POOL_ID` | (none) | Adopt an existing L2 cache pool instead of creating a new one. Used for staging handoff from a Slurm prolog daemon to a workload process. |

### Cache behavior notes

- **Pinned mode**: Pre-staged datasets remain in cache until explicitly
  released. Best for training workloads that re-read the same data
  across epochs.
- **Organic mode**: LRU eviction with usage-weighted retention. Default
  for mixed workloads.
- **Bypass mode**: No caching at all. Best for checkpoint/restart and
  streaming workloads.
- On process restart, the client creates a new L2 pool (wiping orphaned
  pools). A `kiseki-cache-scrub` service cleans orphans on node boot.
- Disconnects longer than 300 seconds (configurable) wipe the entire
  cache.
- Crypto-shred events wipe all cached plaintext for the affected tenant
  within the key health check interval (default 30 seconds).

---

## Metadata capacity (ADR-030)

These variables control the dynamic inline threshold for small-file
placement.

| Variable | Default | Description |
|----------|---------|-------------|
| `KISEKI_META_SOFT_LIMIT_PCT` | `50` | Normal operating ceiling for system disk metadata usage, as a percentage of system partition capacity. Exceeding this triggers inline threshold reduction. |
| `KISEKI_META_HARD_LIMIT_PCT` | `75` | Absolute maximum for system disk metadata usage. Exceeding this forces the inline threshold to the floor (128 bytes) and emits an alert via out-of-band gRPC (not Raft). |

The inline threshold determines whether a file's encrypted content is
stored in `small/objects.redb` (metadata tier, NVMe) or as a chunk
extent on a raw block device (data tier). The threshold is computed
per-shard as the minimum affordable threshold across all Raft voters,
clamped between 128 bytes (floor) and 64 KB (ceiling).

---

## Observability

| Variable | Default | Description |
|----------|---------|-------------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | (none) | OpenTelemetry OTLP gRPC endpoint for distributed traces. Example: `http://jaeger:4317`. When not set, tracing is disabled. |
| `OTEL_SERVICE_NAME` | `kiseki-server` | Service name reported in traces. Set to `kiseki-keyserver` or `kiseki-client` for other binaries. |
| `RUST_LOG` | `info` | Logging filter directive for the `tracing` crate. Supports per-module granularity. Examples: `kiseki=debug`, `kiseki_raft=trace,kiseki=info`, `warn`. |
| `KISEKI_LOG_FORMAT` | `text` | Log output format. `text` for human-readable, `json` for structured JSON (one line per event). Use `json` in production for log aggregation. |

---

## Tuning parameters (runtime)

The following parameters are set at runtime via the `StorageAdminService`
gRPC API (`SetTuningParams` / `GetTuningParams`), not via environment
variables. They are listed here for reference.

### Cluster-wide tuning

| Parameter | Default | Range | Description |
|-----------|---------|-------|-------------|
| `compaction_rate_mb_s` | 100 | 10-1000 | Background compaction throughput cap (MB/s). |
| `gc_interval_s` | 300 | 60-3600 | Interval between GC scans for reclaimable chunks. |
| `rebalance_rate_mb_s` | 50 | 0-500 | Background rebalance/evacuation throughput (MB/s). |
| `scrub_interval_h` | 168 (7 days) | 24-720 | Interval between integrity scrub runs. |
| `max_concurrent_repairs` | 4 | 1-32 | Maximum parallel EC repair jobs. |
| `stream_proc_poll_ms` | 100 | 10-1000 | View materialization polling interval (ms). |
| `inline_threshold_bytes` | 4096 | 512-65536 | Default inline threshold for new shards. |
| `raft_snapshot_interval` | 10000 | 1000-100000 | Entries between Raft snapshots. |

### Per-pool tuning

| Parameter | Default | Range | Description |
|-----------|---------|-------|-------------|
| `ec_data_chunks` | 4 (NVMe) / 8 (HDD) | 2-16 | EC data fragment count. Immutable per pool after creation (I-C6). |
| `ec_parity_chunks` | 2 (NVMe) / 3 (HDD) | 1-8 | EC parity fragment count. Immutable per pool after creation. |
| `replication_count` | 3 | 2-5 | For replication pools (non-EC). |
| `warning_threshold_pct` | Per device class | 50-95 | Pool capacity warning level. |
| `critical_threshold_pct` | Per device class | 60-98 | Pool capacity critical level. Writes rejected. |
| `readonly_threshold_pct` | Per device class | 70-99 | Read-only level. In-flight writes drain. |
| `target_fill_pct` | 70 (SSD) / 80 (HDD) | 50-90 | Rebalance target fill level. |

Default capacity thresholds by device class:

| State | NVMe/SSD | HDD |
|-------|----------|-----|
| Healthy | 0-75% | 0-85% |
| Warning | 75-85% | 85-92% |
| Critical | 85-92% | 92-97% |
| ReadOnly | 92-97% | 97-99% |
| Full | 97-100% | 99-100% |

All tuning parameter changes via `SetTuningParams` are recorded in the
cluster audit shard with parameter name, old value, new value, timestamp,
and admin identity (I-A6).

---

## Environment variable summary

Quick reference of all environment variables:

```bash
# Network
KISEKI_DATA_ADDR=0.0.0.0:9100
KISEKI_ADVISORY_ADDR=0.0.0.0:9101
KISEKI_S3_ADDR=0.0.0.0:9000
KISEKI_NFS_ADDR=0.0.0.0:2049
KISEKI_METRICS_ADDR=0.0.0.0:9090
KISEKI_RAFT_ADDR=0.0.0.0:9300

# Cluster
KISEKI_NODE_ID=1
KISEKI_RAFT_PEERS=1=node1:9300,2=node2:9300,3=node3:9300
KISEKI_BOOTSTRAP=false

# Storage
KISEKI_DATA_DIR=/var/lib/kiseki

# TLS
KISEKI_CA_PATH=/etc/kiseki/tls/ca.crt
KISEKI_CERT_PATH=/etc/kiseki/tls/server.crt
KISEKI_KEY_PATH=/etc/kiseki/tls/server.key
KISEKI_CRL_PATH=/etc/kiseki/tls/crl.pem

# Cache (client only)
KISEKI_CACHE_MODE=organic
KISEKI_CACHE_DIR=/var/cache/kiseki
KISEKI_CACHE_L1_MAX=1073741824
KISEKI_CACHE_L2_MAX=107374182400
KISEKI_CACHE_META_TTL_MS=5000

# Metadata capacity
KISEKI_META_SOFT_LIMIT_PCT=50
KISEKI_META_HARD_LIMIT_PCT=75

# Observability
OTEL_EXPORTER_OTLP_ENDPOINT=http://jaeger:4317
OTEL_SERVICE_NAME=kiseki-server
RUST_LOG=kiseki=info
KISEKI_LOG_FORMAT=json
```
