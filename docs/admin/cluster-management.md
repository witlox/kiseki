# Cluster Management

This guide covers day-to-day cluster operations: adding and removing
nodes, managing shards and pools, maintenance mode, and schema
migration.

---

## Node management

Kiseki uses Raft consensus groups for metadata and log replication.
Adding or removing nodes is done through Raft membership changes, which
are zero-downtime and zero-data-loss operations.

### Adding a node

1. Deploy `kiseki-server` on the new host with a unique `KISEKI_NODE_ID`
   and the full `KISEKI_RAFT_PEERS` list (including the new node).

2. Start the service. The node registers with the cluster and begins
   receiving Raft log entries as a learner.

3. Promote the node to a voter once it has caught up:

   ```bash
   # Via the StorageAdminService gRPC API
   grpcurl -d '{"node_id": 4}' node1:9100 kiseki.v1.StorageAdminService/AddNode
   ```

4. The node receives shard assignments and begins participating in Raft
   elections and commit quorums.

**Catch-up requirement (I-SF3)**: A learner must fully catch up with the
leader's committed index before being promoted to voter. The old voter
remains in membership until the new voter is promoted.

### Removing a node

1. Drain the node to migrate its shard assignments to other nodes:

   ```bash
   grpcurl -d '{"node_id": 4}' node1:9100 kiseki.v1.StorageAdminService/DrainNode
   ```

2. Wait for all shards to be migrated. The drain operation uses Raft
   membership changes (add learner on target, promote, demote source)
   for each shard hosted on the node.

3. Once drained, remove the node from the cluster:

   ```bash
   grpcurl -d '{"node_id": 4}' node1:9100 kiseki.v1.StorageAdminService/RemoveNode
   ```

4. Stop the `kiseki-server` process and decommission the hardware.

**Safety**: Removing a node without draining first triggers automatic
shard repair, but this is reactive rather than proactive. Always drain
first for orderly removal.

### Cluster sizing

- **Minimum**: 3 nodes (Raft requires a majority quorum; 2-of-3 for
  writes).
- **Recommended**: 5+ nodes for production. Tolerates 2 simultaneous
  node failures.
- **Key manager**: Deploy on a dedicated 3-5 node HA cluster, separate
  from storage nodes. The system key manager must be at least as
  available as the log (I-K12).

---

## Shard management

Shards are the smallest unit of totally-ordered deltas, backed by one
Raft group. They split automatically when size or throughput thresholds
are exceeded (I-L6).

### Viewing shard status

```bash
# List all shards
grpcurl node1:9100 kiseki.v1.StorageAdminService/ListShards

# Get details for a specific shard
grpcurl -d '{"shard_id": "shard-0001"}' \
  node1:9100 kiseki.v1.StorageAdminService/GetShard

# Check shard health
grpcurl -d '{"shard_id": "shard-0001"}' \
  node1:9100 kiseki.v1.StorageAdminService/GetShardHealth
```

### Automatic shard split

Shards have a hard ceiling triggering mandatory split (I-L6). The
ceiling is configurable across three dimensions:

- **Delta count**: Maximum number of deltas in a shard.
- **Byte size**: Maximum total size of shard data.
- **Write throughput**: Maximum sustained write rate.

Any dimension exceeding its ceiling forces a split. The split operation:

1. Selects a split boundary (key range partition).
2. Creates a new shard for the upper range.
3. Continues accepting writes during the split (I-O1).
4. Notifies the control plane, views, and clients of the new shard
   topology.

### Manual shard split

```bash
grpcurl -d '{"shard_id": "shard-0001", "boundary": "..."}' \
  node1:9100 kiseki.v1.StorageAdminService/SplitShard
```

### Shard maintenance mode

Set a shard to read-only for maintenance operations:

```bash
# Enable maintenance mode (writes rejected with retriable error)
grpcurl -d '{"shard_id": "shard-0001", "enabled": true}' \
  node1:9100 kiseki.v1.StorageAdminService/SetShardMaintenance
```

During maintenance mode (I-O6):

- Write commands are rejected with a retriable error.
- Read operations continue normally.
- In-progress compaction and GC continue but no new triggers fire from
  write pressure.
- Shard splits do not initiate.

### Cross-shard operations

Cross-shard rename returns `EXDEV` (I-L8). Shards are independent
consensus domains with no two-phase commit. Applications must handle
cross-shard moves via copy + delete.

---

## Pool management

Affinity pools are groups of storage devices sharing a device class.
Pools are the unit of capacity management and durability policy.

### Viewing pools

```bash
# List all pools
grpcurl node1:9100 kiseki.v1.StorageAdminService/ListPools

# Get pool details including capacity and health
grpcurl -d '{"pool_id": "fast-nvme"}' \
  node1:9100 kiseki.v1.StorageAdminService/PoolStatus
```

### Creating a pool

```bash
grpcurl -d '{
  "pool_id": "fast-nvme",
  "device_class": "NvmeU2",
  "ec_data_chunks": 4,
  "ec_parity_chunks": 2
}' node1:9100 kiseki.v1.StorageAdminService/CreatePool
```

**Important**: EC parameters (`ec_data_chunks`, `ec_parity_chunks`) are
immutable per pool after creation (I-C6). Changing them requires
creating a new pool and migrating data via `ReencodePool`.

### Setting pool durability

```bash
# Switch pool durability strategy (applies to new chunks only)
grpcurl -d '{
  "pool_id": "fast-nvme",
  "ec_data_chunks": 4,
  "ec_parity_chunks": 2
}' node1:9100 kiseki.v1.StorageAdminService/SetPoolDurability
```

Existing chunks retain their original EC config. Re-encoding requires
an explicit `ReencodePool` RPC.

### Rebalancing a pool

Rebalance distributes data evenly across devices in a pool:

```bash
# Start rebalance
grpcurl -d '{"pool_id": "fast-nvme"}' \
  node1:9100 kiseki.v1.StorageAdminService/RebalancePool

# Cancel a running rebalance
grpcurl -d '{"pool_id": "fast-nvme"}' \
  node1:9100 kiseki.v1.StorageAdminService/CancelRebalance
```

Rebalance runs at the configured `rebalance_rate_mb_s` (default
50 MB/s) to limit impact on production traffic.

### Device evacuation

When a device shows signs of failure (SMART wear > 90% for SSD, > 100
bad sectors for HDD), automatic evacuation is triggered (I-D3).
Evacuation can also be initiated manually:

```bash
# Start evacuation
grpcurl -d '{"device_id": "nvme-0001"}' \
  node1:9100 kiseki.v1.StorageAdminService/EvacuateDevice

# Cancel evacuation
grpcurl -d '{"device_id": "nvme-0001"}' \
  node1:9100 kiseki.v1.StorageAdminService/CancelEvacuation
```

Evacuation migrates all chunks from the device to other devices in the
same pool. Device removal (`RemoveDevice`) is rejected unless the device
state is `Removed` (post-evacuation) (I-D5).

Device state transitions: `Healthy -> Degraded -> Evacuating -> Failed -> Removed`.
All transitions are recorded in the audit log (I-D2).

### Pool capacity thresholds

Pool writes are rejected when the pool reaches the Critical threshold
(I-C5). Thresholds vary by device class to account for SSD/NVMe GC
pressure at high fill levels:

| State | NVMe/SSD | HDD | Behavior |
|-------|----------|-----|----------|
| Healthy | 0-75% | 0-85% | Normal writes |
| Warning | 75-85% | 85-92% | Log warning, emit telemetry |
| Critical | 85-92% | 92-97% | Reject new placements |
| ReadOnly | 92-97% | 97-99% | In-flight writes drain, no new writes |
| Full | 97-100% | 99-100% | ENOSPC to clients |

Pool redirection stays within the same device class only. ENOSPC is
returned when the pool is Full.

---

## Maintenance mode

Cluster-wide or per-shard maintenance mode sets the cluster (or specific
shards) to read-only (I-O6).

### Enabling cluster-wide maintenance

```bash
# Via the admin dashboard
curl -X POST http://node1:9090/ui/api/ops/maintenance \
  -H 'Content-Type: application/json' \
  -d '{"enabled": true}'

# Via the control plane
grpcurl -d '{"enabled": true}' \
  node1:9100 kiseki.v1.ControlService/SetMaintenanceMode
```

### Maintenance mode behavior

- All write commands are rejected with a retriable error code
  (`MaintenanceMode`). Clients can retry after maintenance ends.
- Read operations continue normally.
- In-progress compaction and GC complete their current run.
- New shard splits, compaction triggers, and GC triggers from write
  pressure are suppressed.
- Maintenance mode is the prerequisite for:
  - Schema migration on upgrade
  - Inline threshold increase (optional migration of small chunked files
    back to inline)
  - Full cluster re-encryption

### Disabling maintenance

```bash
curl -X POST http://node1:9090/ui/api/ops/maintenance \
  -H 'Content-Type: application/json' \
  -d '{"enabled": false}'
```

Writes resume immediately. Clients that were retrying will succeed on
their next attempt.

---

## Schema migration on upgrade

Kiseki uses versioned on-disk formats. Upgrades that change the schema
follow this procedure:

1. **Read the release notes** for migration requirements. Not every
   release requires migration.

2. **Enable maintenance mode** on the cluster to prevent writes during
   migration.

3. **Stop all nodes** in the cluster.

4. **Upgrade the binaries** on all nodes (`kiseki-server`,
   `kiseki-keyserver`, `kiseki-client-fuse`).

5. **Start nodes one at a time.** On startup, each node detects the old
   schema version (via the superblock on each data device and the redb
   metadata version) and applies migration automatically.

6. **Verify migration** by checking the admin dashboard and node logs.

7. **Disable maintenance mode** to resume normal operations.

### Rolling upgrades

For minor releases that do not change the on-disk format, rolling
upgrades are supported:

1. Drain a node (`DrainNode`).
2. Stop the node.
3. Upgrade the binary.
4. Start the node.
5. Wait for it to rejoin and catch up.
6. Repeat for the next node.

The superblock on each data device carries a format version (ADR-029).
Format version mismatches are detected at device open and handled by
the migration path.
