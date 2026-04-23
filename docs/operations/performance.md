# Performance Tuning

Kiseki is designed for HPC and AI workloads running at 200+ Gbps per
NIC. This guide covers tuning levers for maximizing throughput and
minimizing latency.

---

## Transport selection

The transport layer abstracts the network fabric. Kiseki automatically
selects the best available transport, but manual override is possible.

### Transport hierarchy (fastest to slowest)

| Transport | Typical bandwidth | Latency | Feature flag | Notes |
|-----------|------------------|---------|--------------|-------|
| CXI (HPE Slingshot) | 200 Gbps | <1 us | `kiseki-transport/cxi` | Requires libfabric with CXI provider. CSCS/Alps native. |
| InfiniBand verbs | 100-400 Gbps | 1-2 us | `kiseki-transport/verbs` | Requires RDMA-capable NICs and verbs libraries. |
| RoCE v2 | 25-100 Gbps | 2-5 us | `kiseki-transport/verbs` | RDMA over Converged Ethernet. Requires lossless fabric (PFC/ECN). |
| TCP | 10-100 Gbps | 50-200 us | (always available) | Fallback. Uses kernel TCP with TLS. |

### Enabling high-performance transports

```bash
# Build with CXI support (requires libfabric development headers)
cargo build --release --features kiseki-transport/cxi

# Build with RDMA verbs support (requires rdma-core)
cargo build --release --features kiseki-transport/verbs
```

The client automatically detects available transports and selects the
fastest one. Override with:

```bash
# Force TCP transport (e.g., for debugging)
KISEKI_TRANSPORT=tcp kiseki-client-fuse --mountpoint /mnt/kiseki
```

### Transport tuning

- **Connection pooling**: The transport layer maintains a pool of
  connections per peer. Pool size adapts to workload.
- **Keepalive**: Connections are kept alive to avoid handshake overhead.
  Configure via `KISEKI_TRANSPORT_KEEPALIVE_MS`.
- **Zero-copy**: CXI and verbs transports use zero-copy DMA where
  possible.

---

## NUMA pinning

For multi-socket servers, NUMA-aware placement is critical for avoiding
cross-socket memory traffic.

### Recommendations

- **Pin kiseki-server** to the NUMA node closest to the NIC:
  ```bash
  numactl --cpunodebind=0 --membind=0 kiseki-server
  ```
- **Pin NVMe interrupts** to the same NUMA node:
  ```bash
  echo 0 > /proc/irq/<irq>/smp_affinity_list
  ```
- **Pin data devices** to the NUMA node closest to their PCIe root
  complex.

### systemd integration

```ini
[Service]
# Pin to NUMA node 0
ExecStart=/usr/bin/numactl --cpunodebind=0 --membind=0 /usr/local/bin/kiseki-server
```

### Verification

```bash
# Check NUMA topology
numactl --hardware

# Check NIC NUMA node
cat /sys/class/net/eth0/device/numa_node

# Check NVMe NUMA node
cat /sys/block/nvme0n1/device/numa_node
```

---

## Erasure coding parameters

EC parameters control the trade-off between storage overhead, repair
bandwidth, and read performance.

### Common configurations

| Config | Data | Parity | Overhead | Fault tolerance | Use case |
|--------|------|--------|----------|-----------------|----------|
| 4+2 | 4 | 2 | 50% | 2 device failures | Default for NVMe. Good balance. |
| 8+3 | 8 | 3 | 37.5% | 3 device failures | Large HDD pools. Lower overhead. |
| 4+1 | 4 | 1 | 25% | 1 device failure | Low-criticality data. Minimum overhead. |
| 2+2 | 2 | 2 | 100% | 2 device failures | Small pools (<6 devices). High redundancy. |

### Performance implications

- **Read amplification**: Reading a chunk requires reading
  `data_chunks` fragments. More data chunks = more read I/O.
- **Write amplification**: Writing a chunk requires writing
  `data_chunks + parity_chunks` fragments.
- **Repair bandwidth**: Repairing a lost fragment requires reading
  `data_chunks` fragments and writing 1. Higher `data_chunks` = more
  repair bandwidth.
- **Minimum pool size**: The pool must have at least
  `data_chunks + parity_chunks` devices.

EC parameters are immutable per pool after creation (I-C6). Choose
carefully. Changing requires creating a new pool and migrating data.

---

## Inline threshold (ADR-030)

The inline threshold determines whether small files are stored in the
metadata tier (NVMe, redb) or the data tier (block device extents).

### Tuning the threshold

The system automatically adjusts the threshold per-shard based on
system disk capacity (I-SF1, I-SF2). Manual adjustment:

```bash
# Set cluster-wide default for new shards
kiseki-server tuning set --inline-threshold-bytes 8192
```

### Trade-offs

| Threshold | Metadata tier impact | Data tier impact | Latency |
|-----------|---------------------|------------------|---------|
| 128 B (floor) | Minimal metadata growth | All files in chunks | Higher for tiny files |
| 4 KB (default) | Moderate growth | Small files inline | Lower for small files |
| 64 KB (ceiling) | Large growth | More inline data | Lowest for small files |

### Monitoring

```bash
# Check system disk usage
df -h /var/lib/kiseki

# Check per-store sizes
du -sh /var/lib/kiseki/small/objects.redb
du -sh /var/lib/kiseki/raft/log.redb
```

The Raft inline throughput guard (I-SF7) automatically reduces the
threshold to the floor if inline write rate exceeds
`KISEKI_RAFT_INLINE_MBPS` (default 10 MB/s per shard). This prevents
inline data from starving metadata-only Raft operations during write
storms.

---

## Cache tuning (ADR-031)

### L1 cache (in-memory)

The L1 cache holds decrypted plaintext chunks in process memory.

| Parameter | Default | Recommendation |
|-----------|---------|----------------|
| `KISEKI_CACHE_L1_MAX` | 1 GB | Set to 10-25% of available process memory. AI training with large datasets: increase. Memory-constrained compute: decrease. |

### L2 cache (local NVMe)

The L2 cache uses local NVMe on compute nodes.

| Parameter | Default | Recommendation |
|-----------|---------|----------------|
| `KISEKI_CACHE_L2_MAX` | 100 GB | Set based on available NVMe capacity. Training datasets: size to fit the working set. Inference: size to fit model weights. |

### Metadata TTL

| Parameter | Default | Recommendation |
|-----------|---------|----------------|
| `KISEKI_CACHE_META_TTL_MS` | 5000 (5s) | Read-heavy workloads: increase for fewer metadata fetches. Low-latency requirements: decrease for fresher data. POSIX close-to-open consistency: 0 (no caching). |

### Cache mode selection

| Workload | Recommended mode | Rationale |
|----------|-----------------|-----------|
| AI training (epoch reuse) | `pinned` | Dataset is re-read every epoch. Pin to avoid refetching. |
| AI inference | `organic` | Model weights are hot, prompts rotate. LRU works well. |
| HPC checkpoint/restart | `bypass` | Checkpoints are write-heavy. Caching checkpoints wastes NVMe. |
| Climate/weather staging | `pinned` | Boundary conditions staged once, read many times. |
| Interactive analysis | `organic` | Mixed access patterns. LRU adapts. |

### Staging for training workloads

Pre-stage datasets before training begins to avoid cold-start latency:

```bash
# Slurm prolog script
kiseki-client-fuse --stage /datasets/imagenet --mountpoint /mnt/kiseki
export KISEKI_CACHE_POOL_ID=$(cat /var/cache/kiseki/pool_id)

# Workload picks up the staged cache via KISEKI_CACHE_POOL_ID
srun --export=ALL python train.py
```

---

## Raft tuning

### Snapshot interval

```bash
kiseki-server tuning set --raft-snapshot-interval 10000
```

- **Lower values** (1000-5000): More frequent snapshots. Faster
  catch-up for new nodes. More I/O.
- **Higher values** (50000-100000): Less snapshot overhead. Slower
  catch-up.

### Compaction rate

```bash
kiseki-server tuning set --compaction-rate-mb-s 200
```

Higher compaction rate reduces Raft log size faster but consumes more
I/O bandwidth.

### View materialization poll interval

```bash
kiseki-server tuning set --stream-proc-poll-ms 50
```

Lower poll interval reduces view staleness but increases CPU usage.

---

## Benchmark harness

Kiseki includes a transport benchmark for measuring raw fabric
throughput:

```bash
# Run transport benchmarks (if available)
tests/hw/run_transport_bench.sh
```

### What it measures

- **Bandwidth**: Sequential read/write throughput per transport.
- **Latency**: Round-trip latency (p50, p99, p999) per transport.
- **IOPS**: Random read/write IOPS per transport.
- **Concurrency**: Throughput scaling with connection count.

### Interpreting results

| Metric | Good (CXI) | Good (TCP) | Action if below |
|--------|-----------|------------|-----------------|
| Bandwidth | >150 Gbps | >50 Gbps | Check NIC config, MTU, NUMA pinning |
| Latency p99 | <10 us | <500 us | Check CPU frequency, interrupt coalescing |
| IOPS (4K random) | >1M | >100K | Check NVMe config, queue depth |

---

## System tuning checklist

### Kernel parameters

```bash
# Increase maximum open files
echo "fs.file-max = 1048576" >> /etc/sysctl.conf

# Increase socket buffer sizes for high-bandwidth transports
echo "net.core.rmem_max = 67108864" >> /etc/sysctl.conf
echo "net.core.wmem_max = 67108864" >> /etc/sysctl.conf
echo "net.ipv4.tcp_rmem = 4096 87380 67108864" >> /etc/sysctl.conf
echo "net.ipv4.tcp_wmem = 4096 65536 67108864" >> /etc/sysctl.conf

# Disable transparent hugepages (can cause latency spikes)
echo never > /sys/kernel/mm/transparent_hugepage/enabled
```

### NVMe tuning

```bash
# Set I/O scheduler to none (best for NVMe)
echo none > /sys/block/nvme0n1/queue/scheduler

# Increase queue depth
echo 1024 > /sys/block/nvme0n1/queue/nr_requests
```

### Process limits

```ini
# /etc/security/limits.d/kiseki.conf
kiseki  soft  nofile  1048576
kiseki  hard  nofile  1048576
kiseki  soft  memlock unlimited
kiseki  hard  memlock unlimited
```
