# Client Cache & Staging

The client-side cache (ADR-031) eliminates repeated data transfers
across the storage fabric by caching decrypted plaintext chunks on
compute-node local NVMe. It is a library-level module in
`kiseki-client`, shared across all access modes: FUSE, FFI, Python, and
native Rust.

## Architecture

```
canonical (fabric) -> decrypt -> cache store (NVMe) -> serve to caller
                                     ^
                           cache hit path (no fabric, no decrypt)
```

### Two-Tier Storage

| Tier | Backing | Capacity | Purpose |
|------|---------|----------|---------|
| L1 (Hot) | In-memory `HashMap` | 256 MB default | Sub-microsecond hits for active working set |
| L2 (Warm) | Local NVMe files | 50 GB default | Large capacity for datasets and model weights |

Read path: L1 -> L2 (with CRC32 verification) -> canonical (decrypt +
SHA-256 verify + store in L1/L2).

L2 files are organized per-process with isolated cache pools:

```
$KISEKI_CACHE_DIR/
  <tenant_id_hex>/
    <pool_id>/                  <- per-process pool (128-bit CSPRNG)
      chunks/
        <prefix>/
          <chunk_id_hex>        <- plaintext + CRC32 trailer
      meta/
        file_chunks.db
      staging/
        <dataset_id>.manifest
      pool.lock                 <- flock proves process is alive
```

Each client process creates its own pool directory. Multiple concurrent
same-tenant processes on the same node have fully independent pools with
no contention.

### Security Model

The cache stores decrypted plaintext on local NVMe. This is acceptable
because:

- The compute node already holds decrypted data in process memory
  (computation requires plaintext)
- L2 NVMe is local to the compute node, same trust domain as process
  memory
- L2 is ephemeral -- wiped on process exit and on long disconnect
- All cached data is overwritten with zeros (`zeroize`) before
  deallocation or eviction
- File permissions are `0600`, owned by the process UID
- Orphaned pools from crashes are cleaned by the `kiseki-cache-scrub`
  service

## Cache Modes

Three modes are available, selected per client instance at session
establishment.

### Pinned Mode

For workloads that declare their dataset upfront: training runs (epoch
reuse), inference (model weights), climate simulations (boundary
conditions).

- Chunks are retained against eviction until explicit `release()`
- Populated via the staging API or on first access
- Staging captures a point-in-time snapshot; canonical updates do not
  invalidate pinned data
- Capacity bounded by `max_cache_bytes`; staging beyond capacity returns
  `CacheCapacityExceeded`

### Organic Mode

Default for mixed workloads. LRU with usage-weighted retention.

- Chunks cached on first read, evicted when capacity is reached
- Frequently accessed chunks promoted to L1
- L2 eviction: LRU by last-access timestamp, weighted by access count
  (chunks accessed N times survive N eviction rounds)
- Metadata cache with configurable TTL (default 5 seconds)

### Bypass Mode

For workloads that do not benefit from caching: streaming ingest,
one-shot scans, checkpoint writes.

- All reads go directly to canonical
- No L1 or L2 storage consumed
- Zero overhead beyond mode selection

## Staging API

Client-local operation for pre-populating the cache in pinned mode.
Pull-based -- the client fetches from canonical.

### CLI

```bash
# Stage a dataset
kiseki-client stage --dataset /training/imagenet

# Stage in daemon mode (for Slurm prolog)
POOL_ID=$(kiseki-client stage --dataset /training/imagenet --daemon)

# Check staging status
kiseki-client stage --status

# Release a dataset
kiseki-client stage --release /training/imagenet

# Release all
kiseki-client stage --release-all
```

### Rust API

```rust
let result = cache_manager.stage("/training/imagenet").await?;
let datasets = cache_manager.stage_status();
cache_manager.release("/training/imagenet");
cache_manager.release_all();
```

### Python API

```python
client.stage("/training/imagenet")
paths = client.stage_status()
client.release("/training/imagenet")
client.release_all()
```

### C FFI

```c
kiseki_stage(handle, "/training/imagenet", timeout_secs);
kiseki_stage_status(handle, &status);
kiseki_release(handle, "/training/imagenet");
```

### Staging Flow

1. Resolve `namespace_path` to compositions via canonical. For directory
   paths, recursively enumerate all files up to `max_staging_depth` (10)
   and `max_staging_files` (100,000).
2. Extract full chunk list from all resolved compositions.
3. For each chunk not already in L2: fetch from canonical, decrypt,
   verify content-address (SHA-256), store in L2 with CRC32 trailer and
   pinned retention.
4. Write a staging manifest listing all compositions and chunk IDs.
5. Report progress (chunks staged / total, bytes, elapsed).

Staging is **idempotent** -- re-staging an already-staged dataset is a
no-op. Partial staging (interrupted) can be resumed by re-running the
command.

## Slurm Integration

### Staging Handoff

The staging CLI creates a cache pool and holds its `pool.lock` flock.
The workload process adopts the pool instead of creating a new one:

1. **Prolog**: staging CLI fetches chunks in daemon mode, outputs
   `pool_id`.
2. **Workload**: sets `KISEKI_CACHE_POOL_ID=<pool_id>`, starts, adopts
   the existing pool, takes over the flock.
3. **Staging daemon**: detects flock loss, exits cleanly.

### Prolog Script

```bash
#!/bin/bash
# prolog.sh -- run before the job starts

POOL_ID=$(kiseki-client stage --dataset /training/imagenet --daemon)
echo "export KISEKI_CACHE_POOL_ID=$POOL_ID" >> $SLURM_EXPORT_FILE
```

### Epilog Script

```bash
#!/bin/bash
# epilog.sh -- run after the job completes

kiseki-client stage --release-all --pool $KISEKI_CACHE_POOL_ID
```

### Lattice Integration

Lattice injects `KISEKI_CACHE_POOL_ID` into the workload environment
after parallel staging completes across the node set. It queries
`stage --status` to verify readiness before launching the workload.

## Policy Hierarchy

Cache policy follows the same distribution mechanism as quotas, using
the existing `TenantConfig` structure.

```
cluster default -> org override -> project override -> workload override
                                                         -> session selection
```

Each level narrows (never broadens) the parent's settings.

### Policy Attributes

| Attribute | Type | Admin levels | Client selectable | Default |
|-----------|------|-------------|-------------------|---------|
| `cache_enabled` | bool | cluster, org, project, workload | No | `true` |
| `allowed_modes` | set | cluster, org | No | {pinned, organic, bypass} |
| `max_cache_bytes` | u64 | cluster, org, workload | Up to ceiling | 50 GB |
| `max_node_cache_bytes` | u64 | cluster | No | 80% of cache FS |
| `metadata_ttl_ms` | u64 | cluster, org | Up to ceiling | 5000 |
| `max_disconnect_seconds` | u64 | cluster | No | 300 |
| `key_health_interval_ms` | u64 | cluster | No | 30000 |
| `staging_enabled` | bool | cluster, org | No | `true` |
| `mode` | enum | workload (default) | Yes (within allowed) | organic |

### Policy Resolution

At session establishment, the client resolves its effective policy
through multiple paths:

1. **Primary**: `GetCachePolicy` RPC on the data-path gRPC channel to
   any storage node. No gateway or control plane access required.
2. **Secondary**: gateway's locally-cached `TenantConfig`.
3. **Stale tolerance**: last-known policy persisted in the L2 pool
   directory (`policy.json`).
4. **Fallback**: conservative defaults (organic mode, 10 GB max, 5s TTL).

Policy changes apply to new sessions only. Active sessions continue
under the policy effective at session establishment.

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `KISEKI_CACHE_MODE` | Cache mode | `organic` |
| `KISEKI_CACHE_DIR` | L2 cache directory | `/tmp/kiseki-cache` |
| `KISEKI_CACHE_L1_MAX` | L1 memory max bytes | 256 MB |
| `KISEKI_CACHE_L2_MAX` | L2 NVMe max bytes | 50 GB |
| `KISEKI_CACHE_META_TTL_MS` | Metadata TTL (ms) | 5000 |
| `KISEKI_CACHE_POOL_ID` | Adopt existing pool | (none) |

### Mount Options (FUSE)

```bash
kiseki-client-fuse mount /mnt/kiseki \
    -o cache_mode=pinned \
    -o cache_dir=/local-nvme/kiseki \
    -o cache_l2_max=100G
```

### API (Rust)

```rust
let config = CacheConfig {
    mode: CacheMode::Pinned,
    cache_dir: PathBuf::from("/local-nvme/kiseki"),
    max_cache_bytes: 100 * 1024 * 1024 * 1024,
    metadata_ttl: Duration::from_secs(5),
    ..CacheConfig::default()
};
```

### API (Python)

```python
client = kiseki.Client(
    cache_mode="pinned",
    cache_dir="/local-nvme/kiseki",
    cache_l2_max=100 * 1024**3,
)
```

Priority: API/mount options > environment variables > policy defaults.
All client-set values are clamped to policy ceilings.

## Cache Invalidation

### Metadata

TTL-based only. No push invalidation from canonical. The metadata TTL
(default 5 seconds) is the sole freshness mechanism and the upper bound
on read staleness.

Write-through: when the client writes a file, the local metadata cache
is updated immediately, providing read-your-writes consistency within a
single process.

### Crypto-Shred

When a tenant's KEK is destroyed, all cached plaintext for that tenant
must be wiped. Detection via three paths:

1. **Periodic key health check** (default every 30 seconds) -- primary.
2. **Advisory channel notification** -- fast path, best-effort.
3. **KMS error on next operation** -- tertiary.

Maximum detection latency: `min(key_health_interval,
max_disconnect_seconds)` = 30 seconds by default.

### Disconnect

If the client cannot reach any canonical endpoint for
`max_disconnect_seconds` (default 300 seconds), the entire cache is
wiped. Background heartbeat RPCs (every 60 seconds) maintain the
disconnect timer.

## Capacity Management

| Limit | Scope | Default | Enforcement |
|-------|-------|---------|-------------|
| `max_memory_bytes` (L1) | Per-process | 256 MB | Strict LRU eviction |
| `max_cache_bytes` (L2) | Per-process | 50 GB | LRU (organic), reject (pinned) |
| `max_node_cache_bytes` | Per-node | 80% of cache FS | Cooperative check before L2 insert |
| Disk pressure backstop | Per-node | 90% utilization | Hard backstop |

Pinned chunks are never evicted by organic LRU. Organic eviction
considers only non-pinned chunks.

## Crash Recovery

- **On process start**: the client scans for orphaned cache pools
  (those whose `pool.lock` has no live `flock` holder), zeroizes their
  contents, and deletes them.
- **`kiseki-cache-scrub` service**: a systemd one-shot (or cron job)
  that runs on node boot and every 60 seconds, covering the case where
  no subsequent Kiseki process starts on the node after a crash.
