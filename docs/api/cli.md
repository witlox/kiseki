# CLI Reference

Kiseki provides two binaries with CLI interfaces: `kiseki-server` (which
doubles as the admin CLI) and `kiseki-client` (native client with staging
commands).

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

### pool list

```
kiseki-server pool list
```

List all affinity pools with their device class, capacity, utilization,
EC parameters, and health thresholds.

### device list

```
kiseki-server device list
```

List all storage devices with their state (Healthy, Degraded, Evacuating,
Failed, Removed), device class, capacity, and pool membership.

### shard list

```
kiseki-server shard list
```

List all shards with their Raft state (leader node, voter set), delta
count, and maintenance status.

### maintenance on

```
kiseki-server maintenance on
```

Enable maintenance mode. Sets all shards to read-only. Write commands are
rejected with a retriable error. Shard splits, compaction, and GC for
in-progress operations continue but no new triggers fire from write
pressure (I-O6).

### maintenance off

```
kiseki-server maintenance off
```

Disable maintenance mode. Shards resume accepting writes.

---

## kiseki-client

The native client binary provides FUSE mount and staging commands.

### FUSE mount

```
kiseki-client mount --mountpoint /mnt/kiseki --endpoint host:9100
```

Mount Kiseki as a FUSE filesystem at the specified mountpoint. The client
discovers shards and views via the data fabric (ADR-008).

### stage --dataset

```
kiseki-client stage --dataset /path/to/data
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
kiseki-client stage --release
```

Release a staged dataset. Unpins cached chunks, making them eligible for
LRU eviction. To pick up updates from canonical, release and re-stage.

---

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | General error |
| 2 | Invalid arguments |
| 3 | Connection failure (server unreachable) |
| 4 | Authentication failure (mTLS) |
