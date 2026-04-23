# Storage Engine Subplan

**Date**: 2026-04-23
**Parent**: `specs/implementation/mvp-to-production-plan.md`
**Workstream items**: 2.1, 2.2, 2.3, 2.4, 2.5, 2.6, 7.2

## Baseline

**kiseki-block** (1,449 lines): `DeviceBackend` trait, `BitmapAllocator`,
`FileBackedDevice`, superblock, device probing. All core I/O works.
Gaps: no O_DIRECT, no TRIM, no WAL journal, no scrub.

**kiseki-chunk** (3,848 lines): `ChunkOps` trait, `ChunkStore` (in-memory),
`PersistentChunkStore`, EC (4+2, 8+3), CRUSH placement, device health
state machine, retention holds, refcount GC. Gaps: no rebalancing,
no evacuation worker, no auto-repair, capacity thresholds not enforced
on write path.

---

## Phase 1: O_DIRECT + Block I/O Hardening (WS 2.2)

**Goal**: Bypass OS page cache for data-path I/O.

### 1.1 IoStrategy enum

File: `kiseki-block/src/backend.rs`

```rust
pub enum IoStrategy {
    Buffered,               // FileBackedDevice (VMs, CI)
    Direct,                 // O_DIRECT + O_DSYNC (NVMe, SSD)
}
```

Auto-detected at device open: raw block device → Direct, regular
file → Buffered.

### 1.2 RawBlockDevice

New file: `kiseki-block/src/raw.rs`

- Opens `/dev/sdX` or `/dev/nvmeXn1` with `O_DIRECT | O_DSYNC`
- Aligned I/O: all reads/writes aligned to `physical_block_size`
- Alignment buffer pool: pre-allocated aligned buffers to avoid
  per-I/O allocation
- Safety checks: refuse init if filesystem signatures detected
  (`check_no_filesystem()` — read first 4KB, check for ext4/xfs/btrfs magic)
- Implements `DeviceBackend` trait

### 1.3 WAL intent journal

File: `kiseki-block/src/journal.rs`

- Bitmap updates journaled in redb before application to on-disk bitmap
- Journal entry: `(extent_offset, extent_length, operation: Alloc|Free)`
- On crash recovery: replay journal to reconstruct consistent bitmap
- Completes I-C8 enforcement

### Validation 1

| Check | Method | CI? |
|-------|--------|-----|
| FileBackedDevice still works (regression) | Existing tests | Yes |
| RawBlockDevice opens loopback device | Integration test (needs root/loop) | Maybe |
| Aligned I/O enforced | Unit test (misaligned write fails) | Yes |
| Filesystem signature rejected | Unit test (write ext4 magic, reject) | Yes |
| Journal replay after crash | Unit test (write journal, skip apply, replay) | Yes |
| CRC32C hardware acceleration | Benchmark (if available) | Manual |

**Effort**: 2-3 sessions

---

## Phase 2: TRIM Batching (WS 2.3)

**Goal**: Return freed extents to NVMe via TRIM/DISCARD.

### 2.1 TrimQueue

New file: `kiseki-block/src/trim.rs`

```rust
pub struct TrimQueue {
    pending: Vec<Extent>,
    max_batch: usize,          // default 256
    flush_interval: Duration,  // default 5s
    last_flush: Instant,
}
```

- `enqueue(extent)` — add freed extent to pending list
- `flush()` — coalesce adjacent extents, issue `BLKDISCARD` ioctl
- `should_flush()` — true if batch full or interval elapsed
- HDD: no-op (TRIM not supported on rotational devices)
- Background task: flush every `flush_interval`

### 2.2 Wire into DeviceBackend

- `DeviceBackend::free()` enqueues extent for TRIM instead of
  immediate discard
- `DeviceBackend::sync()` flushes pending TRIMs
- `FileBackedDevice`: TRIM → `fallocate(FALLOC_FL_PUNCH_HOLE)`
  on Linux, no-op on other platforms

### Validation 2

| Check | Method | CI? |
|-------|--------|-----|
| TrimQueue coalesces adjacent extents | Unit test | Yes |
| Flush interval respected | Unit test | Yes |
| HDD skips TRIM | Unit test (mock rotational device) | Yes |
| fallocate hole punch on file backend | Integration test (Linux) | Yes |

**Effort**: 1 session

---

## Phase 3: Multi-Device EC Striping (WS 2.1)

**Goal**: Distribute EC fragments across distinct physical devices.

### 3.1 Real fragment distribution

Current: EC encode produces fragments, stored in `HashMap`.
Production: fragments placed on distinct devices per I-D4.

File: `kiseki-chunk/src/placement.rs`

- Extend `place_fragments()` to return `Vec<(DeviceId, Extent)>`
- Enforce: no two fragments of same chunk on same device
- Enforce: fragments on distinct failure domains (rack awareness
  via device metadata) when available
- Write fails with `InsufficientDevices` if fewer healthy devices
  than data + parity fragments

### 3.2 Multi-device write path

File: `kiseki-chunk/src/persistent_store.rs`

- `write_chunk()`: EC encode → place fragments → write each to
  its assigned device → record fragment map in metadata
- Fragment map: `HashMap<FragmentIndex, (DeviceId, Extent)>`
- Stored in chunk metadata alongside existing fields

### 3.3 Degraded read path

- `read_chunk()`: try all data fragments. If any missing/corrupt,
  use EC decode with available data + parity fragments
- Return `ChunkError::DegradedRead { missing_fragments }` as warning
  alongside successful data
- Trigger repair if degraded (§5)

### Validation 3

| Check | Method | CI? |
|-------|--------|-----|
| Fragments placed on distinct devices | Unit test (3+ devices) | Yes |
| InsufficientDevices error | Unit test (fewer devices than EC params) | Yes |
| Degraded read succeeds with parity | Unit test (remove 1 fragment) | Yes |
| Fragment map persisted | Unit test (write, reopen, read) | Yes |
| Round-trip with real EC 4+2 | Integration test | Yes |

**Effort**: 2-3 sessions

---

## Phase 4: Pool Rebalancing (WS 2.4)

**Goal**: Migrate chunks between devices when pool crosses Warning.

### 4.1 RebalanceWorker

New file: `kiseki-chunk/src/rebalance.rs`

```rust
pub struct RebalanceWorker {
    pool: PoolId,
    source_devices: Vec<DeviceId>,  // over-capacity
    target_devices: Vec<DeviceId>,  // under-capacity
    rate_limit: BytesPerSec,        // default 100 MB/s
    progress: RebalanceProgress,
}
```

- Scans pool for devices above Warning threshold
- Identifies target devices within same device class (I-C3)
- Migrates chunks: read from source → write to target → update
  fragment map → free source extent
- Rate-limited to avoid saturating I/O
- Interruptible: admin can pause/cancel via control plane
- Progress tracking: chunks moved, bytes moved, estimated remaining

### 4.2 Capacity enforcement on write path

File: `kiseki-chunk/src/persistent_store.rs`

- Check pool capacity before write
- At Warning: log warning, continue writing
- At Critical: reject write with `PoolCapacityExceeded` (I-C5)
- At Full: reject with `ENOSPC`

### Validation 4

| Check | Method | CI? |
|-------|--------|-----|
| Rebalance moves chunks from hot to cold device | Integration test | Yes |
| Rate limiting respected | Unit test (mock I/O counter) | Yes |
| Write rejected at Critical threshold | Unit test | Yes |
| ENOSPC at Full | Unit test | Yes |
| Rebalance interruptible | Unit test (cancel mid-rebalance) | Yes |

**Effort**: 2 sessions

---

## Phase 5: Device Scrub + Auto-Repair (WS 2.6)

**Goal**: Detect and repair corruption, orphans, bitmap inconsistencies.

### 5.1 Scrub engine

New file: `kiseki-block/src/scrub.rs`

```rust
pub struct ScrubReport {
    pub bitmap_errors: u64,       // primary/mirror mismatch
    pub crc_failures: u64,        // corrupt extents
    pub orphan_extents: u64,      // allocated but no chunk_meta
    pub total_extents_checked: u64,
    pub elapsed: Duration,
}
```

Checks:
- Bitmap primary/mirror consistency
- CRC32 on sampled extents (configurable sample rate, default 10%)
- Orphan extent detection: bitmap says allocated, no chunk references it
- Report emitted to audit log

### 5.2 Auto-repair

File: `kiseki-chunk/src/repair.rs`

- When scrub or degraded read detects missing/corrupt fragment:
  - If EC parity available: reconstruct missing fragment → write to
    healthy device → update fragment map
  - If replication: copy from replica → update fragment map
  - Audit log entry with repair details

### 5.3 Device evacuation worker

File: `kiseki-chunk/src/evacuation.rs`

- Triggered by: admin request, SMART wear > 90% (SSD) or > 100 bad
  sectors (HDD), device entering Degraded state
- Migrates all chunks off the device to other pool members
- Progress tracking via `ManagedDevice.evacuation_progress`
- Completion: device transitions to Removed state (I-D5)
- Audit logging of state transitions (I-D2)

### 5.4 Health monitor

File: `kiseki-chunk/src/health_monitor.rs`

- Background task: polls device health every 60s
- Checks: SMART attributes (via sysfs), capacity thresholds, bitmap
  consistency
- Triggers: auto-evacuation (`should_auto_evacuate()`), rebalance
  (pool above Warning), scrub (periodic, default every 24h)
- Device state transitions audited (I-D2)

### Validation 5

| Check | Method | CI? |
|-------|--------|-----|
| Scrub detects bitmap inconsistency | Unit test (corrupt bitmap) | Yes |
| Scrub detects CRC failure | Unit test (corrupt extent) | Yes |
| Scrub detects orphan extent | Unit test (alloc without chunk meta) | Yes |
| Auto-repair reconstructs from parity | Integration test (EC 4+2, remove 1) | Yes |
| Evacuation migrates all chunks | Integration test (multi-device) | Yes |
| Evacuation blocked before completion (I-D5) | Unit test | Yes |
| Health monitor triggers auto-evacuation | Unit test (mock SMART) | Yes |
| Device state transitions audited | Unit test (check audit entries) | Yes |

**Effort**: 3-4 sessions

---

## Phase 6: Storage Failure Validation (WS 7.2)

**Goal**: Validate F-D1 through F-D4 failure modes.

### 6.1 F-D1: Device failure → EC repair

- Simulate device failure (mark device Failed)
- Verify: chunks repaired from EC parity on remaining devices
- Verify: repair completes within bounded time

### 6.2 F-D2: Multiple device failure

- Simulate 2 device failures in a pool with 4+2 EC
- Verify: chunks still readable (2 parity can cover 2 failures)
- Simulate 3 device failures: verify partial data loss (> EC parity)

### 6.3 F-D3: Corrupted extent

- Write corrupt data to a device extent
- Verify: CRC32 detection on read → degraded read from parity
- Verify: auto-repair replaces corrupt fragment

### 6.4 F-D4: Full device

- Fill a device to Critical threshold
- Verify: new writes redirected to other pool members
- Fill to Full: verify ENOSPC returned

### Validation 6

| Check | Method | CI? |
|-------|--------|-----|
| Single device failure → EC repair succeeds | Integration test | Yes |
| Double failure within EC tolerance | Integration test | Yes |
| Triple failure → data loss detected | Integration test | Yes |
| Corrupt extent → CRC detection → repair | Integration test | Yes |
| Full device → writes redirect | Integration test | Yes |
| Full pool → ENOSPC | Integration test | Yes |

**Effort**: 1-2 sessions

---

## Phase Dependency Graph

```
Phase 1 (O_DIRECT + journal)
    │
    ├── Phase 2 (TRIM)
    │
    ├── Phase 3 (EC striping)
    │       │
    │       ├── Phase 4 (rebalance)
    │       │
    │       └── Phase 5 (scrub + repair + evacuation)
    │               │
    │               └── Phase 6 (failure validation)
    │
    └── (Phase 5 depends on Phase 1 for journal + Phase 3 for multi-device)
```

Phase 1 is the foundation. Phases 2 and 3 can proceed in parallel
after Phase 1. Phases 4 and 5 depend on Phase 3. Phase 6 depends
on all prior phases.

## Estimated Total Effort

| Phase | Sessions | Notes |
|-------|----------|-------|
| 1: O_DIRECT + journal | 2-3 | Foundation, touches block layer |
| 2: TRIM batching | 1 | Small, builds on Phase 1 |
| 3: Multi-device EC | 2-3 | Core storage change |
| 4: Pool rebalance | 2 | Background worker |
| 5: Scrub + repair + evacuation | 3-4 | Multiple background workers |
| 6: Failure validation | 1-2 | Integration tests |
| **Total** | **11-15** | |
