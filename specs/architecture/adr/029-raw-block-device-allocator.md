# ADR-029: Raw Block Device Allocator

**Status**: Accepted
**Adversarial review**: 2026-04-22 (8 findings: 2H 4M 2L, all resolved)
**Date**: 2026-04-22
**Context**: ADR-022, ADR-024, ADR-005, I-C1 through I-C6

## Problem

Chunk ciphertext needs to persist on JBOD data devices. ADR-024
specifies XFS on each device as the default, but filesystem overhead
becomes the bottleneck at HPC scale:

- **Double journaling**: XFS journals its metadata, then redb journals
  ours — redundant durability cost
- **Page cache pollution**: OS caches data we already manage in our
  own cache layer, wasting DRAM
- **Inode contention**: Billions of chunks = billions of inodes;
  XFS metadata operations become the throughput ceiling
- **Indirection**: Every I/O traverses VFS → XFS → block layer →
  device; raw access removes two layers

Ceph's migration from FileStore (XFS) to BlueStore (raw block) was
driven by exactly these issues. DAOS uses SPDK for the same reason.

## Decision

### New crate: `kiseki-block`

A device I/O crate that manages raw block devices (and file-backed
fallback for VMs/CI). Separate from `kiseki-chunk` (domain logic).
`kiseki-chunk` depends on `kiseki-block` for storage.

### Device Backend Trait

```rust
/// Abstraction over a storage device — raw block or file-backed.
/// Auto-detects device characteristics and adapts I/O strategy.
#[async_trait]
pub trait DeviceBackend: Send + Sync {
    /// Allocate a contiguous extent of at least `size` bytes.
    /// Alignment matches the device's physical block size.
    fn alloc(&self, size: u64) -> Result<Extent, AllocError>;

    /// Write data at the given extent.
    fn write(&self, extent: &Extent, data: &[u8]) -> Result<(), BlockError>;

    /// Read data from the given extent.
    fn read(&self, extent: &Extent) -> Result<Vec<u8>, BlockError>;

    /// Free an extent, returning blocks to the free pool.
    fn free(&self, extent: &Extent) -> Result<(), AllocError>;

    /// Sync all pending writes to stable storage.
    fn sync(&self) -> Result<(), BlockError>;

    /// Device capacity: (used_bytes, total_bytes).
    fn capacity(&self) -> (u64, u64);

    /// Probed device characteristics (read-only after open).
    fn characteristics(&self) -> &DeviceCharacteristics;
}
```

### Auto-detection (no manual configuration)

On `DeviceManager::open(path)`, probe sysfs (Linux):

```
/sys/block/<dev>/queue/rotational         → 0 (SSD/NVMe) or 1 (HDD)
/sys/block/<dev>/queue/physical_block_size → 512 or 4096
/sys/block/<dev>/queue/optimal_io_size    → device-preferred I/O size
/sys/block/<dev>/queue/max_hw_sectors_kb  → max single I/O size
/sys/block/<dev>/device/model             → model string
/sys/block/<dev>/device/numa_node         → NUMA node (-1 if none)
/sys/block/<dev>/queue/discard_max_bytes  → TRIM support (>0 = yes)
```

Derived properties:

```rust
pub struct DeviceCharacteristics {
    pub medium: DetectedMedium,
    pub physical_block_size: u32,
    pub optimal_io_size: u32,
    pub rotational: bool,
    pub numa_node: Option<u32>,
    pub supports_trim: bool,
    pub supports_smart: bool,
    pub io_strategy: IoStrategy,
}

pub enum DetectedMedium {
    NvmeSsd,       // /sys/block/nvme*/ + rotational=0
    SataSsd,       // rotational=0, not NVMe
    Hdd,           // rotational=1
    Virtual,       // virtio in model, no SMART
    Unknown,
}

pub enum IoStrategy {
    DirectAligned,       // O_DIRECT | O_DSYNC — NVMe, SATA SSD
    BufferedSequential,  // O_SYNC — HDD (readahead benefits)
    FileBacked,          // Default flags — VM, dev, CI
}
```

For non-Linux / VMs without sysfs: detect `virtio` in model string
or absence of block device properties → fall back to
`IoStrategy::FileBacked` with sparse file. All three strategies
implement the same `DeviceBackend` trait transparently.

### On-disk format

Per data device:

```
Offset 0:     [Superblock — 4K]
Offset 4K:    [Primary Bitmap — variable size]
Offset M:     [Mirror Bitmap — same size as primary]
Offset N:     [Data Region — remainder of device]
```

**Superblock** (4K, first block):

```rust
pub struct Superblock {
    pub magic: [u8; 8],              // b"KISEKI\x01\x00"
    pub version: u32,                // Format version (1)
    pub device_id: [u8; 16],         // UUID
    pub block_size: u32,             // Physical block size (probed)
    pub total_blocks: u64,           // Device capacity in blocks
    pub bitmap_offset: u64,          // Byte offset of primary bitmap
    pub bitmap_mirror_offset: u64,   // Byte offset of mirror bitmap
    pub bitmap_blocks: u64,          // Size of each bitmap in blocks
    pub data_offset: u64,            // Byte offset of data region
    pub generation: u64,             // Monotonic, incremented on bitmap flush
    pub checksum: [u8; 32],          // SHA-256 of superblock fields
}
```

**Allocation bitmap** (primary + mirror): 1 bit per block in the
data region. Stored twice at different offsets for redundancy.
- At 4K blocks: 4TB device = 1 billion blocks = 128MB × 2 = 256MB
- At 512B blocks: 4TB device = 8 billion blocks = 1GB × 2 = 2GB
- Bitmap overhead: 0.006% (4K) to 0.048% (512B)
- On read: verify primary against mirror. On mismatch, use the
  copy consistent with the redb journal.

**Per-extent CRC32**: Every data extent has a 4-byte CRC32 trailer
written after the payload data (within the same aligned block).
- On read: verify CRC32 before returning data.
- CRC mismatch → hardware corruption → trigger EC repair from
  parity fragments (not a security incident).
- AES-GCM auth_tag failure after CRC pass → actual tampering
  (security incident, alert + audit).
- This distinguishes hardware failure from cryptographic attack,
  enabling correct operational response.

### Allocation algorithm

**Extent-based best-fit with free-list cache** (Ceph BlueStore
pattern, simpler than DAOS VEA):

- **In-memory**: B-tree of free extents `(offset, block_count)`,
  sorted by offset. On alloc, scan for smallest extent >=
  requested blocks. On free, insert and coalesce with neighbors.
- **Concurrency**: `alloc()` and `free()` are serialized per device
  via Mutex on the allocator state. This is acceptable — allocation
  is a B-tree lookup (microseconds); I/O is the bottleneck, not
  allocation. Ceph BlueStore also serializes allocation per OSD.
- **On-disk**: Bitmap is ground truth. Free-list rebuilt from
  bitmap on startup (~100ms for 4TB at 4K blocks).
- **Crash safety**: Bitmap updates journaled in redb
  (`device_alloc` table) before applied to device bitmap region.
  On crash recovery: reload bitmap from device, replay pending
  journal entries from redb, rebuild free-list.

Allocation flow (WAL-ordered for crash safety):
1. Round up requested size to `block_size` boundary
2. Search free-list for best-fit extent
3. Split extent if larger than needed
4. **Journal intent** in redb (`device_alloc` table: alloc intent)
5. Mark bits in bitmap (pwrite to bitmap region)
6. Return `Extent { offset, length }`
7. Caller writes data to extent, then commits `chunk_meta` to redb
8. **Clear intent** from `device_alloc` journal (write complete)

On crash recovery: scan `device_alloc` for pending intents. If
the corresponding `chunk_meta` exists → write completed, clear
intent. If no `chunk_meta` → write was interrupted, free the
extent (clear bitmap bits, remove intent). This is the standard
WAL pattern — Ceph BlueStore uses the same approach.

Free flow:
1. Journal the deallocation intent in redb
2. Clear bits in bitmap
3. Insert freed extent into free-list, coalesce neighbors
4. If `supports_trim`: add to TRIM batch queue (see below)
5. Clear dealloc intent from journal

**TRIM batching**: Freed extents accumulate in a TRIM queue per
device. A batched `BLKDISCARD` ioctl is issued periodically
(every 60 seconds or when queue exceeds 1GB). This avoids
write amplification from many small TRIM commands.

**Maximum extent size**: 16MB. Allocations larger than 16MB are
split into multiple extents. `FragmentLocation` in `chunk_meta`
already supports multiple extents per chunk via `Vec<FragmentLocation>`.

### I/O strategy per device type

| Strategy | Open flags | Alignment | Sync | Use case |
|----------|-----------|-----------|------|----------|
| `DirectAligned` | `O_DIRECT \| O_DSYNC` | `physical_block_size` | Implicit (O_DSYNC) | NVMe, SATA SSD |
| `BufferedSequential` | `O_SYNC` | 512B | `fdatasync()` | HDD |
| `FileBacked` | default | 4K (simulated) | `fsync()` | VM, dev, CI |

**FileBacked alignment**: `FileBackedDevice` enforces the same 4K
alignment as `RawBlockDevice` to ensure tests faithfully reproduce
raw block behavior. Code that passes CI will not fail on real
hardware due to alignment issues.

- Write buffers aligned via `std::alloc::Layout::from_size_align`
  for O_DIRECT compatibility
- NUMA-aware: pin allocator thread to `numa_node` if detected
- TRIM/UNMAP on free if `supports_trim` (SSD wear management)
- `optimal_io_size` used for write batching (coalesce small writes
  up to this size before issuing I/O)

### Metadata in redb (system partition)

ADR-022's redb on the RAID-1 system partition stores chunk metadata:

**Table: `chunk_meta`**
```
Key:   [u8; 32]  (chunk_id)
Value: bincode-serialized ChunkMeta {
    refcount: u64,
    retention_holds: Vec<String>,
    pool_name: String,
    stored_bytes: u64,
    fragments: Vec<FragmentLocation {
        device_id: [u8; 16],
        offset: u64,
        length: u64,
    }>,
    envelope_meta: EnvelopeMeta {
        nonce: [u8; 12],
        auth_tag: [u8; 16],
        system_epoch: u64,
        tenant_epoch: Option<u64>,
        tenant_wrapped_material: Option<Vec<u8>>,
    },
}
```

**Table: `device_alloc`** (bitmap journal for crash safety)
```
Key:   (device_id: [u8; 16], generation: u64)
Value: bincode-serialized Vec<AllocJournalEntry {
    offset: u64,
    length: u64,
    is_alloc: bool,  // true = allocate, false = free
}>
```

### Separation of concerns

The allocator does NOT know about device subclasses (`NvmeU2` vs
`NvmeQlc`, `HddEnterprise` vs `HddBulk`). Those are pool/placement
concerns in `kiseki-chunk` and `kiseki-control` (ADR-024).

| Layer | Cares about | Doesn't care about |
|-------|------------|-------------------|
| `kiseki-block` | physical_block_size, rotational, O_DIRECT | TLC vs QLC, RPM, pool policy |
| `kiseki-chunk` | pool thresholds, EC config, placement | block alignment, I/O flags |
| `kiseki-control` | device class, pool assignment, tiering | how bytes reach the device |

The `DeviceClass` enum (ADR-024) stays in `kiseki-chunk`/`kiseki-control`.
`DeviceCharacteristics` (auto-probed) stays in `kiseki-block`.

### Integration with existing code

- `ChunkOps` trait (ADR-005) unchanged — callers unaware of backend
- New `PersistentChunkStore` in `kiseki-chunk` implements `ChunkOps`:
  - `write_chunk()`: EC encode → alloc extents per device via
    `DeviceBackend` → write fragments → update redb `chunk_meta`
  - `read_chunk()`: lookup redb `chunk_meta` → `DeviceBackend::read`
    per fragment → EC decode if needed → return Envelope
  - `gc()`: free extents via `DeviceBackend::free` → update bitmap
    → remove from redb
- `DeviceManager` in `kiseki-block` opens devices at startup, probes
  characteristics, creates appropriate `DeviceBackend` per device
- Server runtime (`kiseki-server`) wires `DeviceManager` → pools →
  `PersistentChunkStore` when `KISEKI_DATA_DIR` is set

### Crate structure

```
kiseki-block/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── backend.rs        # DeviceBackend trait
    ├── raw.rs            # RawBlockDevice (O_DIRECT)
    ├── file.rs           # FileBackedDevice (sparse file)
    ├── probe.rs          # Sysfs device probing
    ├── superblock.rs     # On-disk superblock format
    ├── bitmap.rs         # Allocation bitmap
    ├── allocator.rs      # Extent allocator (free-list + bitmap)
    ├── extent.rs         # Extent type
    ├── manager.rs        # DeviceManager
    └── error.rs          # BlockError, AllocError
```

## Rationale

- **Raw block over XFS**: Eliminates FS overhead (journaling, inode,
  page cache) that becomes the bottleneck at NVMe line rate. Ceph
  BlueStore validated this approach at scale.
- **Auto-detection over manual config**: Reduces deployment friction.
  Admin provides device paths; Kiseki probes characteristics.
  Works correctly on bare metal, VMs, and CI without config changes.
- **Bitmap over B-tree free-list on disk**: Simpler crash recovery
  (fixed-size, position-indexed). Free-list is derived in-memory.
  DAOS VEA uses B-tree on persistent memory, but we don't require
  PMEM — bitmap on block device with redb journal is sufficient.
- **File-backed fallback**: Same trait, different backend. Tests and
  CI don't need raw devices. VMs work without device passthrough.
- **Separate crate**: `kiseki-block` has no domain knowledge (chunks,
  EC, pools). Clean dependency boundary. Testable in isolation.

## Alternatives Considered

1. **XFS on each JBOD device** (ADR-024 original default): Rejected
   for production — FS overhead at NVMe line rate is unacceptable.
   Still available as `FileBacked` strategy for dev/VM.

2. **SPDK userspace I/O** (DAOS model): Rejected — requires dedicated
   devices (no kernel access), complicates deployment, needs custom
   memory management (DMA buffers). Future optimization path if
   kernel I/O overhead is measured as bottleneck.

3. **Pool files (one large file per device)**: Rejected — still has
   FS overhead (XFS metadata for the pool file itself). Raw block
   eliminates the FS entirely.

4. **redb for chunk data**: Rejected — B-tree not designed for
   multi-GB blob storage. Acceptable for metadata only.

## Consequences

- Adds `kiseki-block` crate to workspace (~2000 lines estimated)
- Data devices must be provisioned as raw (no filesystem). Operator
  provides device paths in config; Kiseki writes superblock on init.
- VMs and CI use file-backed mode transparently (no raw devices needed)
- Crash recovery depends on redb journal + device bitmap consistency
- Device initialization is a destructive operation (writes superblock,
  bitmap — existing data on device is lost). Safety checks before
  init: (1) check for existing Kiseki superblock magic — require
  `--force` if found, (2) check for known FS signatures (XFS, ext4,
  NTFS magic) — refuse with clear error, (3) audit log the init
- TRIM/UNMAP support improves SSD endurance but is optional
- Future: SPDK backend can implement `DeviceBackend` trait for
  userspace I/O without changing upper layers

## Adversarial Review Findings (2026-04-22)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | High | Write ordering — data before metadata creates phantom chunks on crash | WAL intent journal: alloc → journal intent → write data → commit chunk_meta → clear intent. Recovery replays intents. |
| 2 | High | No per-extent checksum — silent corruption indistinguishable from tampering | CRC32 trailer per extent. CRC fail = hardware corruption (EC repair). Auth tag fail after CRC pass = tampering (security alert). |
| 3 | Medium | Bitmap single point of failure per device | Primary + mirror bitmap at different offsets. On mismatch, use copy consistent with redb journal. |
| 4 | Medium | No device init safety — accidental overwrite of existing data | Safety checks: existing Kiseki magic → require --force. Known FS signatures → refuse. Audit log init. |
| 5 | Medium | File-backed mode doesn't enforce alignment — CI misses bugs | FileBacked enforces same 4K alignment as RawBlockDevice. |
| 6 | Medium | Concurrent alloc race on shared free-list | Mutex per device on allocator state. Allocation is microseconds; I/O is the bottleneck. |
| 7 | Low | Immediate TRIM on free causes write amplification | Batch TRIM queue: accumulate, issue BLKDISCARD every 60s or at 1GB threshold. |
| 8 | Low | No max extent size — unbounded alloc fragments bitmap scan | Max extent 16MB. Larger chunks split into multiple extents. |

## References

- Ceph BlueStore: [Architecture](https://docs.ceph.com/en/reef/rados/configuration/bluestore-config-ref/)
- DAOS VOS/VEA: [Storage Model](https://docs.daos.io/master/overview/storage/)
- ADR-022: Storage backend (redb for metadata)
- ADR-024: Device management and capacity thresholds
- ADR-005: EC and chunk durability
