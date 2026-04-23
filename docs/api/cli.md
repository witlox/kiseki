# CLI Reference

Kiseki provides two binaries with CLI interfaces: `kiseki-server` (which
doubles as the admin CLI) and `kiseki-client` (native client with staging
and cache commands).

All admin operations use these CLIs. The underlying gRPC API is also
available for programmatic access (see [gRPC](grpc.md)), but the CLI is
the primary admin interface.

---

## kiseki-server

The server binary starts the storage node when invoked without arguments.
When invoked with a subcommand, it acts as an admin CLI that connects to
the local node's gRPC endpoint.

### Server mode

```
kiseki-server
```

Starts the storage node. Configuration is via environment variables (see
[Environment Variables](environment.md)).

### status

```
kiseki-server status
```

Display cluster status summary: node count, shard count, device health,
Raft leadership, and pool utilization.

### Node management

```
kiseki-server node add --node-id <id>
kiseki-server node drain --node-id <id>
kiseki-server node remove --node-id <id>
```

Add, drain, or remove a node from the cluster. Drain migrates shard
assignments before removal. See
[Cluster Management](../admin/cluster-management.md).

### Shard management

```
kiseki-server shard list
kiseki-server shard info --shard-id <id>
kiseki-server shard health --shard-id <id>
kiseki-server shard split --shard-id <id> [--boundary <key>]
kiseki-server shard maintenance --shard-id <id> --enabled
kiseki-server shard maintenance --shard-id <id> --disabled
```

List shards, inspect details, check health, trigger manual splits, and
toggle per-shard maintenance mode (I-O6).

### Pool management

```
kiseki-server pool list
kiseki-server pool status --pool-id <id>
kiseki-server pool create --pool-id <id> --device-class <class> --ec-data <n> --ec-parity <n>
kiseki-server pool set-durability --pool-id <id> --ec-data <n> --ec-parity <n>
kiseki-server pool rebalance --pool-id <id>
kiseki-server pool cancel-rebalance --pool-id <id>
kiseki-server pool set-thresholds --pool-id <id> --warning-pct <n> --critical-pct <n>
```

Manage affinity pools: create, inspect capacity, set EC parameters,
rebalance data, and adjust capacity thresholds (I-C5, I-C6).

### Device management

```
kiseki-server device list
kiseki-server device info --device-id <id>
kiseki-server device evacuate --device-id <id>
kiseki-server device cancel-evacuation --device-id <id>
kiseki-server device scrub --device-id <id>
```

List devices, check health and SMART status, trigger evacuation or
integrity scrub, and cancel in-progress evacuations (I-D2, I-D3, I-D5).

### Maintenance mode

```
kiseki-server maintenance on
kiseki-server maintenance off
```

Enable or disable cluster-wide maintenance mode. Sets all shards to
read-only. Write commands are rejected with a retriable error. Shard
splits, compaction, and GC for in-progress operations continue but no
new triggers fire from write pressure (I-O6).

### Backup and recovery

```
kiseki-server backup create
kiseki-server backup list
kiseki-server backup delete --backup-id <id>
kiseki-server repair list
kiseki-server compact
```

Create, list, and delete backup snapshots. List active repairs and
evacuations. Trigger Raft log compaction.

### Key management

```
kiseki-server keymanager health
kiseki-server keymanager check-kms
kiseki-server keymanager check-kms --tenant-id <id>
```

Check system key manager health and tenant KMS connectivity.

### S3 credentials

```
kiseki-server s3-credentials create --tenant-id <id> --workload-id <id>
```

Provision S3-compatible access keys for a tenant workload via the
control plane.

### Tuning parameters

```
kiseki-server tuning set --inline-threshold-bytes <n>
kiseki-server tuning set --raft-snapshot-interval <n>
kiseki-server tuning set --compaction-rate-mb-s <n>
kiseki-server tuning set --stream-proc-poll-ms <n>
```

Adjust cluster-wide tuning parameters. See
[Performance Tuning](../operations/performance.md) for guidance.

---

## kiseki-client

The native client binary provides dataset staging and cache management
commands for compute nodes.

### stage --dataset

```
kiseki-client stage --dataset <path> [--timeout <seconds>]
```

Pre-fetch a dataset's chunks into the L2 cache with pinned retention.
Recursively enumerates compositions under the given namespace path, fetches
all chunks from canonical, verifies by content-address (SHA-256), and
stores in the L2 cache pool.

Staging is idempotent and resumable. Produces a manifest file listing
staged compositions and chunk IDs.

Limits: `max_staging_depth` (10 levels), `max_staging_files` (100,000).

### stage --status

```
kiseki-client stage --status
```

Show the status of the current staging operation: progress, number of
chunks fetched, total size, and any errors.

### stage --release

```
kiseki-client stage --release <path>
```

Release a staged dataset. Unpins cached chunks, making them eligible for
LRU eviction. To pick up updates from canonical, release and re-stage.

### stage --release-all

```
kiseki-client stage --release-all
```

Release all staged datasets.

### cache --stats

```
kiseki-client cache --stats
```

Print cache statistics: mode, L1/L2 bytes used, hit/miss counts, errors,
metadata cache stats, and wipe count.

### cache --wipe

```
kiseki-client cache --wipe
```

Wipe all cached data (L1 + L2 + metadata). Zeroizes data before deletion
(I-CC2).

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

## kiseki-admin

Standalone remote administration CLI. Runs from an admin workstation
and connects to any Kiseki node via the REST API (port 9090). No server
dependencies are needed on the workstation.

Default endpoint: `KISEKI_ENDPOINT` env var, or `http://localhost:9090`.

### status

```
kiseki-admin --endpoint http://storage-node:9090 status
```

Cluster status summary: node count, Raft entries, gateway requests,
data written/read, and active connections.

Example output:

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

Node list with health badges and per-node metrics.

Example output:

```
NODE              STATUS    RAFT     REQUESTS  WRITTEN   READ      CONNS
10.0.0.1:9090     healthy   14,189   411       4.2 GB    2.7 GB    5
10.0.0.2:9090     healthy   14,189   412       4.2 GB    2.8 GB    5
10.0.0.3:9090     healthy   14,189   411       4.1 GB    2.7 GB    5
```

### events

```
kiseki-admin events [--severity error] [--hours 1]
```

Filtered event log. Optional `--severity` (info, warning, error,
critical) and `--hours` (default: 3).

Example output:

```
TIME      SEVERITY  CATEGORY  SOURCE    MESSAGE
12:34:56  ERROR     node      node-3    unreachable
12:35:12  ERROR     device    nvme0n1   CRC mismatch detected
```

### history

```
kiseki-admin history [--hours 3]
```

Metric history time series for the specified number of hours (default: 3).

### maintenance

```
kiseki-admin maintenance on
kiseki-admin maintenance off
```

Toggle cluster-wide maintenance mode. Enables read-only on all shards.
Write commands return a retriable error (I-O6).

### backup

```
kiseki-admin backup
```

Trigger a background backup operation (ADR-016).

### scrub

```
kiseki-admin scrub
```

Trigger a background data integrity scrub.

---

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | General error |
| 2 | Invalid arguments |
| 3 | Connection failure (server unreachable) |
| 4 | Authentication failure (mTLS) |
