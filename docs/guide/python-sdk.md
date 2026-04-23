# Python SDK

Kiseki provides Python bindings via [PyO3](https://pyo3.rs/), exposing
the native client's cache, staging, and workflow advisory APIs to Python
workloads. The bindings are part of the `kiseki-client` crate, enabled
with the `python` feature flag.

## Building

Build and install the Python module using
[maturin](https://www.maturin.rs/):

```bash
pip install maturin
maturin develop --features python
```

This builds the native Rust code and installs the `kiseki` module into
the active Python environment.

For a release build:

```bash
maturin build --release --features python
pip install target/wheels/kiseki-*.whl
```

## Quick Start

```python
import kiseki

# Create a client with organic caching (default)
client = kiseki.Client(cache_mode="organic", cache_dir="/tmp/kiseki-cache")

# Stage a dataset into the local cache
client.stage("/training/imagenet")

# ... workload reads via FUSE or native API ...

# Check cache statistics
stats = client.cache_stats()
print(stats)
# CacheStats(l1_hits=42, l2_hits=1500, misses=200, l1_bytes=134217728, l2_bytes=5368709120, wipes=0)

# Release the staged dataset
client.release("/training/imagenet")

# Clean up
client.close()
```

## API Reference

### `kiseki.Client`

The main entry point. Each `Client` instance manages its own cache pool
(L1 in-memory + L2 NVMe) and advisory session.

#### Constructor

```python
client = kiseki.Client(
    cache_mode="organic",           # "pinned", "organic", or "bypass"
    cache_dir="/tmp/kiseki-cache",  # L2 NVMe cache directory
    cache_l2_max=50 * 1024**3,      # L2 max bytes (default: 50 GB)
    meta_ttl_ms=5000,               # Metadata TTL in ms (default: 5000)
)
```

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `cache_mode` | `str` | `"organic"` | Cache mode: `"pinned"`, `"organic"`, or `"bypass"` |
| `cache_dir` | `str` | `"/tmp/kiseki-cache"` | Directory for L2 NVMe cache files |
| `cache_l2_max` | `int` or `None` | `None` (50 GB) | Maximum L2 cache size in bytes |
| `meta_ttl_ms` | `int` | `5000` | Metadata cache TTL in milliseconds |

#### `stage(namespace_path: str) -> None`

Pre-fetch a dataset's chunks into the local cache with pinned retention.
The dataset is identified by its namespace path (e.g.,
`"/training/imagenet"`). Staging is idempotent -- re-staging an
already-staged dataset is a no-op.

```python
client.stage("/training/imagenet")
client.stage("/training/imagenet")  # no-op, already staged
```

For directory paths, staging recursively enumerates all files up to a
depth of 10 and a maximum of 100,000 files.

#### `stage_status() -> list[str]`

Return the namespace paths of all currently staged datasets.

```python
paths = client.stage_status()
# ["/training/imagenet", "/models/gpt-3"]
```

#### `release(namespace_path: str) -> None`

Release a staged dataset, unpinning its chunks and making them eligible
for eviction.

```python
client.release("/training/imagenet")
```

#### `release_all() -> None`

Release all staged datasets.

```python
client.release_all()
```

#### `cache_stats() -> CacheStatsView`

Return current cache statistics.

```python
stats = client.cache_stats()
print(f"L1 hits: {stats.l1_hits}")
print(f"L2 hits: {stats.l2_hits}")
print(f"Misses:  {stats.misses}")
print(f"L1 used: {stats.l1_bytes / 1024**2:.0f} MB")
print(f"L2 used: {stats.l2_bytes / 1024**3:.1f} GB")
print(f"Wipes:   {stats.wipes}")
```

#### `cache_mode() -> str`

Return the current cache mode as a string.

```python
print(client.cache_mode())  # "organic"
```

#### `declare_workflow() -> int`

Declare a new workflow for advisory integration. Returns a workflow ID
(128-bit integer) that can be used to correlate operations with the
advisory channel for telemetry feedback.

```python
wf_id = client.declare_workflow()
# ... run training epochs ...
client.end_workflow(wf_id)
```

#### `end_workflow(workflow_id: int) -> None`

End a previously declared workflow.

#### `wipe() -> None`

Immediately wipe the entire cache (L1 + L2). All cached plaintext is
zeroized before deletion.

#### `close() -> None`

Wipe the cache and release resources. Call this when the workload is
done. Equivalent to `wipe()`.

### `kiseki.CacheStatsView`

Read-only statistics object returned by `cache_stats()`.

| Attribute | Type | Description |
|-----------|------|-------------|
| `l1_hits` | `int` | Number of L1 (memory) cache hits |
| `l2_hits` | `int` | Number of L2 (NVMe) cache hits |
| `misses` | `int` | Number of cache misses (fetched from canonical) |
| `l1_bytes` | `int` | Current L1 memory usage in bytes |
| `l2_bytes` | `int` | Current L2 disk usage in bytes |
| `wipes` | `int` | Number of full cache wipes |

## Example: Training Workflow

```python
import kiseki

def train():
    # Pin the dataset for the duration of training
    client = kiseki.Client(cache_mode="pinned", cache_dir="/local-nvme/cache")

    # Pre-stage the dataset (ideally done in Slurm prolog)
    client.stage("/training/imagenet-22k")

    # Declare a workflow for advisory telemetry
    wf_id = client.declare_workflow()

    try:
        for epoch in range(100):
            # Dataset reads hit L2 cache after first epoch
            # ... training loop reads from /mnt/kiseki/training/imagenet-22k/ ...
            pass

        stats = client.cache_stats()
        print(f"Cache hit rate: {(stats.l1_hits + stats.l2_hits) / "
              f"(stats.l1_hits + stats.l2_hits + stats.misses) * 100:.1f}%")
    finally:
        client.end_workflow(wf_id)
        client.release_all()
        client.close()

if __name__ == "__main__":
    train()
```

## Example: Inference with Organic Caching

```python
import kiseki

client = kiseki.Client(cache_mode="organic", cache_l2_max=20 * 1024**3)

# Model weights are cached on first load, then served from L2
# Prompt data is cached with LRU eviction

wf_id = client.declare_workflow()
try:
    # ... inference serving loop ...
    pass
finally:
    client.end_workflow(wf_id)
    client.close()
```

## Example: Checkpoint Writer (No Caching)

```python
import kiseki

# Bypass mode: checkpoint writes go straight to canonical
client = kiseki.Client(cache_mode="bypass")

# ... write checkpoints to /mnt/kiseki/checkpoints/ ...

client.close()
```

## Environment Variable Overrides

The Python client respects the same environment variables as the FUSE
mount and CLI:

| Variable | Description |
|----------|-------------|
| `KISEKI_CACHE_MODE` | Override cache mode |
| `KISEKI_CACHE_DIR` | Override cache directory |
| `KISEKI_CACHE_L1_MAX` | Override L1 max bytes |
| `KISEKI_CACHE_L2_MAX` | Override L2 max bytes |
| `KISEKI_CACHE_META_TTL_MS` | Override metadata TTL |
| `KISEKI_CACHE_POOL_ID` | Adopt an existing cache pool (staging handoff) |

Constructor parameters take priority over environment variables.
All client-set values are clamped to the effective policy ceilings set
by tenant and cluster administrators.
