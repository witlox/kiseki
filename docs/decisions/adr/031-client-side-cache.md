# ADR-031: Client-Side Cache

**Status**: Accepted
**Date**: 2026-04-23
**Deciders**: Architect + domain expert
**Adversarial review**: 2026-04-23 (14 findings: 2C 4H 4M 4L, all resolved)

## Context

ADR-013 (POSIX semantics scope), ADR-019 (gateway deployment model),
ADR-020 (workflow advisory), ADR-030 (dynamic small-file placement),
`control-plane.feature` (policy distribution precedent),
`native-client.feature` (client architecture).

CSCS workload mix: LLM pretraining (epoch reuse of tokenized datasets),
LLM inference (model weight cold-start), climate/weather simulation
(bounded input staging with hard deadlines), HPC checkpoint/restart.
Common pattern: compute nodes repeatedly pull the same encrypted chunks
across the fabric.

Existing client architecture: `kiseki-client` crate with feature flags
(`fuse`, `ffi`, `python`, pure-Rust default). Performs tenant-layer
encryption — plaintext never leaves the workload process. The existing
`ClientCache` is an in-memory `HashMap<ChunkId, Vec<u8>>` with TTL and
max-entries eviction.

## Problem

1. Repeat reads of the same chunks cross the fabric unnecessarily.
   Training datasets are read epoch after epoch. Inference weights are
   loaded identically by multiple model replicas. Climate boundary
   conditions are staged identically to every simulation rank.

2. In-memory cache (current `ClientCache`) is bounded by process memory,
   which is primarily needed for computation. Compute-node NVMe is
   available and underutilized.

3. No mechanism for pre-staging datasets. Jobs start with cold cache
   and pay first-access latency on every rank simultaneously, creating
   a thundering-herd pattern on the storage fabric.

4. No cache mode differentiation. Training (pin everything), inference
   (pin weights, LRU prompts), and HPC checkpoint (don't cache) have
   fundamentally different cache needs.

## Decision

### 1. Cache architecture

The client-side cache is a library-level module in `kiseki-client`,
shared across all linkage modes (FUSE, FFI, Python, native Rust). It
operates on **decrypted plaintext chunks** keyed by `ChunkId`.

```
canonical (fabric) → decrypt → cache store (NVMe) → serve to caller
                                    ↑
                          cache hit path (no fabric, no decrypt)
```

**Two-tier storage**:

| Tier | Backing | Purpose | Eviction |
|------|---------|---------|----------|
| Hot (L1) | In-memory `HashMap` | Sub-microsecond hits for active working set | LRU, bounded by `max_memory_bytes` |
| Warm (L2) | Local NVMe file or directory | Large capacity for datasets and weights | Per-mode policy (see §2) |

L2 layout on NVMe (CC-ADV-4 resolved: per-process subdirectories):
```
$KISEKI_CACHE_DIR/
├── <tenant_id_hex>/
│   ├── <pool_id>/                 ← per-process pool (128-bit CSPRNG)
│   │   ├── chunks/
│   │   │   ├── <prefix>/
│   │   │   │   └── <chunk_id_hex> ← plaintext + CRC32 trailer
│   │   │   └── ...
│   │   ├── meta/
│   │   │   └── file_chunks.db
│   │   ├── staging/
│   │   │   └── <dataset_id>.manifest
│   │   └── pool.lock              ← flock, proves process is alive
│   └── <pool_id>/                 ← another concurrent process
│       └── ...
└── ...
```

Each client process creates its own `pool_id` directory (128-bit
CSPRNG, same generation as `client_id` per I-WA4). The `pool.lock`
file holds an `flock` for the process lifetime. Multiple concurrent
same-tenant processes on the same node have fully independent pools
with no contention.

**L2 integrity** (CC-ADV-3 resolved): Each L2 chunk file stores the
plaintext data followed by a 4-byte CRC32 trailer, computed at insert
time. On L2 read, the CRC32 is verified before serving. Full SHA-256
content-address verification occurs only at fetch time (when the chunk
is first retrieved from canonical). CRC32 catches bit-flips and
filesystem corruption at ~1 GB/s throughput cost. CRC mismatch
triggers bypass to canonical and L2 entry deletion (I-CC7).

**Security model** (plaintext cache):

The L2 cache holds decrypted plaintext on local NVMe. This is
acceptable because:
- The compute node already holds decrypted data in process memory
  (computation requires plaintext)
- L2 NVMe is local to the compute node, same trust domain as process
  memory
- L2 is ephemeral — wiped on process exit and on long disconnect
- `zeroize` on eviction/wipe: overwrite chunk data before deallocation
  (I-CC2)
- File permissions: `0600`, owned by process UID
- Crash recovery: startup scavenger + periodic scrubber clean orphaned
  pools (CC-ADV-1 resolved, see §9)

**Residual risk** (CC-ADV-10 acknowledged): Software zeroize on NVMe/SSD
provides logical-level erasure only. The Flash Translation Layer may
retain physical copies of overwritten data until internal garbage
collection. For deployments requiring physical erasure guarantees, use
NVMe drives with hardware encryption (OPAL/SED) and rotate the drive
encryption key on node reboot. This is an operational hardening measure,
not a baseline requirement.

### 2. Cache modes

Three modes, selectable per client instance at session establishment:

#### Pinned mode

For workloads that declare their dataset upfront: training runs (epoch
reuse), inference (model weights), climate (boundary conditions).

- Chunks are **retained against eviction** until explicit release
- Populated via the staging API (§6) or on first access
- L2 is the primary tier; L1 is a hot subset
- Eviction: only on explicit `release()` or process exit
- Capacity bounded by `max_cache_bytes` (§8); staging beyond capacity
  returns an error, does not evict pinned chunks

**Dataset versioning** (CC-ADV-8 resolved): Pinned mode stages a
point-in-time snapshot of the dataset. The staged version is immutable
in the cache regardless of canonical updates. This is intentional —
training runs require a stable dataset across epochs. To pick up
dataset updates, the user must explicitly `release` and re-`stage`.
There is no automatic dataset-level version check.

#### Organic mode

Default for mixed workloads. LRU with usage-weighted retention.

- Chunks cached on first read, evicted on LRU when capacity is reached
- Frequently accessed chunks promoted to L1
- L2 eviction: LRU by last-access timestamp, weighted by access count
  (chunks accessed N times survive N eviction rounds)
- Metadata cache (file→chunk_list) with configurable TTL (default 5s)

#### Bypass mode

For workloads that don't benefit from caching: streaming ingest, one-shot
scans, checkpoint writes, compute-bound codes with no repeat reads.

- All reads go directly to canonical
- No L1 or L2 storage consumed
- Zero overhead beyond mode selection

### 3. Metadata cache

The cache stores file-to-chunk-list mappings with a bounded TTL:

```rust
struct MetadataEntry {
    chunk_list: Vec<ChunkId>,
    fetched_at: Instant,
    ttl: Duration,
}
```

**I-CC3** (metadata freshness and authority): File→chunk_list metadata
mappings are served from cache only within the configured TTL (default
5s). After TTL expiry, the mapping must be re-fetched from canonical
before serving chunks that depend on it. Within the TTL window, the
cached mapping is authoritative — it may serve data for files that
have since been modified or deleted in canonical. This is an accepted
consequence of the TTL window, not a correctness violation. Modifications
create new compositions with new chunk_ids; the old mapping points to
valid immutable chunks that were the file's content at fetch time.
Deletions remove the composition; the cached mapping continues to serve
the deleted file's data until TTL expiry.

**I-CC5** (staleness bound): Metadata TTL is the upper bound on read
staleness. A file modified or deleted in canonical will be visible to
a caching client within at most one metadata TTL period. The default
TTL (5 seconds) balances freshness against metadata lookup cost.

**Write-through**: When the client writes a file (creating new chunks
and a new composition), the local metadata cache is updated immediately
with the new chunk list. This provides read-your-writes consistency
within a single client process without waiting for TTL expiry.

### 4. Correctness invariants

The cache's correctness rests on a small set of stated invariants.
Each case where the cache serves (rather than bypasses) is backed by
one or more of these invariants. Cases not covered bypass to canonical.

**I-CC1** (chunk immutability): Chunks are immutable in canonical
(I-C1). A chunk fetched, verified by content-address (SHA-256 of
plaintext matches chunk_id derivation), and stored in cache is correct
for all future reads of that chunk_id. No TTL needed for chunk data.

**I-CC2** (plaintext security): Cached plaintext is overwritten with
zeros (`zeroize`) before deallocation, eviction, or cache wipe.
File-level: overwrite contents before unlink. Memory-level:
`Zeroizing<Vec<u8>>` for L1 entries. This provides logical-level
erasure; physical-level erasure on flash storage requires hardware
encryption (see §1 residual risk).

**I-CC6** (disconnect threshold): Cached entries remain authoritative
across fabric disconnects shorter than `max_disconnect_seconds`
(default 300s). Beyond this threshold, the entire cache (L1 + L2) is
wiped. Disconnect is defined as: no successful RPC to any canonical
endpoint (storage node or gateway) for `max_disconnect_seconds`
consecutive seconds. The client maintains a `last_successful_rpc`
timestamp updated on every successful data-path or heartbeat RPC.
Background heartbeat RPCs (every 60s, piggybacked on metadata TTL
refresh when idle) keep this timestamp current. Transient single-RPC
failures do not trigger the disconnect timer — only sustained
unreachability across all endpoints does.

**I-CC7** (error bypass): Any local cache error (L2 I/O failure,
corrupt chunk detected by CRC32 mismatch, metadata lookup failure)
bypasses to canonical unconditionally. The cache never serves data
it cannot verify. Failed L2 reads are not retried from L2 — they
go to canonical immediately.

**I-CC8** (wipe on restart / crash recovery): On process start, the
client either creates a new L2 pool (wiping any prior orphaned pools)
or adopts an existing pool identified by `KISEKI_CACHE_POOL_ID`
environment variable (see §6 staging handoff). Orphaned pools are
detected by attempting `flock` on each `pool.lock` — if the lock
succeeds, the pool is orphaned (no live process holds it) and is
wiped (zeroized and deleted). A separate `kiseki-cache-scrub` service
runs on node boot and periodically (every 60s) to clean orphaned
pools across all tenants, covering crash recovery when no subsequent
kiseki process starts on that node.

**I-CC13** (L2 integrity): L2 cache entries are protected by a CRC32
checksum computed at insert time and stored as a 4-byte trailer on
each chunk file. On L2 read, the CRC32 is verified before serving.
CRC mismatch triggers bypass to canonical and L2 entry deletion.

### 5. Policy authority and distribution

Cache policy follows the same distribution mechanism as quotas
(per `control-plane.feature` scenario "Quota enforcement during
control plane outage").

#### Policy hierarchy

```
cluster default → org override → project override → workload override
                                                      → session selection
```

Each level narrows (never broadens) the parent's settings, consistent
with ADR-020 / I-WA7.

#### Policy attributes

| Attribute | Type | Admin levels | Client selectable | Default |
|-----------|------|-------------|-------------------|---------|
| `cache_enabled` | bool | cluster, org, project, workload | No | true |
| `allowed_modes` | set{pinned, organic, bypass} | cluster, org | No | {pinned, organic, bypass} |
| `max_cache_bytes` | u64 | cluster, org, workload | Up to ceiling | 50 GB |
| `max_node_cache_bytes` | u64 | cluster | No | 80% of cache filesystem |
| `metadata_ttl_ms` | u64 | cluster, org | Up to ceiling | 5000 |
| `max_disconnect_seconds` | u64 | cluster | No | 300 |
| `key_health_interval_ms` | u64 | cluster | No | 30000 |
| `staging_enabled` | bool | cluster, org | No | true |
| `mode` | enum | workload (default) | Yes (within allowed) | organic |

**Narrowing rules** (same as I-WA7):
- `cache_enabled = false` at any level → disabled for all children
- `allowed_modes` at child ⊆ `allowed_modes` at parent
- `max_cache_bytes` at child ≤ `max_cache_bytes` at parent
- `metadata_ttl_ms` at child ≤ `metadata_ttl_ms` at parent

#### Distribution mechanism

Cache policy is carried in the same `TenantConfig` structure that
carries quotas. At session establishment, the client resolves its
effective policy through multiple paths (CC-ADV-6 resolved):

1. **Primary**: `GetCachePolicy` RPC on the data-path gRPC channel to
   any connected storage node. Storage nodes have `TenantConfig`
   (same data they use for quota enforcement). No gateway or control
   plane reachability required — the client only needs the data fabric.
2. **Secondary**: fetch from gateway's locally-cached `TenantConfig`
   (if gateway is reachable)
3. **Stale tolerance**: last-known policy persisted in L2 pool directory
   (`policy.json`). Remains effective during outages, consistent with
   quota scenario in `control-plane.feature`.
4. **Fallback**: if no policy resolvable (first-ever session, all paths
   unreachable), use **conservative defaults** (cache enabled, organic
   mode, 10 GB max, 5s TTL)
5. **Reconciliation**: on control-plane recovery, client re-fetches
   policy and applies prospectively (I-WA18 pattern — active sessions
   continue under session-start policy; new sessions use updated policy)

No parallel policy-distribution path is introduced. Cache policy is
one more field in `TenantConfig`, alongside quotas, compliance tags,
and advisory settings.

**I-CC9** (policy fallback): When effective cache policy is unreachable
at session start, the client operates with conservative defaults (cache
enabled, organic mode, 10 GB ceiling, 5s metadata TTL). The cache is
a performance feature; failing to resolve policy must not prevent data
access.

**I-CC10** (prospective policy): Cache policy changes apply to new
sessions only. Active sessions continue under the policy effective at
session establishment, consistent with I-WA18.

### 6. Staging API

Client-local operation for pre-populating the cache with a dataset's
chunks in pinned mode. Pull-based — the client fetches from canonical.

#### Interface

```
# CLI (Slurm prolog, manual use)
kiseki-client stage --dataset <namespace_path> [--timeout <seconds>]
kiseki-client stage --status [--dataset <namespace_path>]
kiseki-client stage --release <namespace_path>
kiseki-client stage --release-all

# Rust API
impl CacheManager {
    async fn stage(&self, namespace_path: &str) -> Result<StageResult>;
    fn stage_status(&self) -> Vec<StagedDataset>;
    fn release(&self, namespace_path: &str);
    fn release_all(&self);
}

# Python API (via PyO3)
client.stage(namespace_path="/training/imagenet")
client.stage_status()
client.release(namespace_path="/training/imagenet")

# C FFI
kiseki_stage(handle, "/training/imagenet", timeout_secs)
kiseki_stage_status(handle, &status)
kiseki_release(handle, "/training/imagenet")
```

#### Flow (CC-ADV-11 resolved: directory tree handling)

1. Resolve `namespace_path` to composition metadata via canonical.
   If the path is a directory, recursively enumerate all files
   (compositions) up to `max_staging_depth` (default 10) and
   `max_staging_files` (default 100,000). If limits are exceeded,
   return an error with the count of files discovered.
2. Extract full chunk list from all resolved compositions
3. For each chunk not already in L2: fetch from canonical, decrypt,
   verify content-address (SHA-256), store in L2 with CRC32 trailer
   and pinned retention
4. Write `staging/<dataset_id>.manifest` listing all compositions,
   their chunk_ids, and the total byte count
5. Report progress (chunks staged / total, bytes, elapsed)

Staging is **idempotent** — re-staging an already-staged dataset is
a no-op (chunks already present). Partial staging (interrupted) can
be resumed by re-running the command.

#### Staging handoff (CC-ADV-5 resolved)

The staging CLI creates a cache pool and holds the `pool.lock` flock
for its lifetime. The workload process adopts the staging pool instead
of creating a new one:

1. Staging CLI: `kiseki-client stage --dataset /training/imagenet`
   - Creates pool, writes `pool_id` to stdout and to
     `$KISEKI_CACHE_DIR/<tenant>/staging_pool_id`
   - Stages chunks, holds flock, stays alive (daemon mode)
2. Workload process: sets `KISEKI_CACHE_POOL_ID=<pool_id>` (from
   Slurm prolog output, Lattice env injection, or the file)
   - On start, detects existing pool with matching `pool_id`
   - Adopts pool: takes over flock from staging daemon
   - Staging daemon detects flock loss, exits cleanly
3. If `KISEKI_CACHE_POOL_ID` is not set: normal fresh-pool behavior
   (create new pool, wipe orphans)

**Slurm integration**:
```bash
# prolog.sh:
POOL_ID=$(kiseki-client stage --dataset /training/imagenet --daemon)
echo "export KISEKI_CACHE_POOL_ID=$POOL_ID" >> $SLURM_EXPORT_FILE

# epilog.sh:
kiseki-client stage --release-all --pool $KISEKI_CACHE_POOL_ID
```

**Lattice integration**: injects `KISEKI_CACHE_POOL_ID` into the
workload environment after parallel staging completes across the
node set. Queries `stage --status` to verify readiness before
launching the workload.

**I-CC11** (staging correctness): Staged chunks are fetched from
canonical, verified by content-address, and stored with pinned
retention. The staging manifest records the compositions and chunk_ids
at staging time as a point-in-time snapshot. If the dataset is modified
in canonical after staging, the staged version remains correct for its
chunk_ids (immutable chunks) but stale relative to the current dataset
version. To pick up updates, the user must explicitly `release` and
re-`stage`.

### 7. Cache invalidation

The cache is primarily self-consistent due to chunk immutability
(I-C1). Explicit invalidation is needed only for metadata:

**Metadata invalidation**: TTL-based. No push invalidation from
canonical to client. The metadata TTL is the sole freshness mechanism.

**Chunk invalidation**: Not needed under normal operation (chunks are
immutable). Two exceptional cases:

1. **Crypto-shred** (CC-ADV-2 resolved): When a tenant's KEK is
   destroyed, all cached plaintext for that tenant must be wiped.
   Detection via three paths:

   - **Periodic key health check**: Client pings KMS every
     `key_health_interval` (default 30s). If the tenant KEK is
     reported destroyed (`KEK_DESTROYED` error), wipe immediately.
   - **Advisory channel**: If connected, receives shred notification
     immediately (fast path, best-effort).
   - **KMS error on next operation**: Any key fetch that returns
     `KEK_DESTROYED` triggers immediate wipe.
   - **Unreachability fallback**: If KMS is unreachable for
     `max_disconnect_seconds`, the disconnect timer triggers a full
     cache wipe (I-CC6), which covers the case where the KMS is
     unreachable *because* the KEK was destroyed.

   Maximum time between crypto-shred event and cache wipe is bounded
   by `min(key_health_interval, max_disconnect_seconds)` — default 30
   seconds.

2. **Key rotation**: When the system key epoch rotates, existing
   cached plaintext remains valid (same content, different encryption
   at rest). No cache action needed — the cache holds plaintext, not
   ciphertext.

**I-CC12** (crypto-shred wipe): On crypto-shred event, all cached
plaintext for the affected tenant is wiped from L1 and L2 with
zeroize. Detection bounded by `key_health_interval` (default 30s).
No cached data from a shredded tenant is served after detection.

### 8. Capacity management

**Per-process limits**:

| Parameter | Default | Source |
|-----------|---------|--------|
| `max_memory_bytes` (L1) | 256 MB | env `KISEKI_CACHE_L1_MAX` or API |
| `max_cache_bytes` (L2) | 50 GB | policy ceiling or env `KISEKI_CACHE_L2_MAX` |

**Per-node limit** (CC-ADV-9 resolved):

`max_node_cache_bytes` (default: 80% of `$KISEKI_CACHE_DIR` filesystem
capacity). Enforced cooperatively: before inserting into L2, each
process sums total usage across all pool directories in
`$KISEKI_CACHE_DIR`. If total exceeds `max_node_cache_bytes`, the
insert is rejected (organic: evict first; pinned: staging error).
The disk-pressure check (90% filesystem utilization) remains as a
hard backstop.

**Capacity enforcement**:
- L1: strict LRU eviction at `max_memory_bytes`
- L2 organic mode: LRU eviction at `max_cache_bytes`
- L2 pinned mode: staging requests rejected with `CacheCapacityExceeded`
  when staged + proposed > `max_cache_bytes`. No eviction of pinned data.
- Combined pinned + organic: pinned chunks are never evicted by organic
  LRU. Organic eviction only considers non-pinned chunks.
- Node-wide: cooperative check against `max_node_cache_bytes` before
  any L2 insert.

### 9. Lifecycle

**Process start** (CC-ADV-1 resolved: crash recovery):
1. If `KISEKI_CACHE_POOL_ID` set: adopt existing pool (§6 handoff)
2. Otherwise: create new pool with CSPRNG `pool_id`
3. **Scavenge orphans**: scan all pool directories under
   `$KISEKI_CACHE_DIR/<tenant_id>/`, attempt flock on each `pool.lock`.
   If lock succeeds (no live holder), the pool is orphaned — zeroize
   all chunk files, delete directory. This catches prior crashes.
4. Resolve effective cache policy (§5)
5. Initialize L1 (empty `HashMap`)
6. Start background tasks: metadata TTL eviction, disk-pressure check,
   key health check (every `key_health_interval`), heartbeat RPC
   (every 60s for disconnect detection)
7. Cache operational

**Crash recovery service** (`kiseki-cache-scrub`):
A systemd one-shot service (or cron job) that runs on node boot and
every 60 seconds. Scans `$KISEKI_CACHE_DIR` for all tenant/pool
directories, wipes any whose `pool.lock` has no live flock holder.
This covers the case where no subsequent kiseki process starts on the
node after a crash.

**Steady state**:
- Reads: L1 → L2 (CRC32 verify) → canonical (decrypt + SHA-256 verify
  + store in L1/L2 with CRC32 trailer)
- Writes: straight to canonical; update local metadata cache
- Background: periodic L1 expired-entry eviction, L2 disk-pressure
  check, key health check, heartbeat RPC

**Disconnect (fabric unreachable)**:
- Reads from L1/L2 continue to be served (chunks are immutable)
- After `max_disconnect_seconds` with no successful RPC to any
  canonical endpoint: wipe entire cache (I-CC6)
- On reconnect before threshold: resume normal operation

**Process exit (clean)**:
- Wipe L2 (zeroize all chunk files, delete pool directory)
- L1 freed with process memory (`Zeroizing` drop handles cleanup)
- Release flock on `pool.lock`

**Process exit (crash)**:
- L2 chunk files remain on NVMe (no zeroize opportunity)
- Next process start or `kiseki-cache-scrub` service detects orphaned
  pool via flock check and wipes it

### 10. Configuration surface

| Linkage mode | Configuration mechanism |
|-------------|----------------------|
| FUSE mount | Mount options: `-o cache_mode=organic,cache_l2_max=50G,cache_dir=/tmp/kiseki` |
| Rust API | `CacheConfig` struct passed to `Client::new()` |
| Python | `kiseki.Client(cache_mode="pinned", cache_l2_max=50*1024**3)` |
| C FFI | `kiseki_open()` with `KisekiCacheConfig` struct |
| Environment | `KISEKI_CACHE_MODE`, `KISEKI_CACHE_DIR`, `KISEKI_CACHE_L1_MAX`, `KISEKI_CACHE_L2_MAX`, `KISEKI_CACHE_META_TTL_MS`, `KISEKI_CACHE_POOL_ID` |

Priority: API/mount options > environment variables > policy defaults.
All client-set values are clamped to policy ceilings (§5).

### 11. Observability

Cache metrics exposed via the client's local metrics (not Prometheus —
client runs on compute nodes, not storage nodes):

| Metric | Type | Description |
|--------|------|-------------|
| `cache_l1_hits` | counter | L1 (memory) cache hits |
| `cache_l2_hits` | counter | L2 (NVMe) cache hits |
| `cache_misses` | counter | Cache misses (bypassed to canonical) |
| `cache_bypasses` | counter | Bypass mode reads (intentional non-cache) |
| `cache_errors` | counter | L2 I/O errors (bypassed to canonical per I-CC7) |
| `cache_l1_bytes` | gauge | Current L1 memory usage |
| `cache_l2_bytes` | gauge | Current L2 disk usage |
| `cache_staged_datasets` | gauge | Number of pinned datasets |
| `cache_staged_bytes` | gauge | Total bytes in pinned datasets |
| `cache_meta_hits` | counter | Metadata cache hits (within TTL) |
| `cache_meta_misses` | counter | Metadata cache misses (TTL expired or absent) |
| `cache_wipes` | counter | Full cache wipes (disconnect threshold, restart, crypto-shred) |
| `cache_l2_read_latency_us` | histogram | L2 NVMe read latency |
| `cache_l2_write_latency_us` | histogram | L2 NVMe write latency |

Metrics available via workflow advisory telemetry (scoped to caller)
and via local API (`cache_stats()`).

## Consequences

### Positive

- Repeat reads served from local NVMe: order-of-magnitude latency
  reduction for training datasets, inference weights, simulation input
- Staging API with scheduler handoff eliminates thundering-herd on
  job start
- Three modes match the three dominant workload patterns precisely
- Plaintext cache means cache hits avoid decryption cost entirely
- Policy model reuses existing `TenantConfig` distribution — no new
  subsystem
- Content-addressed chunk immutability makes cache correctness simple
  (I-C1 is the foundation)
- Crash recovery via flock-based orphan detection + scrubber service

### Negative

- Plaintext on local NVMe is a security surface. Mitigated by zeroize,
  file permissions, wipe-on-exit, crash scrubber, and ephemeral-only
  semantics (I-CC2, I-CC8). Residual FTL risk documented.
- Metadata TTL introduces a staleness window including for deleted
  files. Mitigated by short default (5s) and write-through for own
  writes (I-CC3, I-CC5)
- L2 NVMe cache competes with application use of local NVMe (e.g.,
  scratch, checkpoint). Mitigated by configurable per-process ceiling,
  per-node ceiling, and disk-pressure backoff (§8)
- No cross-process chunk sharing within a tenant means duplicate chunks
  when multiple jobs for the same tenant run on the same node. Accepted
  trade-off: simplicity over hit-rate optimization

### Neutral

- Bypass mode has zero overhead (no cache code on read path)
- Staging is idempotent and resumable
- Cache wipe on long disconnect is conservative but safe
- Policy distribution via data-path gRPC works in all deployment
  topologies (no gateway or control plane access required)

## Adversarial findings

| ID | Severity | Section | Finding | Resolution |
|----|----------|---------|---------|------------|
| CC-ADV-1 | Critical | §1, §9 | Crash leaves plaintext on NVMe unreachable by zeroize. Process crash skips the exit wipe path. | Resolved: startup scavenger wipes orphaned pools (flock detection). `kiseki-cache-scrub` systemd/cron service runs on boot + every 60s for nodes where no subsequent kiseki process starts. Residual FTL risk documented. §9 updated. |
| CC-ADV-2 | Critical | §7 | Crypto-shred detection has no reliable delivery path. Advisory channel is best-effort. | Resolved: periodic key health check (default 30s) as primary detection. Advisory channel as fast path. KMS error on next operation as tertiary. Unreachability falls through to disconnect timer (I-CC6). Maximum detection latency: `min(key_health_interval, max_disconnect_seconds)` = 30s default. §7 updated, I-CC12 revised. |
| CC-ADV-3 | High | §1 | L2 read verification unspecified. Full SHA-256 on every read too expensive for training throughput. | Resolved: CRC32 trailer on each L2 chunk file, verified on read. SHA-256 only at fetch time. CRC32 catches bit-flips at ~1 GB/s cost. CRC mismatch → bypass canonical + delete entry. New I-CC13. §1 updated. |
| CC-ADV-4 | High | §1 | `cache.lock` flock contradicts separate-pool semantics for concurrent same-tenant processes. | Resolved: per-process `pool_id` subdirectory (128-bit CSPRNG). Each process has own `pool.lock`. No contention between concurrent processes. L2 layout updated in §1. |
| CC-ADV-5 | High | §6 | Staging CLI is separate process — workload's wipe-on-start destroys staged data. | Resolved: staging daemon holds flock; workload adopts pool via `KISEKI_CACHE_POOL_ID` env var instead of creating new pool. Handoff mechanism specified in §6. I-CC8 revised to include adoption path. |
| CC-ADV-6 | High | §5 | Policy resolution via gateway unreachable in some topologies. | Resolved: primary path is `GetCachePolicy` RPC on data-path gRPC channel to any storage node. No gateway or control plane access required. Fallback chain: data-path → gateway → persisted last-known → conservative defaults. §5 updated. |
| CC-ADV-7 | Medium | §3, §4 | Metadata TTL authority doesn't explicitly cover file deletion case. | Resolved: I-CC3 text now explicitly states that serving data for a deleted file within TTL is an accepted consequence. I-CC5 updated to cover deletion. §3 updated. |
| CC-ADV-8 | Medium | §2 | Pinned mode has no mechanism to detect canonical dataset updates. | Resolved: documented as intentional. Pinned mode stages a point-in-time snapshot. Update requires explicit release + re-stage. §2 updated. |
| CC-ADV-9 | Medium | §8 | No aggregate capacity enforcement across processes on same node. | Resolved: `max_node_cache_bytes` policy attribute (default 80% of cache filesystem). Cooperative enforcement: each process sums all pools before inserting. Disk-pressure 90% as hard backstop. §8 updated, policy table updated. |
| CC-ADV-10 | Medium | §1 | NVMe FTL retains physical copies after software zeroize. | Resolved: acknowledged as residual risk. Recommended hardening: OPAL/SED NVMe with per-boot key rotation. §1 updated. |
| CC-ADV-11 | Medium | §6 | Staging conflates namespace path with single composition. | Resolved: staging flow now specifies recursive directory enumeration with `max_staging_depth` (default 10) and `max_staging_files` (default 100,000). §6 flow updated. |
| CC-ADV-12 | Low | §4 | I-CC3, I-CC4, I-CC5 partially overlap. | Resolved: I-CC3 and I-CC4 consolidated into single I-CC3 covering freshness, authority, and deletion case. I-CC5 retained as the externally-facing staleness guarantee. Invariant table updated. |
| CC-ADV-13 | Low | §9 | Disconnect detection mechanism unspecified. | Resolved: defined as "no successful RPC to any canonical endpoint for `max_disconnect_seconds` consecutive seconds." Client maintains `last_successful_rpc` timestamp. Background heartbeat every 60s. I-CC6 updated with detection mechanism. |
| CC-ADV-14 | Low | §11 | Missing L2 read/write latency metrics. | Resolved: added `cache_l2_read_latency_us` and `cache_l2_write_latency_us` histograms to metrics table. §11 updated. |

## Invariant impact

| Invariant | Impact |
|-----------|--------|
| I-C1 | Foundation: chunk immutability enables the cache. No change to I-C1. |
| I-K1, I-K2 | Unchanged: plaintext never leaves the compute node. Cache stores plaintext locally, same trust domain as process memory. |
| I-WA18 | Reused: cache policy changes apply prospectively. |
| I-WA7 | Reused: scope narrowing pattern for policy hierarchy. |

## New invariants

| ID | Invariant |
|----|-----------|
| I-CC1 | A chunk in pinned or organic mode is served from cache if and only if (a) the chunk was fetched from canonical and verified by chunk_id content-address match (SHA-256) at fetch time, and (b) no crypto-shred event has been detected for that tenant since fetch. Chunks are immutable in canonical (I-C1); therefore a verified chunk remains correct indefinitely absent crypto-shred. |
| I-CC2 | Cached plaintext is overwritten with zeros (zeroize) before deallocation, eviction, or cache wipe. File-level: overwrite contents before unlink. Memory-level: `Zeroizing<Vec<u8>>` for L1 entries. This provides logical-level erasure; physical-level erasure on flash storage requires hardware encryption (OPAL/SED). |
| I-CC3 | File→chunk_list metadata mappings are served from cache only within the configured TTL (default 5s). After TTL expiry, the mapping must be re-fetched from canonical. Within the TTL window, the cached mapping is authoritative: it may serve data for files that have since been modified or deleted in canonical. This is the sole freshness window in the cache design — chunk data itself has no TTL. |
| I-CC5 | Metadata TTL is the upper bound on read staleness. A file modified or deleted in canonical is visible to a caching client within at most one metadata TTL period (default 5s). |
| I-CC6 | Cached entries remain authoritative across fabric disconnects shorter than `max_disconnect_seconds` (default 300s). Beyond this threshold, the entire cache (L1 + L2) is wiped. Disconnect defined as: no successful RPC to any canonical endpoint for the threshold duration. Background heartbeat RPCs (every 60s) maintain the `last_successful_rpc` timestamp. |
| I-CC7 | Any local cache error (L2 I/O failure, CRC32 mismatch, metadata lookup failure) bypasses to canonical unconditionally. The cache never serves data it cannot verify. |
| I-CC8 | The cache is ephemeral. On process start, the client either creates a new L2 pool (wiping orphaned pools detected via flock) or adopts an existing pool via `KISEKI_CACHE_POOL_ID`. A `kiseki-cache-scrub` service runs on node boot and periodically to clean orphaned pools from crashed processes. |
| I-CC9 | When effective cache policy is unreachable at session start, the client operates with conservative defaults (cache enabled, organic mode, 10 GB ceiling, 5s metadata TTL). Policy is fetched via data-path gRPC (primary), gateway (secondary), persisted last-known (tertiary), or conservative defaults (fallback). |
| I-CC10 | Cache policy changes apply to new sessions only. Active sessions continue under session-start policy (consistent with I-WA18). |
| I-CC11 | Staged chunks are fetched from canonical, verified by content-address, and stored with pinned retention as a point-in-time snapshot. The staged version is immutable in the cache regardless of canonical updates. To pick up updates, the user must explicitly release and re-stage. Staging enumerates directory trees recursively up to `max_staging_depth` (10) and `max_staging_files` (100,000). |
| I-CC12 | On crypto-shred event, all cached plaintext for the affected tenant is wiped from L1 and L2 with zeroize. Detection via periodic key health check (default 30s), advisory channel notification, or KMS error on next operation. Maximum detection latency bounded by `min(key_health_interval, max_disconnect_seconds)`. |
| I-CC13 | L2 cache entries are protected by a CRC32 checksum computed at insert time and stored as a 4-byte trailer. On L2 read, the CRC32 is verified before serving. Mismatch triggers bypass to canonical and L2 entry deletion. |

## Spec references

- `specs/features/native-client.feature` — cache hit/invalidation/staging scenarios (extend)
- `specs/features/control-plane.feature` — cache policy distribution scenarios (extend)
- `specs/invariants.md` — add I-CC1 through I-CC13
- `specs/ubiquitous-language.md` — add cache-specific terms
- `specs/failure-modes.md` — add F-CC1 through F-CC4
- `specs/assumptions.md` — add A-CC1 through A-CC4
