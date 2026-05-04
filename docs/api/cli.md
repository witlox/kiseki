# CLI Reference

Kiseki ships three binaries:

| Binary | Purpose |
|---|---|
| `kiseki-server` | Storage node daemon. No subcommands — configuration is via environment variables (see [Environment Variables](environment.md)). |
| `kiseki-admin` | Lightweight HTTP-based cluster summary CLI. Connects to any node's metrics port (default `:9090`). Zero external dependencies — safe to run from a workstation. |
| `kiseki-storage` | Full `StorageAdminService` gRPC client (ADR-025). 26 verbs covering every storage admin RPC: pools, devices, shards, tuning, repair, scrub, streaming events. |

The native client (`kiseki-client`) is documented at the end for dataset staging and cache management on compute nodes.

---

## kiseki-server

```
kiseki-server
```

Starts the storage node. All configuration is via environment variables — there are no positional arguments or subcommands. Run with `KISEKI_DATA_DIR=/var/lib/kiseki` (or unset for in-memory mode), `KISEKI_DATA_ADDR=0.0.0.0:50051`, etc. Full list in [Environment Variables](environment.md).

---

## kiseki-admin (HTTP cluster summary)

Standalone remote administration CLI. Talks to any node's metrics+admin HTTP port (default `:9090`). No mTLS required (the metrics port is plaintext); intended for read-only cluster summaries and a small set of cluster-wide ops (maintenance, backup trigger, scrub trigger). For per-pool / per-device / per-shard mutations, use `kiseki-storage`.

Default endpoint: `KISEKI_ENDPOINT` env var, or `http://localhost:9090`.

### status

```
kiseki-admin --endpoint http://storage-node:9090 status
```

Cluster status summary: node count, Raft entries, gateway requests, data written/read, and active connections.

```
Cluster Status
══════════════
Nodes:       3/3 healthy
Raft:        42,567 entries
Requests:    1,234 served
Written:     12.5 GB
Read:        8.2 GB
Connections: 15 active
```

### nodes

```
kiseki-admin nodes
```

Per-node table with health badges + per-node metrics aggregated across the cluster.

### events

```
kiseki-admin events [--severity {info|warning|error|critical}] [--hours N]
```

Filtered event log. `--hours` defaults to 3.

### history

```
kiseki-admin history [--hours N]
```

Metric history time series (default 3 h).

### maintenance / backup / scrub

```
kiseki-admin maintenance on
kiseki-admin maintenance off
kiseki-admin backup
kiseki-admin scrub
```

Toggle cluster-wide maintenance mode (sets all shards read-only — write commands return a retriable error per I-O6), trigger an immediate backup snapshot (ADR-016), or kick off an integrity scrub.

---

## kiseki-storage (gRPC StorageAdminService)

Full client for the `StorageAdminService` defined in ADR-025. 26 verbs — one per RPC. Connects via tonic gRPC (mTLS-aware once the server is configured for it). Exhaustive coverage of pool / device / shard / tuning / repair / scrub admin surface.

Default endpoint: `KISEKI_STORAGE_ENDPOINT` env var, or `http://localhost:50051` (the data-path gRPC port). Override with `--endpoint <url>`.

### Devices

```
kiseki-storage devices list [--pool <name>]
kiseki-storage devices get <device-id>
kiseki-storage devices add <pool> <device-id> [--capacity <bytes>] [--class {nvme_ssd|ssd|hdd|mixed}]
kiseki-storage devices remove <device-id>
kiseki-storage devices evacuate <device-id> [--throughput <mb/s>]
kiseki-storage devices cancel-evacuation <evac-id>
```

`evacuate` returns an `evacuation_id` that the matching `cancel-evacuation` consumes. The drain orchestrator (ADR-035) is the consumer of progress updates.

### Pools

```
kiseki-storage pools list
kiseki-storage pools get <name>
kiseki-storage pools status <name>
kiseki-storage pools create <name> --class <kind> --durability {replication|erasure_coding} \
                                   [--copies N | --data-shards N --parity-shards N] \
                                   [--capacity <bytes>]
kiseki-storage pools set-durability <name> --durability <kind> [--copies N | --data-shards N --parity-shards N]
kiseki-storage pools set-thresholds <name> [--warn N] [--critical N] [--readonly N] [--target N]
kiseki-storage pools rebalance <name> [--throughput <mb/s>]
```

`create` / `set-durability` validate per ADR-005: replication copies in `[2, 5]`, EC data shards `[2, 16]`, EC parity shards `[1, 8]`. `set-durability` rejects when the pool is non-empty (`used_bytes > 0`) — durability change while data exists requires a separate migration plan.

`set-thresholds` validates per ADR-024: warning ∈ `[50, 95]`, critical ∈ `[60, 98]`, readonly ∈ `[70, 99]`, target_fill ∈ `[50, 90]`, with cross-field ordering `warning < critical < readonly`. Zero is treated as "unset" (uses ADR-024 defaults).

### Tuning

```
kiseki-storage tuning get
kiseki-storage tuning set KEY=VALUE [KEY=VALUE...]
```

`tuning get` returns the 8 cluster-wide parameters from ADR-025 §"Cluster-wide tuning". `tuning set` accepts one or more `KEY=VALUE` overrides; the rest of the params keep their current values. Recognised keys: `compaction_rate_mb_s` (10..=1000), `gc_interval_s` (60..=3600), `rebalance_rate_mb_s` (0..=500), `scrub_interval_h` (24..=720), `max_concurrent_repairs` (1..=32), `stream_proc_poll_ms` (10..=1000), `inline_threshold_bytes` (512..=65536), `raft_snapshot_interval` (1000..=100000).

Persisted to `<KISEKI_DATA_DIR>/tuning/tuning.redb` when the data dir is set; in-memory otherwise. Survives server restart.

### Cluster + observability

```
kiseki-storage cluster status
kiseki-storage device-health [--device <id>]
kiseki-storage io-stats [--pool <name>]
```

`cluster status` returns aggregated node count, total/used capacity, leader node, and a sampled-at timestamp. `device-health` and `io-stats` are server-streaming RPCs — they consume one event per invocation today (a `--watch` mode lands once the data-path producers ship). The producers (chunk-store device-state observer, chunk-cluster IOStats sampler) are wired in follow-on PRs; until then both streams hold open subscriptions but emit nothing.

### Shards

```
kiseki-storage shards list [--tenant <id>]
kiseki-storage shards get <id>
kiseki-storage shards split <id> [--pivot <key>]
kiseki-storage shards merge <left-id> <right-id>
kiseki-storage shards maintenance <id> {on|off}
```

`split` returns `(left_shard_id, right_shard_id)` — the original shard becomes "left" (lower half of the key range), the new shard is "right". `merge` is right-into-left: the left id survives, the right is decommissioned (ADR-034 protocol; `Retiring` state). `maintenance` flips a per-shard atomic that gates writes (`PutFragment` returns `FailedPrecondition`) while leaving reads served.

### Repair / scrub

```
kiseki-storage scrub [--pool <name>]
kiseki-storage repair-chunk <chunk-id-hex>
kiseki-storage repairs list [--limit N]
```

`scrub` triggers an on-demand pass via `ScrubScheduler::trigger_now()` — returns a `scrub_id` immediately; the pass runs async and writes a record into the repair tracker. `repair-chunk` reuses the under-replication scrub on a single-chunk candidate list and reports `already_healthy` when the placement matched the policy. `repairs list` returns up to N (default 100, max 1000) most-recent records — newest first.

### Observability

Every `kiseki-storage` RPC emits an OpenTelemetry span named `StorageAdminService.<RpcName>` and bumps the Prometheus counter `kiseki_storage_admin_calls_total{rpc, outcome}` where `outcome` is one of `ok`, `client_error`, `server_error`, `unimplemented`. Operator audit log: every `tuning set` is also logged at `tracing::info` with the full parameter set.

---

## kiseki-client

The native client binary provides dataset staging and cache management commands for compute nodes.

### stage --dataset

```
kiseki-client stage --dataset <path> [--timeout <seconds>]
```

Pre-fetch a dataset's chunks into the L2 cache with pinned retention. Recursively enumerates compositions under the given namespace path, fetches all chunks from canonical, verifies by content-address (SHA-256), and stores in the L2 cache pool.

Staging is idempotent and resumable. Produces a manifest file listing staged compositions and chunk IDs.

Limits: `max_staging_depth` (10 levels), `max_staging_files` (100,000).

### stage --status

```
kiseki-client stage --status
```

Show the status of the current staging operation: progress, number of chunks fetched, total size, and any errors.

### stage --release

```
kiseki-client stage --release <path>
```

Release a staged dataset. Unpins cached chunks, making them eligible for LRU eviction. To pick up updates from canonical, release and re-stage.

### stage --release-all

```
kiseki-client stage --release-all
```

Release all staged datasets.

### cache --stats

```
kiseki-client cache --stats
```

Print cache statistics: mode, L1/L2 bytes used, hit/miss counts, errors, metadata cache stats, and wipe count.

### cache --wipe

```
kiseki-client cache --wipe
```

Wipe all cached data (L1 + L2 + metadata). Zeroizes data before deletion (I-CC2).

### version

```
kiseki-client version
```

Print the client version.

---

## Environment variables (kiseki-client)

| Variable | Default | Description |
|----------|---------|-------------|
| `KISEKI_CACHE_DIR` | `/tmp/kiseki-cache` | Cache directory |
| `KISEKI_CACHE_MODE` | `organic` | Cache mode: `pinned`, `organic`, `bypass` |
| `KISEKI_CACHE_L1_MAX` | `268435456` (256 MB) | L1 max bytes |
| `KISEKI_CACHE_L2_MAX` | `53687091200` (50 GB) | L2 max bytes |

---

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | General error (incl. RPC `server_error` outcome) |
| 2 | Invalid arguments |
| 3 | Connection failure (server unreachable) |
| 4 | Authentication failure (mTLS) |
