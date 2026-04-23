# Troubleshooting

This guide covers common issues, diagnostic tools, and resolution
procedures for Kiseki clusters.

---

## Diagnostic tools

### Health endpoint

```bash
# Quick liveness check (returns "OK" or connection refused)
curl http://node1:9090/health
```

### Event log

The event log captures categorized diagnostic events in memory.
Query via the admin API:

```bash
# All events from the last 3 hours
curl http://node1:9090/ui/api/events

# Error events only
curl 'http://node1:9090/ui/api/events?severity=error'

# Critical events from the last 24 hours
curl 'http://node1:9090/ui/api/events?severity=critical&hours=24'

# Device-related events
curl 'http://node1:9090/ui/api/events?category=device'

# Raft events (elections, membership changes)
curl 'http://node1:9090/ui/api/events?category=raft'
```

### Node status

```bash
# Per-node metrics and health
curl http://node1:9090/ui/api/nodes

# Cluster summary
curl http://node1:9090/ui/api/cluster
```

### Structured logs

```bash
# Tail logs for errors (systemd)
journalctl -u kiseki-server -f --priority=err

# Search for specific errors in JSON logs
journalctl -u kiseki-server --output=json | jq 'select(.level == "ERROR")'

# Raft-specific logs
journalctl -u kiseki-server | grep kiseki_raft
```

---

## Common issues

### Connection refused on data-path port (9100)

**Symptoms**: Clients cannot connect. `curl http://node:9090/health`
returns OK but gRPC connections to port 9100 fail.

**Diagnosis**:

1. Verify the port is listening:
   ```bash
   ss -tlnp | grep 9100
   ```
2. Check firewall rules:
   ```bash
   iptables -L -n | grep 9100
   ```
3. Check the server logs for bind errors:
   ```bash
   journalctl -u kiseki-server | grep "bind\|listen\|9100"
   ```

**Common causes**:

- Port conflict: Another process is using port 9100.
- Bind address: `KISEKI_DATA_ADDR` is set to `127.0.0.1:9100` instead
  of `0.0.0.0:9100`.
- Firewall: Port 9100 is not open between nodes or to clients.

### mTLS authentication failures

**Symptoms**: `AuthenticationFailed` errors in logs. Clients receive
gRPC `UNAUTHENTICATED` (16) status.

**Diagnosis**:

```bash
# Verify certificate validity
openssl x509 -in /etc/kiseki/tls/server.crt -noout -dates -subject -issuer

# Verify certificate chain
openssl verify -CAfile /etc/kiseki/tls/ca.crt /etc/kiseki/tls/server.crt

# Test TLS handshake
openssl s_client -connect node1:9100 \
  -cert /etc/kiseki/tls/client.crt \
  -key /etc/kiseki/tls/client.key \
  -CAfile /etc/kiseki/tls/ca.crt
```

**Common causes**:

- Certificate expired: Renew the certificate.
- CA mismatch: Client and server certificates signed by different CAs.
- Missing SAN: Server certificate does not include the hostname or IP
  the client is connecting to.
- CRL revocation: Certificate revoked via `KISEKI_CRL_PATH`. Check the
  CRL:
  ```bash
  openssl crl -in /etc/kiseki/tls/crl.pem -text -noout
  ```
- Wrong OU: Tenant certificate has wrong OU, or admin certificate does
  not have `kiseki-admin` OU.

### Capacity full (ENOSPC)

**Symptoms**: Write operations return `PoolFull` errors. S3 PutObject
returns HTTP 507. NFS writes return EIO or ENOSPC.

**Diagnosis**:

```bash
# Check pool capacity
curl -s http://node1:9090/metrics | grep kiseki_pool_capacity

# Check system disk usage
df -h /var/lib/kiseki
```

**Resolution**:

1. **Add devices** to the pool to increase capacity.
2. **Rebalance** to distribute data more evenly:
   ```bash
   kiseki-server pool rebalance --pool-id fast-nvme
   ```
3. **Evacuate** devices from an over-full pool to a different pool
   (within the same device class).
4. **Delete data**: Remove compositions/objects to free space. GC runs
   periodically (default every 300 seconds).
5. **Adjust thresholds** if the defaults are too conservative for your
   deployment:
   ```bash
   kiseki-server pool set-thresholds --pool-id fast-nvme \
     --warning-pct 80 --critical-pct 90
   ```

### Metadata disk full (system partition)

**Symptoms**: Inline threshold drops to floor (128 bytes). Alert:
"system disk metadata usage exceeds hard limit." Raft may stall if the
system disk is completely full.

**Diagnosis**:

```bash
# Check system partition usage
df -h /var/lib/kiseki

# Check individual redb sizes
du -sh /var/lib/kiseki/raft/log.redb
du -sh /var/lib/kiseki/chunks/meta.redb
du -sh /var/lib/kiseki/small/objects.redb
```

**Resolution**:

1. The system automatically reduces the inline threshold to the floor
   (128 bytes) when the hard limit is exceeded (I-SF2).
2. Trigger Raft log compaction to reduce `raft/log.redb` size:
   ```bash
   kiseki-server compact
   ```
3. Run GC to clean up orphaned entries in `small/objects.redb`
   (I-SF6).
4. Consider migrating shards to nodes with larger system disks.
5. If the system partition is persistently undersized, upgrade to
   larger NVMe for the system RAID-1.

---

## Raft diagnostics

### Leader election issues

**Symptoms**: `ShardUnavailable` errors. Writes fail intermittently.

**Diagnosis**:

```bash
# Check shard health
kiseki-server shard health --shard-id shard-0001

# Check Raft events
curl 'http://node1:9090/ui/api/events?category=raft'

# Check election metrics
curl -s http://node1:9090/metrics | grep kiseki_raft
```

**Common causes**:

- **Network partition**: Raft peers cannot communicate. Check
  connectivity on port 9300 between all nodes.
- **Clock skew**: Large clock differences can cause election timeouts.
  Verify NTP synchronization. Nodes with `Unsync` clock quality are
  flagged (I-T6).
- **Disk latency**: HDD system disks cause 5-10ms fsync latency per
  Raft commit. Use NVMe or SSD for the system partition.

### Quorum loss

**Symptoms**: All writes fail. Reads may succeed (depending on
consistency model).

**Diagnosis**:

```bash
# Check how many nodes are reachable
for node in node1 node2 node3; do
  echo -n "$node: "
  curl -s http://$node:9090/health && echo "OK" || echo "DOWN"
done
```

**Resolution**:

- If one node is down (3-node cluster): The remaining 2 nodes form a
  majority. Raft continues. Repair or replace the failed node.
- If two nodes are down: Quorum is lost. See
  [Backup & Recovery](../admin/backup-recovery.md) for recovery
  procedures.

### Shard split stalls

**Symptoms**: Shard reports high delta count or throughput but split
does not complete.

**Diagnosis**:

```bash
kiseki-server shard info --shard-id shard-0001
```

**Resolution**:

- Verify the shard is not in maintenance mode (I-O6).
- Check if the cluster-wide concurrent migration limit is reached
  (I-SF4): `max(1, num_nodes / 10)`.
- Check the exponential backoff timer (I-SF4): Minimum 2 hours between
  placement changes per shard.
- Manually trigger a split if auto-split is not firing:
  ```bash
  kiseki-server shard split --shard-id shard-0001
  ```

---

## Device issues

### Integrity scrub

Trigger a manual integrity scrub to verify chunk data against EC parity:

```bash
# Scrub all devices
curl -X POST http://node1:9090/ui/api/ops/scrub

# Scrub a specific device
kiseki-server device scrub --device-id nvme-0001
```

The periodic scrub runs every 7 days by default (`scrub_interval_h`).

### SMART warnings

Automatic evacuation triggers when a device reports:

- SSD: SMART wear indicator > 90%.
- HDD: > 100 bad sectors.

Check device health:

```bash
kiseki-server device info --device-id nvme-0001
```

### Device evacuation

Monitor evacuation progress:

```bash
# List active repairs/evacuations
kiseki-server repair list

# Check device state
kiseki-server device info --device-id nvme-0001
```

Device state transitions: `Healthy -> Degraded -> Evacuating -> Failed -> Removed` (I-D2).

A device in `Evacuating` state can be cancelled:

```bash
kiseki-server device cancel-evacuation --device-id nvme-0001
```

`RemoveDevice` is rejected unless the device state is `Removed`
(post-evacuation) (I-D5).

---

## Key management issues

### Key manager unreachable

**Symptoms**: `KeyManagerUnavailable` errors. All chunk writes fail
cluster-wide (I-K12).

**Diagnosis**:

```bash
# Check key manager health
kiseki-server keymanager health

# Check connectivity from storage node
curl -s http://node1:9090/metrics | grep kms_reachability
```

**Resolution**:

- The key manager is a Raft-replicated HA service. If one node is down,
  the remaining majority continues serving.
- If the entire key manager cluster is unreachable, storage nodes use
  cached master keys (mlock'd in memory) for reads but cannot process
  new writes.
- Restore key manager connectivity as soon as possible.

### Tenant KMS unreachable

**Symptoms**: `TenantKmsUnreachable` errors for operations involving
the affected tenant. Other tenants are unaffected.

**Diagnosis**:

```bash
kiseki-server keymanager check-kms --tenant-id acme-corp
```

**Resolution**:

- Check network connectivity to the tenant's KMS endpoint.
- Check KMS credentials and certificate validity.
- The tenant admin is responsible for their KMS availability (I-K11).

### Crypto-shred verification

After a crypto-shred, verify that all clients have wiped their caches:

```bash
# Check crypto-shred count
curl -s http://node1:9090/metrics | grep kiseki_crypto_shred_total

# Check security events
curl 'http://node1:9090/ui/api/events?category=security'
```

---

## Gateway issues

### S3 errors

Common S3 error codes returned by the gateway:

| Error | Cause | Resolution |
|-------|-------|------------|
| 403 Forbidden | SigV4 authentication failure | Check access key/secret key. |
| 404 Not Found | Bucket or object does not exist | Verify namespace and key. |
| 507 Insufficient Storage | Pool full | Add capacity. See Capacity Full above. |
| 503 Service Unavailable | Raft quorum lost or maintenance mode | Wait for recovery or disable maintenance. |

### NFS errors

| Error | Cause | Resolution |
|-------|-------|------------|
| ESTALE | Shard split caused file handle invalidation | Retry the operation. |
| EIO | Internal error (chunk read failure, key manager unreachable) | Check server logs. |
| ENOSPC | Pool full | Add capacity. |
| EXDEV | Cross-shard rename (I-L8) | Use copy + delete instead. |
| ENOTSUP | Writable shared mmap (I-O8) | Use read/write instead of mmap for writes. |
