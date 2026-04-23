# Backup & Recovery

Kiseki's primary disaster recovery mechanism is federation (async
replication to a secondary site). External backup is additive and
optional, providing defense-in-depth for deployments that require it.

---

## Architecture overview

### Federation as primary DR

Federated-async replication to a secondary site is the recommended DR
strategy (ADR-016). Properties:

- **RPO**: Bounded by async replication lag (seconds to minutes).
- **RTO**: Secondary site is warm (has replicated data + tenant config);
  switchover requires KMS connectivity and control plane
  reconfiguration.
- **Data replication**: Ciphertext-only. No key material in the
  replication stream.

### What is replicated

| Component | Replicated? | Mechanism |
|-----------|-------------|-----------|
| Chunk data (ciphertext) | Yes | Async replication to peer site |
| Log deltas | Yes | Async replication of committed deltas |
| Control plane config | Yes | Federation config sync |
| Tenant KMS config | No | Same tenant KMS serves both sites |
| System master keys | No | Per-site system key manager |
| Audit log | Yes | Per-tenant audit shard replicated |

### External backup

Cluster admins can configure external backup targets (S3-compatible
object store). Backup data is encrypted with the system key at rest.

---

## Backup operations

### Creating a backup

```bash
# Via the admin dashboard
curl -X POST http://node1:9090/ui/api/ops/backup

# Via the StorageAdminService gRPC API
grpcurl node1:9100 kiseki.v1.StorageAdminService/CreateBackup
```

### Backup contents

Each backup snapshot contains:

1. **Per-shard metadata**: Raft log snapshots for each shard, capturing
   the delta history up to the snapshot point.
2. **Chunk extent manifests**: The `chunks/meta.redb` index mapping
   chunk IDs to device extents.
3. **Inline content**: The `small/objects.redb` database (small-file
   data below the inline threshold).
4. **Control plane state**: Tenant configuration, namespace mappings,
   quotas, compliance tags, federation peer registry.
5. **Key epoch metadata**: Key epoch records from
   `keys/epochs.redb` (key material itself is NOT included in backups;
   it is managed by the system key manager and tenant KMS
   independently).

All backup data is encrypted. No plaintext chunk data appears in backup
output. Backups reference chunk ciphertext on data devices by extent
coordinates, not by copying the raw ciphertext (which would require
reading and re-encrypting terabytes of data).

### Listing backups

```bash
grpcurl node1:9100 kiseki.v1.StorageAdminService/ListBackups
```

### Deleting a backup

```bash
grpcurl -d '{"backup_id": "backup-20260423-001"}' \
  node1:9100 kiseki.v1.StorageAdminService/DeleteBackup
```

---

## Retention policy

Backup retention is configurable per cluster. Defaults:

| Setting | Default | Description |
|---------|---------|-------------|
| Retention period | 7 days | Backups older than this are automatically deleted. |
| Maximum backups | 10 | Maximum number of retained backup snapshots. |
| Backup frequency | Daily | How often automatic backups are created (if enabled). |

Retention is enforced by a background task that runs on the Raft leader.
Deletion of expired backups is recorded in the cluster audit log.

---

## Recovery procedures

### Single node failure

**Recovery path**: Raft re-election + EC repair.

1. The Raft group detects the failed node and elects a new leader
   (if the failed node was leader).
2. EC repair automatically rebuilds chunk fragments that were on the
   failed node's devices.
3. RPO: 0 (committed data is on a majority of replicas). RTO: seconds
   to minutes.

No manual intervention required. Monitor the repair progress via:

```bash
grpcurl node1:9100 kiseki.v1.StorageAdminService/ListRepairs
```

### Multiple node failure (quorum maintained)

**Recovery path**: Raft reconfiguration + EC repair.

If the cluster still has a Raft majority (e.g., 2 of 3 nodes alive),
recovery is automatic:

1. Raft continues operating with the surviving majority.
2. EC repair rebuilds lost chunk fragments.
3. Deploy replacement nodes and add them to the cluster.

### Multiple node failure (quorum lost)

**Recovery path**: Manual Raft reconfiguration.

If the majority is lost (e.g., 2 of 3 nodes down), Raft cannot make
progress. Recovery requires manual intervention:

1. Identify the surviving node(s) with the most recent committed state.
2. Force a new Raft configuration with the surviving node(s) as the
   initial voter set.
3. Deploy replacement nodes and add them as learners.
4. Promote learners to voters once they catch up.

**Data loss risk**: Deltas committed on the failed majority but not yet
replicated to the surviving minority may be lost.

### Full site failure (with federation)

**Recovery path**: Failover to federated peer.

1. Redirect clients to the secondary site (DNS, load balancer, or
   manual reconfiguration).
2. The secondary site has replicated chunk data, log deltas, and
   control plane config.
3. Tenant KMS must be reachable from the secondary site (same KMS
   serves both sites).
4. The secondary site's system key manager has its own master keys,
   but tenant data is accessible because tenant KEKs come from the
   shared tenant KMS.

RPO: Replication lag. RTO: Minutes to hours (depends on control plane
reconfiguration speed).

### Full site failure (without federation)

**Recovery path**: Restore from external backup.

1. Deploy a new cluster.
2. Restore the backup snapshot to the new cluster.
3. The system key manager on the new cluster generates new system master
   keys.
4. Tenant KMS must be reconfigured to point to the new cluster.
5. Re-wrap all envelopes with new system master keys.

RPO: Time since last backup. RTO: Hours (depends on data volume).

### Tenant KMS loss

**Unrecoverable** (I-K11). If the tenant loses their KMS and has no
backup of their KEK material, all data encrypted under those keys is
permanently unreadable. Kiseki documents this requirement but provides
no system-side escrow. The tenant controls and is responsible for their
keys.

---

## Recovery summary

| Scenario | Recovery path | RPO | RTO |
|----------|---------------|-----|-----|
| Single node loss | Raft re-election + EC repair | 0 | Seconds-minutes |
| Multiple node loss (quorum held) | Raft reconfiguration + EC repair | 0 | Minutes |
| Multiple node loss (quorum lost) | Manual Raft reconfig | Possible delta loss | Minutes-hours |
| Full site loss (with federation) | Failover to peer | Replication lag | Minutes-hours |
| Full site loss (no federation) | Restore from backup | Backup lag | Hours |
| Tenant KMS loss | Unrecoverable | N/A | N/A |

---

## Limitations

- **No point-in-time restore.** Backups are snapshots, not continuous
  journals. Recovery restores the cluster to the state at the snapshot
  time. Deltas committed after the snapshot are lost unless federation
  has replicated them.

- **Backup does not include key material.** System master keys and
  tenant KEKs are managed by their respective key managers. Backup and
  recovery of key material is the responsibility of the key manager
  operator (cluster admin for system keys, tenant admin for tenant
  KEKs).

- **Chunk ciphertext is referenced, not copied.** Backup manifests
  reference chunk extents on data devices. If data devices are
  destroyed, the chunk ciphertext is lost. Federation replicates the
  actual ciphertext to a secondary site, which is why it is the
  primary DR mechanism.

- **Cross-site backup requires federation.** There is no built-in
  mechanism to ship backup snapshots to a remote site outside of the
  federation framework. For cross-site backup without federation,
  operators must arrange their own transport of backup snapshots.
