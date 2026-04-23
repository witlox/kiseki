# Client-Side Cache (ADR-031)

The native client (`kiseki-client`) includes a two-tier read-only cache
of decrypted plaintext chunks. The cache is a performance feature, not a
correctness mechanism -- it is ephemeral and wiped on process restart or
extended disconnect.

---

## Architecture

```
┌────────────────────────────────────────────┐
│            kiseki-client process           │
│                                            │
│  ┌──────────────────────────────────────┐  │
│  │  L1: In-memory cache               │  │
│  │  Zeroizing<Vec<u8>> entries         │  │
│  │  Content-addressed by ChunkId       │  │
│  └──────────────┬───────────────────────┘  │
│                 │ miss                      │
│  ┌──────────────▼───────────────────────┐  │
│  │  L2: Local NVMe cache pool          │  │
│  │  CRC32 integrity per entry          │  │
│  │  Per-process, per-tenant isolation   │  │
│  └──────────────┬───────────────────────┘  │
│                 │ miss                      │
│                 ▼                           │
│         Fetch from canonical               │
│         (verify by ChunkId SHA-256)        │
└────────────────────────────────────────────┘
```

**L1 (in-memory)**: Fast access to recently-used chunks. Entries use
`Zeroizing<Vec<u8>>` so plaintext is overwritten with zeros on eviction
or deallocation (I-CC2).

**L2 (local NVMe)**: Larger cache on local storage. Each entry has a
CRC32 checksum trailer computed at insert time (I-CC13). On read, the
CRC32 is verified before serving; mismatch triggers bypass to canonical
and entry deletion (I-CC7).

---

## Cache modes

Three modes are available per client session (selected at session
establishment):

| Mode | Behavior | Use case |
|---|---|---|
| **Pinned** | Staging-driven, eviction-resistant; for declared datasets | HPC pre-staging (Slurm prolog) |
| **Organic** | LRU with usage-weighted retention | Mixed workloads (default) |
| **Bypass** | No caching | Streaming, checkpoint workloads |

Mode is per session, not per file. The admin controls which modes are
available for each workload.

---

## Staging API

Staging pre-fetches a dataset's chunks into the L2 cache with pinned
retention:

```
kiseki-client stage --dataset /path/to/data
```

1. Takes a namespace path and recursively enumerates compositions
2. Fetches and verifies all chunks from canonical (SHA-256 match)
3. Stores chunks in L2 with pinned retention
4. Produces a manifest file listing staged compositions and chunk IDs

Staging is idempotent and resumable. Limits: `max_staging_depth` (10),
`max_staging_files` (100,000).

### Pool handoff

The staging daemon and workload process can be different processes (e.g.,
Slurm prolog stages, then the workload runs):

1. Staging daemon holds the L2 pool via `flock` on `pool.lock`
2. Workload process adopts the pool via `KISEKI_CACHE_POOL_ID` env var
3. Workload takes over the `flock`

Each cache pool is identified by a 128-bit CSPRNG `pool_id`, isolated
per process and per tenant.

---

## Freshness and staleness

**Metadata TTL** (default 5s): File-to-chunk-list mappings are cached
with a configurable TTL. Within the TTL, cached metadata is authoritative
and may serve data for files that have since been modified (I-CC3, I-CC5).

**Chunk data**: No TTL needed. Chunks are immutable (I-C1), so a verified
chunk remains correct indefinitely absent crypto-shred.

---

## Crypto-shred detection

On crypto-shred (tenant KEK destruction), all cached plaintext must be
wiped (I-CC12):

**Detection mechanisms** (in priority order):
1. Advisory channel notification (if active)
2. KMS error on next operation
3. Periodic key health check (default every 30s)

**Response**: Immediate wipe of L1 and L2 with zeroize.

Maximum detection latency: `min(key_health_interval, max_disconnect_seconds)`.

---

## Disconnect handling

If the client loses connectivity to all canonical endpoints for longer
than `max_disconnect_seconds` (default 300s), the entire cache (L1 + L2)
is wiped (I-CC6).

A background heartbeat RPC (every 60s) maintains the `last_successful_rpc`
timestamp for disconnect detection.

---

## Error handling

Any local cache error bypasses to canonical unconditionally (I-CC7):

- L2 I/O failure: bypass and flag pool for scrub
- CRC32 mismatch: bypass, delete corrupt entry
- Metadata lookup failure: bypass to canonical

---

## Invariants

| ID | Rule |
|---|---|
| I-CC1 | A cached chunk is served only if content-address verified and no crypto-shred detected |
| I-CC2 | Cached plaintext is zeroized before deallocation, eviction, or cache wipe |
| I-CC3 | File-to-chunk metadata served from cache only within TTL (default 5s) |
| I-CC5 | Metadata TTL is the upper bound on read staleness |
| I-CC6 | Disconnect beyond threshold triggers full cache wipe |
| I-CC7 | Any cache error bypasses to canonical unconditionally |
| I-CC8 | Cache is ephemeral; wiped on process start (or adopted via pool handoff) |
| I-CC9 | Unreachable cache policy falls back to conservative defaults |
| I-CC10 | Cache policy changes apply to new sessions only |
| I-CC11 | Staged chunks are a point-in-time snapshot; re-stage to pick up updates |
| I-CC12 | Crypto-shred triggers immediate cache wipe with zeroize |
| I-CC13 | L2 entries protected by CRC32 checksum trailer |

---

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `KISEKI_CACHE_MODE` | `organic` | Cache mode: `organic`, `pinned`, or `bypass` |
| `KISEKI_CACHE_DIR` | `/tmp/kiseki-cache` | L2 pool directory on local NVMe |
| `KISEKI_CACHE_L2_MAX` | 50 GB | Maximum L2 cache size in bytes |
| `KISEKI_CACHE_POOL_ID` | (generated) | Adopt an existing pool (for staging handoff) |
