# ADR-022: Storage Backend — redb (Pure Rust) + fjall on the Composition Hot Path

**Status**: Accepted (rev-2 amendment 2026-05-06).
**Date**: 2026-04-20 (rev-1 — redb), 2026-05-06 (rev-2 — fjall on composition).
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

- No LSM-tree compaction complexity (rev-1: when redb is the only backend; rev-2 carries an LSM on the composition path)
- No C++ build toolchain required
- Chunk blobs as files: simple, inspectable, compatible with RDMA
- redb's COW B-tree has higher write amplification than LSM for
  high-frequency point writes — see rev-2 amendment below

## Rev-2 amendment (2026-05-06) — fjall on the composition hot path

The escape clause "if redb proves insufficient … migrate to fjall"
fired. The composition store is the highest-throughput metadata
write path in Kiseki — every PUT lands one composition row + one
name-binding row, and the gateway exercises it at every write
amplification level (S3 PUT, NFSv4 CREATE, FUSE write-through).

### Trigger

The 2026-05-05 perf spike pinned redb's per-write commit cost as
the bottleneck on the persistent composition path. Even after the
ADR-040 rev-3 write-behind queue moved the txn off the gateway hot
path, the drainer thread itself bottlenecked at ~18 k op/s PUT in
the `kiseki-profile` `in-process-persistent` driver — the floor
that bounds every wire protocol the production gateway runs.

### Decision

Replace `PersistentRedbStorage` (and the rev-3 write-behind queue
layered on top of it) with `FjallStorage` on the
`kiseki-composition` `CompositionStorage` trait. Wire format
unchanged — postcard-encoded `Composition` values, 16-byte
`(ns_id, name)` keys for the name index — moved to a shared
`persistent::encoding` module so a future swap doesn't reserialize
rows.

The fjall keyspace ships **four columns** (one LSM-tree each):
`comps`, `names`, `names_rev`, `meta`. Atomic batches commit
across all four (cross-keyspace atomicity is fjall's contract),
preserving I-CP1.

Durability model maps 1:1 onto the existing knobs:

| redb (rev-1)                            | fjall (rev-2)                                          |
|----------------------------------------- |--------------------------------------------------------|
| `Durability::Immediate` (per-write fsync)| `PersistMode::SyncAll` (per-write fsync)               |
| `with_eventual_durability(true)`         | `PersistMode::Buffer` + periodic flush task            |
| `RedbFlusher` for `fsync_pending` hook   | `FjallFlusher` for `fsync_pending` hook                |
| Write-behind drainer (rev-3 queue)       | Native LSM memtable + WAL append (no extra queue)      |

The write-behind queue is **deleted** — fjall's memtable is what
the queue was emulating. POSIX `fsync(2)` semantics preserved via
the same `gateway.fsync_pending()` hook chain documented in
`docs/operations/durability.md`.

### Measurement (2026-05-06, single-host, 16-way concurrency, 64 KiB objects)

| shape       | redb baseline (rev-1+rev-3) | fjall (rev-2)        | factor |
|-------------|----------------------------|----------------------|--------|
| put-heavy   | ~18 000 op/s, p99 ~ 5 ms   | 36 324 op/s, p99 1.1 ms | **2.0×** |
| get-heavy   | ~150 000 op/s              | 194 262 op/s, p99 0.5 ms | 1.3×   |
| mixed 70/30 | ~28 000 op/s               | 47 933 op/s, p99 1.1 ms | 1.7×   |

The 2× PUT lift is exactly the bottleneck the rev-3 write-behind
queue was failing to remove. p99 latency improved alongside
throughput because the LSM doesn't pay redb's freed-pages
processing on every commit.

### Scope of the swap (what stayed on redb)

Only the `kiseki-composition` persistent path moved.

- **Raft log store** (`kiseki-raft`'s `RedbLogStore`): stayed on
  redb. Append-mostly workload + snapshot-driven log truncation
  fits redb's COW B-tree well; no measured throughput pressure
  yet.
- **View store** (`kiseki-view`'s `PersistentRedbStorage`):
  stayed on redb. Watermark checkpoints are low-frequency;
  migration would cost more than it returns.
- **Key manager** (`kiseki-keymanager`'s `PersistentKeyStore`),
  **small object store** (`kiseki-chunk`'s `SmallObjectStore`),
  **tuning store** (`kiseki-server`'s `RedbTuningStore`): all
  low-frequency; stay on redb.

A future spike that finds Raft log append capped at the rev-1
floor will reopen this for the log path; the same encoding-module
pattern is in place to make that swap mechanical.

### Rollback path (pre-production stance)

No on-disk migration tool ships — the project is pre-1.0 and
operators wipe + re-replicate from Raft if they need to roll
back. `git revert` on the rev-2 commits is the supported path
(redb implementation is in git history, ~1 354 LOC for
`PersistentRedbStorage` + 636 LOC for the write-behind queue).

## References

- redb: https://github.com/cberner/redb
- fjall: https://github.com/fjall-rs/fjall
- RFC 1813 §3: NFS3 procedure semantics
- build-phases.md Phase 3: "SSTable" storage (now redb B-tree)
- ADR-029: Raw Block Device Allocator (chunk data I/O)
- ADR-040: Persistent metadata stores (composition + view)
- `docs/operations/durability.md`: per-knob loss windows
- 2026-05-05 perf-spike findings: project memory
  `project_2026_05_05_perf_findings.md`
