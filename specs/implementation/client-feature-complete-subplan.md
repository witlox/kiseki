# Client Feature-Complete Subplan

**Date**: 2026-04-23
**Parent**: `specs/implementation/mvp-to-production-plan.md`
**ADR references**: ADR-031 (client-side cache), ADR-020 (workflow advisory),
ADR-013 (POSIX semantics)

## Baseline

`kiseki-client` crate: 1,576 lines across 11 modules. Core FUSE,
batching, prefetch, discovery, and transport selection are implemented
and tested. Two major gaps:

1. **Cache**: current `ClientCache` is a simple in-memory `HashMap` with
   TTL. ADR-031 specifies a two-tier (L1 in-memory + L2 NVMe) cache
   with three modes, staging, policy resolution, integrity verification,
   crash recovery, and crypto-shred detection.

2. **Client bindings**: FFI stubs return `NotConnected`. Python module
   is empty. No staging CLI.

3. **Advisory integration**: ADR-020 client-side hooks (workflow
   declaration, hint emission, backpressure) not implemented.

4. **FUSE**: single-level directory only, no nested dirs, no write-at-
   offset, no cache-mode mount options.

Scenario coverage: 19/40 native-client.feature scenarios plausibly
handleable; 21 need new implementation.

---

## Phase 1: Cache Engine (ADR-031 core)

**Goal**: Replace `cache.rs` with two-tier L1/L2 cache engine.

### 1.1 L1 cache (in-memory)

Replace `ClientCache` with `CacheL1`:
- `HashMap<ChunkId, Zeroizing<Vec<u8>>>` — plaintext with zeroize
- LRU eviction at `max_memory_bytes` (default 256 MB)
- Access-count tracking for promotion decisions
- No TTL on chunk data (chunks are immutable, I-CC1)

### 1.2 L2 cache (NVMe)

New `CacheL2`:
- Per-process pool directory: `$KISEKI_CACHE_DIR/<tenant>/<pool_id>/chunks/`
- Pool ID: 128-bit CSPRNG (same generation as `client_id`)
- `pool.lock` flock for ownership
- Chunk files: plaintext + 4-byte CRC32 trailer (I-CC13)
- File permissions: `0600`
- Read path: verify CRC32, serve; mismatch → delete + bypass (I-CC7)
- Write path: SHA-256 verify at fetch, compute CRC32, write file
- Capacity tracking: disk usage counter, `max_cache_bytes` enforcement
- Node-wide capacity check: sum all pools in `$KISEKI_CACHE_DIR`

### 1.3 Metadata cache

New `MetadataCache`:
- `HashMap<CompositionId, MetadataEntry>` with TTL
- `MetadataEntry { chunk_list: Vec<ChunkId>, fetched_at: Instant, ttl: Duration }`
- Write-through: own writes update immediately
- TTL expiry → re-fetch from canonical (I-CC3)

### 1.4 Cache modes

`CacheMode` enum: `Pinned`, `Organic`, `Bypass`
- `Organic`: L1 + L2 with LRU eviction
- `Pinned`: L1 + L2, chunks retained until explicit release
- `Bypass`: no caching, pass-through to canonical

### 1.5 Unified `CacheManager`

Orchestrates L1, L2, metadata cache:
```rust
pub struct CacheManager {
    mode: CacheMode,
    l1: CacheL1,
    l2: Option<CacheL2>,       // None in bypass mode
    meta: MetadataCache,
    config: CacheConfig,
    metrics: CacheMetrics,
}
```

Public API:
- `get_chunk(chunk_id) -> Option<Vec<u8>>` — L1 → L2 → None
- `put_chunk(chunk_id, data)` — insert into L1 + L2
- `get_metadata(composition_id) -> Option<Vec<ChunkId>>`
- `put_metadata(composition_id, chunk_list)`
- `invalidate_chunk(chunk_id)`
- `wipe()` — zeroize all L1 + L2
- `stats() -> CacheStats`

### Validation 1

| Check | Method | CI? |
|-------|--------|-----|
| L1 cache hit avoids L2 read | Unit test | Yes |
| L2 cache hit with CRC32 verify | Unit test | Yes |
| CRC32 mismatch returns None | Unit test | Yes |
| LRU eviction at capacity | Unit test | Yes |
| Metadata TTL expiry | Unit test | Yes |
| Write-through metadata | Unit test | Yes |
| Bypass mode skips all storage | Unit test | Yes |
| Pinned chunks survive LRU | Unit test | Yes |
| Zeroize on eviction | Unit test (check zeroed buffer) | Yes |
| Pool directory created with 0600 | Unit test | Yes |

**Effort**: 4-5 sessions

---

## Phase 2: Staging API

**Goal**: Pre-populate cache for declared datasets.

### 2.1 Stage/release operations

Add to `CacheManager`:
```rust
async fn stage(&self, ns_path: &str, gateway: &G) -> Result<StageResult>;
fn stage_status(&self) -> Vec<StagedDataset>;
fn release(&self, ns_path: &str);
fn release_all(&self);
```

Flow: resolve path → enumerate compositions (recursive, bounded by
`max_staging_depth` and `max_staging_files`) → fetch + verify chunks →
write manifest.

### 2.2 Staging handoff

`KISEKI_CACHE_POOL_ID` env var: on process start, adopt existing pool
instead of creating new. Verify flock can be acquired (staging daemon
releases). Staging daemon: `kiseki-client stage --daemon` writes
pool_id to stdout, holds flock, stages chunks, waits for adopter.

### 2.3 CLI commands

Binary entry point in `kiseki-client`:
```
kiseki-client stage --dataset <path> [--daemon] [--timeout <s>]
kiseki-client stage --status [--dataset <path>]
kiseki-client stage --release <path>
kiseki-client stage --release-all
kiseki-client cache --stats
kiseki-client cache --wipe
```

### Validation 2

| Check | Method | CI? |
|-------|--------|-----|
| Stage single file | Integration test | Yes |
| Stage directory tree | Integration test | Yes |
| Stage depth limit exceeded | Unit test | Yes |
| Stage file count limit exceeded | Unit test | Yes |
| Release frees pinned chunks | Unit test | Yes |
| Idempotent re-stage | Unit test | Yes |
| Pool handoff via env var | Integration test | Yes |
| CLI stage --status output | Integration test | Yes |

**Effort**: 2-3 sessions

---

## Phase 3: Cache Policy + Key Health

**Goal**: Resolve cache policy from data-path, detect crypto-shred.

### 3.1 Policy resolution

New `CachePolicyResolver`:
- Fetch via `GetCachePolicy` RPC on data-path gRPC channel
- Fallback chain: data-path → gateway → persisted `policy.json` →
  conservative defaults
- Policy clamping: client selections bounded by admin ceilings

### 3.2 Key health check

Background tokio task:
- Ping KMS every `key_health_interval` (default 30s)
- On `KEK_DESTROYED`: trigger `cache_manager.wipe()` (I-CC12)
- On KMS unreachable: start disconnect timer

### 3.3 Disconnect detection

Background heartbeat:
- Track `last_successful_rpc` timestamp
- Heartbeat every 60s (piggyback on metadata refresh)
- `max_disconnect_seconds` exceeded → full cache wipe (I-CC6)

### Validation 3

| Check | Method | CI? |
|-------|--------|-----|
| Policy resolved from mock data-path | Unit test | Yes |
| Policy fallback to defaults | Unit test | Yes |
| Client clamped to policy ceiling | Unit test | Yes |
| Key health detects KEK_DESTROYED | Unit test (mock KMS) | Yes |
| Disconnect timer triggers wipe | Unit test (simulated time) | Yes |
| Policy persisted to pool directory | Unit test | Yes |

**Effort**: 2-3 sessions

---

## Phase 4: Cache Scrub Service

**Goal**: Standalone binary for crash recovery cleanup.

### 4.1 `kiseki-cache-scrub` binary

New binary crate (or feature-gated binary in `kiseki-client`):
- Scan `$KISEKI_CACHE_DIR` for all `<tenant>/<pool_id>/pool.lock`
- For each: attempt `flock(LOCK_EX | LOCK_NB)`
  - Success → pool is orphaned → zeroize chunk files → delete directory
  - EWOULDBLOCK → pool is live → skip
- Log actions via tracing
- Exit with count of cleaned pools

### 4.2 Systemd unit

`kiseki-cache-scrub.service` + `kiseki-cache-scrub.timer`:
- OnBoot + every 60s
- One-shot service
- Configurable via `KISEKI_CACHE_DIR` env var

### Validation 4

| Check | Method | CI? |
|-------|--------|-----|
| Scrub detects orphaned pool | Unit test (create pool, drop lock, run scrub) | Yes |
| Scrub skips live pool | Unit test (hold lock, run scrub) | Yes |
| Scrub zeroizes before delete | Unit test (check file contents) | Yes |
| Scrub handles empty dir | Unit test | Yes |

**Effort**: 1 session

---

## Phase 5: FFI Implementation

**Goal**: Wire C FFI stubs to real gateway client + cache.

### 5.1 `KisekiHandle` internals

```rust
struct KisekiHandleInner {
    cache: CacheManager,
    gateway: Arc<dyn GatewayOps>,
    discovery: DiscoveryClient,
    runtime: tokio::runtime::Runtime,
}
```

### 5.2 Wire all C functions

- `kiseki_open()` → create runtime, discover, init cache, return handle
- `kiseki_close()` → wipe cache, shutdown runtime
- `kiseki_read()` → cache.get_chunk or gateway.read
- `kiseki_write()` → gateway.write, cache.put_metadata
- `kiseki_stat()` → gateway stat or metadata lookup
- `kiseki_stage()` → cache.stage()
- `kiseki_release()` → cache.release()

### 5.3 C header update

Update `include/kiseki_client.h` with staging functions and cache config.

### Validation 5

| Check | Method | CI? |
|-------|--------|-----|
| kiseki_open / kiseki_close lifecycle | Unit test via FFI | Yes |
| kiseki_read returns data | Integration test | Yes |
| kiseki_stage populates cache | Integration test | Yes |
| Header matches implementation | cbindgen check | Yes |

**Effort**: 2-3 sessions

---

## Phase 6: Python Bindings

**Goal**: PyO3 module wrapping cache + staging + read/write API.

### 6.1 PyO3 module

```python
import kiseki

client = kiseki.Client(
    seeds="10.0.0.1:9100",
    cache_mode="pinned",
    cache_dir="/tmp/kiseki",
    cache_l2_max=50 * 1024**3,
)
data = client.read("/training/imagenet/shard-0001.bin")
client.stage("/training/imagenet")
client.release("/training/imagenet")
stats = client.cache_stats()
client.close()
```

### 6.2 Async support

PyO3 with `pyo3-asyncio` for staging (long-running) operations.
Sync wrappers for simple read/write.

### Validation 6

| Check | Method | CI? |
|-------|--------|-----|
| Python import works | Python unit test | Yes |
| Read/write roundtrip | Python integration test | Yes |
| Stage/release lifecycle | Python integration test | Yes |
| Cache stats accessible | Python unit test | Yes |

**Effort**: 2-3 sessions

---

## Phase 7: FUSE Hardening

**Goal**: Production-ready FUSE mount.

### 7.1 Nested directories

Current `KisekiFuse` supports only single-level (parent=1).
Extend inode table to track directory trees across shards.

### 7.2 Write-at-offset

Currently only offset=0 writes. Extend to support arbitrary
offset writes (needed for checkpoint/restart patterns).

### 7.3 Mount options for cache

Wire cache config into FUSE mount options:
`-o cache_mode=organic,cache_l2_max=50G,cache_dir=/tmp/kiseki,meta_ttl=5000`

### Validation 7

| Check | Method | CI? |
|-------|--------|-----|
| mkdir/readdir in nested dirs | Unit test | Yes |
| Write at non-zero offset | Unit test | Yes |
| Mount options parsed correctly | Unit test | Yes |
| Read from subdirectory | Integration test | Yes |

**Effort**: 1-2 sessions

---

## Phase 8: Workflow Advisory Integration

**Goal**: ADR-020 client-side hooks.

### 8.1 WorkflowSession handle

```rust
pub struct WorkflowSession {
    workflow_id: WorkflowId,
    client_id: ClientId,
    phase: AtomicU64,
    advisory_channel: Option<AdvisoryChannel>,
}
```

### 8.2 Advisory channel

gRPC bidi stream to `WorkflowAdvisoryService`:
- `DeclareWorkflow` → receive pool handles, profile confirmation
- `PhaseAdvance` → monotonic phase progression
- Hint emission (fire-and-forget, I-WA1)
- Telemetry subscription (backpressure, locality, prefetch effectiveness)

### 8.3 Pattern-detector advisory integration

Wire `PrefetchAdvisor` to emit access-pattern hints when advisory
channel is available. Graceful degradation when advisory disabled
(I-WA12) or channel unavailable (I-WA2).

### 8.4 Backpressure handling

On `hard` backpressure telemetry: client throttles write rate.
On `soft`: client logs warning, continues at reduced rate.
Advisory outage: no effect on data path (I-WA2).

### Validation 8

| Check | Method | CI? |
|-------|--------|-----|
| DeclareWorkflow returns session | Unit test (mock advisory) | Yes |
| PhaseAdvance monotonic enforcement | Unit test | Yes |
| Hint emission fire-and-forget | Unit test | Yes |
| Advisory outage → data path unaffected | Unit test | Yes |
| Advisory disabled → graceful degradation | Unit test | Yes |
| Backpressure throttle | Unit test | Yes |
| PrefetchAdvisor emits hints when channel available | Unit test | Yes |

**Effort**: 3-4 sessions

---

## Phase Dependency Graph

```
Phase 1 (cache engine) ─────────────────────────┐
    │                                            │
    ├── Phase 2 (staging) ──┐                    │
    │                       ├── Phase 5 (FFI) ───┤
    ├── Phase 3 (policy)    │       │            │
    │                       │       ▼            │
    ├── Phase 4 (scrub)     │   Phase 6 (Python) │
    │                       │                    │
    └── Phase 7 (FUSE) ────┘                    │
                                                 │
Phase 8 (advisory) ─────────────────────────────┘
    (independent, can start after Phase 1)
```

Phase 1 is the critical path. Phases 2-4 and 7-8 can proceed in
parallel after Phase 1. Phases 5-6 depend on Phase 2 (staging API).

## Estimated Total Effort

| Phase | Sessions | Notes |
|-------|----------|-------|
| 1: Cache engine | 4-5 | Critical path, largest piece |
| 2: Staging API | 2-3 | Depends on Phase 1 |
| 3: Policy + key health | 2-3 | Depends on Phase 1 |
| 4: Scrub service | 1 | Small, standalone |
| 5: FFI | 2-3 | Depends on Phases 1-2 |
| 6: Python | 2-3 | Depends on Phase 5 |
| 7: FUSE hardening | 1-2 | Depends on Phase 1 |
| 8: Advisory integration | 3-4 | Independent of Phases 2-7 |
| **Total** | **17-24** | |

## CI Integration

- All phases: `cargo fmt --check && cargo clippy -- -D warnings && cargo test`
- Phase 4: new binary target `kiseki-cache-scrub`
- Phase 5: FFI tests (compile C test against generated header)
- Phase 6: Python tests via `maturin develop` + `pytest`
- BDD scenario count: target 40/40 native-client.feature scenarios
