# FUSE Mount

The Kiseki native client provides a FUSE mount that exposes the
distributed storage as a local filesystem on compute nodes. Unlike the
NFS gateway, the FUSE client runs in the workload's process space and
performs client-side encryption -- plaintext never leaves the process.

## Building

The FUSE mount is feature-gated. Build the client binary with the `fuse`
feature:

```bash
cargo build --release --bin kiseki-client-fuse --features fuse
```

This requires the `fuser` crate, which depends on the FUSE kernel module
being available on the host:

- **Linux**: install `fuse3` or `libfuse3-dev`
- **macOS**: install [macFUSE](https://osxfuse.github.io/)

## Mounting

```bash
kiseki-client-fuse mount /mnt/kiseki \
    --data-addr <storage-node>:9100 \
    --tenant <tenant-id> \
    --namespace <namespace-id>
```

### Mount Options

Options are passed with `-o`:

```bash
kiseki-client-fuse mount /mnt/kiseki \
    -o cache_mode=organic \
    -o cache_dir=/local-nvme/kiseki-cache \
    -o cache_l2_max=100G \
    -o meta_ttl_ms=5000
```

| Option | Values | Default | Description |
|--------|--------|---------|-------------|
| `cache_mode` | `pinned`, `organic`, `bypass` | `organic` | Cache operating mode (see [Client Cache](client-cache.md)) |
| `cache_dir` | path | `/tmp/kiseki-cache` | L2 NVMe cache directory |
| `cache_l1_max` | bytes | `256M` | L1 (in-memory) cache size |
| `cache_l2_max` | bytes | `50G` | L2 (NVMe) cache size per process |
| `meta_ttl_ms` | milliseconds | `5000` | Metadata cache TTL |

### Environment Variables

Mount options can also be set via environment variables. Mount options
take priority over environment variables.

| Variable | Equivalent option |
|----------|-------------------|
| `KISEKI_CACHE_MODE` | `cache_mode` |
| `KISEKI_CACHE_DIR` | `cache_dir` |
| `KISEKI_CACHE_L1_MAX` | `cache_l1_max` |
| `KISEKI_CACHE_L2_MAX` | `cache_l2_max` |
| `KISEKI_CACHE_META_TTL_MS` | `meta_ttl_ms` |
| `KISEKI_CACHE_POOL_ID` | Adopt an existing cache pool (see [staging handoff](client-cache.md#staging-handoff)) |

## Supported Operations

### Read/Write

| Operation | Supported | Notes |
|-----------|-----------|-------|
| `read` | Yes | Served from cache (L1 -> L2 -> canonical) |
| `write` | Yes | Writes to canonical; local metadata cache updated immediately |
| `open` / `close` | Yes | Standard file handles |
| `fsync` / `fdatasync` | Yes | Flushes delta to Raft quorum |
| `truncate` / `ftruncate` | Yes | Composition resize |
| `O_APPEND` | Yes | Atomic append via delta |
| `O_CREAT` / `O_EXCL` | Yes | Atomic create-if-not-exists |
| `O_DIRECT` | Limited | Bypasses client cache, still goes through FUSE |

### Directory Operations

| Operation | Supported | Notes |
|-----------|-----------|-------|
| `mkdir` / `rmdir` | Yes | Create and remove directories |
| `readdir` / `readdirplus` | Yes | Listing from materialized view |
| `rename` (within namespace) | Yes | Atomic within shard |
| `rename` (cross-namespace) | No | Returns `EXDEV` |

### Metadata and Links

| Operation | Supported | Notes |
|-----------|-----------|-------|
| `stat` / `fstat` / `lstat` | Yes | File metadata |
| `chmod` / `chown` | Yes | Stored in delta attributes |
| `symlink` / `readlink` | Yes | Symlink targets stored as inline data |
| Hard links (within namespace) | Yes | |
| Hard links (cross-namespace) | No | Returns `EXDEV` |
| `xattr` operations | Yes | `getxattr`, `setxattr`, `listxattr`, `removexattr` |

### Nested Directories and Write-at-Offset

The FUSE filesystem supports full directory trees within a namespace.
Files can be created in nested directories, and writes at arbitrary
offsets within a file are supported (the composition tracks chunk
references and handles sparse regions with zero-fill).

```bash
mkdir -p /mnt/kiseki/experiments/run-42/logs
echo "epoch 1 loss: 0.3" > /mnt/kiseki/experiments/run-42/logs/train.log

# Write at offset (sparse file)
dd if=/dev/zero of=/mnt/kiseki/data/sparse.bin bs=1 count=1 seek=1048576
```

### Not Supported

| Operation | Reason |
|-----------|--------|
| Writable shared `mmap` | Returns `ENOTSUP`. Read-only mmap works. Use `write()` instead. (ADR-013) |
| POSIX ACLs | Unix permissions only (uid/gid/mode) |

## Cache Mode Selection

The cache mode determines how aggressively the client caches data on
local storage. Choose the mode that matches your workload:

| Mode | Best for | Behavior |
|------|----------|----------|
| `pinned` | Training (epoch reuse), inference (model weights) | Chunks retained until explicit release. Populate via staging API. |
| `organic` | Mixed workloads, interactive use | LRU eviction with usage-weighted retention. Default. |
| `bypass` | Streaming ingest, checkpoint writes, one-shot scans | No caching. All reads go directly to canonical storage. |

```bash
# Training job: pin the dataset
kiseki-client-fuse mount /mnt/kiseki -o cache_mode=pinned

# Interactive exploration
kiseki-client-fuse mount /mnt/kiseki -o cache_mode=organic

# Checkpoint writer
kiseki-client-fuse mount /mnt/kiseki -o cache_mode=bypass
```

See [Client Cache & Staging](client-cache.md) for staging pre-fetch,
Slurm integration, and policy configuration.

## Transport Selection

The native client automatically selects the fastest available transport
to reach storage nodes:

1. **libfabric/CXI** (Slingshot) -- if available on the fabric
2. **RDMA verbs** -- if InfiniBand/RoCE is available
3. **TCP+TLS** -- universal fallback

Transport selection is automatic and requires no configuration. The
client discovers available transports during fabric discovery at
startup (ADR-008).

## Unmounting

```bash
fusermount -u /mnt/kiseki    # Linux
umount /mnt/kiseki           # macOS
```

On clean unmount, the L2 cache pool is wiped (all chunk files are
zeroized and deleted). On crash, the orphaned cache pool is cleaned up
by the next client process or by the `kiseki-cache-scrub` service.
