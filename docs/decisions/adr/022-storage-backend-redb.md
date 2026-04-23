# ADR-022: Storage Backend — redb (Pure Rust)

**Status**: Accepted.
**Date**: 2026-04-20.
**Deciders**: Architect + implementer.

## Context

The system needs persistent storage for:
1. **Raft log entries** — append-heavy, sequential reads for replay
2. **State machine snapshots** — periodic full-state serialization
3. **Chunk metadata index** — key-value mapping (chunk_id → placement, refcount)
4. **View watermark checkpoints** — small, frequently updated

The spec references "RocksDB or equivalent" (build-phases.md Phase 3)
but does not commit to a specific engine. RocksDB is C++ and brings
~200MB build dependency via cmake/clang/librocksdb.

## Decision

Use **redb** v2 for all structured persistent storage.

### What redb handles

| Data | redb Table | Key | Value |
|------|-----------|-----|-------|
| Raft log entries | `raft_log` | `u64` (log index) | bincode-serialized entry |
| Raft vote/term | `raft_meta` | `&str` ("vote", "term") | `u64` |
| State machine snapshot | `sm_snapshot` | `"latest"` | bincode-serialized state |
| Chunk metadata | `chunk_meta` | `[u8; 32]` (chunk_id) | bincode ChunkMeta |
| Device allocation | `device_alloc` | `(DeviceId, u64)` (device + offset) | `[u8; 32]` (chunk_id) — reverse index |
| View watermarks | `view_wm` | `[u8; 16]` (view_id) | `u64` (sequence) |

### What redb does NOT handle

**Chunk ciphertext data** is written directly to raw block devices
(or file-backed fallback for VMs/CI) via the `DeviceBackend` trait
in `kiseki-block` (ADR-029). redb stores metadata only; chunk
ciphertext never passes through redb.
```
$KISEKI_DATA_DIR/
  devices/
    /dev/nvme0n1          # raw block device (default, ADR-029)
    /dev/nvme1n1          # raw block device
    /tmp/kiseki-dev0.img  # file-backed fallback (VMs/CI)
  raft/
    db.redb               # redb database file (metadata only)
```

redb tracks chunk placement: `chunk_meta` table maps
`chunk_id → (device_id, offset, size, fragment_index)`.
The `device_alloc` table provides a reverse index
`(device_id, offset) → chunk_id` for bitmap rebuild and scrub.
Bitmap allocation updates are journaled in redb before application
to the on-device bitmap (ADR-029).

**Why pool files, not per-chunk files**:
- At 100TB / 64KB avg = 1.6B chunks → filesystem inode exhaustion
- Pool files support O_DIRECT and RDMA pre-registration (single mmap region)
- Chunks are 4KB-aligned within the pool file for NVMe block alignment
- Pool file is sparse: only allocated regions consume disk space

### EC fragment placement (CRUSH-like)

Fragments placed across devices via deterministic hashing:
```
fn place_fragment(chunk_id, frag_idx, pool_devices) -> DeviceId {
    // Ensure no two fragments on same device
    let mut candidates = pool_devices.clone();
    for prior in 0..frag_idx {
        candidates.remove(placed[prior]);
    }
    candidates[hash(chunk_id, frag_idx) % candidates.len()]
}
```
Deterministic — can recalculate placement without storing it.
Reverse index `(device_id, chunk_id) → fragment_index` in redb
enables efficient repair on device failure.

### Raft snapshots

- **Trigger**: Every 10,000 log entries
- **Format**: `bincode::serialize(&state_machine_inner)`
- **Storage**: redb `sm_snapshot` table, key = `"latest"`
- **Restore**: Deserialize snapshot → replay log entries after snapshot index
- **Log cleanup**: Truncate entries before snapshot index after snapshot

## Rationale

| Criterion | redb | RocksDB | fjall | Custom files |
|-----------|------|---------|-------|-------------|
| Pure Rust | Yes | No (C++) | Yes | Yes |
| Build deps | None | cmake, clang, librocksdb | None | None |
| Binary size | ~50KB | ~5MB | ~100KB | 0 |
| ACID | Yes (COW) | Yes (WAL) | Yes (WAL) | Manual (fsync) |
| Crash recovery | Automatic | Automatic | Automatic | Manual replay |
| Compaction | None needed (B-tree) | Required (LSM) | Required (LSM) | None |
| Maturity | 1.0, used by Firefox | Very mature | Newer | N/A |
| Write amplification | Low (COW) | High (LSM) | High (LSM) | Low |

redb wins on simplicity, zero deps, and sufficient performance for
Raft log append + metadata lookup.

## Consequences

- No LSM-tree compaction complexity
- No C++ build toolchain required
- Chunk blobs as files: simple, inspectable, compatible with RDMA
- redb's COW B-tree has higher read amplification than LSM for
  range scans — acceptable for our workload (point lookups + append)
- If redb proves insufficient for high-throughput Raft log append,
  migrate to fjall (LSM, same API pattern)

## References

- redb: https://github.com/cberner/redb
- RFC 1813 §3: NFS3 procedure semantics
- build-phases.md Phase 3: "SSTable" storage (now redb B-tree)
- ADR-029: Raw Block Device Allocator (chunk data I/O)
