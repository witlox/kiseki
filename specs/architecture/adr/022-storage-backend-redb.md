# ADR-022: Storage Backend — fjall on Hot Paths, redb on Low-Frequency Stores

**Status**: Accepted (rev-4 amendment 2026-05-06).
**Date**: 2026-04-20 (rev-1 — redb), 2026-05-06 (rev-2 — fjall on
composition, rev-3 — fjall on the Raft log, rev-4 — fjall on chunk
+ fragment meta).
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

### Scope of the rev-2 swap (what stayed on redb)

Only the `kiseki-composition` persistent path moved in rev-2.
rev-3 below extends the swap to the Raft log path.

- **View store** (`kiseki-view`'s `PersistentRedbStorage`):
  stayed on redb. Watermark checkpoints are low-frequency;
  migration would cost more than it returns.
- **Small object store** (`kiseki-chunk`'s `SmallObjectStore`)
  and **tuning store** (`kiseki-server`'s `RedbTuningStore`):
  low-frequency; stay on redb.

### Rollback path (pre-production stance)

No on-disk migration tool ships — the project is pre-1.0 and
operators wipe + re-replicate from Raft if they need to roll
back. `git revert` on the rev-2 commits is the supported path
(redb implementation is in git history, ~1 354 LOC for
`PersistentRedbStorage` + 636 LOC for the write-behind queue).

## Rev-3 amendment (2026-05-06) — fjall on the Raft log path

Following rev-2's win on the composition path, the same swap
landed on the Raft log:

- `RedbLogStore` (the generic key-value persistence wrapper used
  by `PersistentShardStore`, `PersistentKeyStore`,
  `PersistentAuditStore`) → `FjallLogStore`. Same public surface
  (`open`/`append`/`get`/`range`/`truncate_*`/`set_meta`/
  `get_meta`/`last_index`/`len`).
- `RedbRaftLogStore` (the openraft `RaftLogStorage` +
  `RaftLogReader` impl) → `FjallRaftLogStore`. Thin wrapper, same
  trait shape; consumers (cluster_control, log shards, audit,
  keymanager) updated in-place.

### Trigger

Same theory as rev-2: redb's per-write COW B-tree commit cost
dominates CPU at the rates the multi-shard Raft transport drives.
Each Raft append calls `begin_write` + `insert` + `commit` with
`Durability::Immediate` — one fsync, one freed-pages pass, one
B-tree mutation per entry. fjall replaces this with one
`PersistMode::SyncAll` per `WriteBatch::commit` (one fsync, one
WAL append, one memtable insert) and amortizes the LSM compaction
off the hot path.

### Schema

Two fjall keyspaces inside one database, matching the
two-redb-table layout 1:1:

- `raft_log`  — `u64.to_be_bytes()` (log index) → JSON-encoded entry
- `raft_meta` — UTF-8 key (`vote`, `committed`, `last_purged`, …)
                 → JSON-encoded value

Big-endian u64 keys give the LSM range iterator the same monotonic
traversal redb's native `u64` ordering provided.

### Path layout change

fjall is a directory keyspace, not a single file. Paths drop the
`.redb` extension:

| rev-1 / rev-2                               | rev-3                              |
|---------------------------------------------|------------------------------------|
| `<data_dir>/raft/log.redb`                   | `<data_dir>/raft/log/`             |
| `<data_dir>/raft/cluster_control.redb`       | `<data_dir>/raft/cluster_control/` |
| `<data_dir>/raft/keymanager.redb`            | `<data_dir>/raft/keymanager/`      |
| `<data_dir>/raft/audit.redb`                 | `<data_dir>/raft/audit/`           |
| `<data_dir>/raft/shard-{uuid}.redb`          | `<data_dir>/raft/shard-{uuid}/`    |
| `<data_dir>/keys/epochs.redb`                | `<data_dir>/keys/epochs/`          |

The runtime's canonical-path test (`runtime::tests::
store_layout_paths_are_distinct_and_under_data_dir`) was updated
to assert the new layout. The persistence.feature `Background`
step uses the new path shape.

### Measurement (single-host, single-thread, release)

Append-throughput microbench
(`crates/kiseki-raft/tests/raft_log_microbench.rs`) on
`FjallLogStore::append` with `PersistMode::SyncAll`:

| entry size | throughput     |
|------------|----------------|
| 64 B       | 237 932 op/s   |
| 512 B      | 101 199 op/s   |
| 4 KiB      |  27 248 op/s   |

For comparison, redb's per-fsync `WriteTransaction::commit` lands
in the ~5–15 k op/s range for small entries on the same hardware
(measured during rev-2 spike); the microbench above shows fjall
clearing 10× the throughput at 512 B (the typical Raft entry
size for client writes).

### Scope of the rev-3 swap (what stayed on redb)

- **View store** (`kiseki-view`'s `PersistentRedbStorage`):
  watermark checkpoints, low-frequency.
- **Small object store** (`kiseki-chunk`'s `SmallObjectStore`):
  ADR-030 inline-data path, low-frequency.
- **Tuning store** (`kiseki-server`'s `RedbTuningStore`):
  ADR-025 cluster tuning params, low-frequency.

These are kept on redb because the cost of migration would not
return any measurable performance lift; their bottleneck (if any)
is upstream contention, not commit cost.

### Rollback path (rev-3)

`git revert` of the rev-3 commits. The redb implementation
(`RedbLogStore`, `RedbRaftLogStore` — together ~625 LOC) is in
git history. As with rev-2, no on-disk migration tool ships —
operators wipe + re-replicate from Raft if a rollback is needed.

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

## Rev-4 amendment (2026-05-06) — fjall on the chunk + fragment meta path

The `PersistentChunkStore::save_meta` / `save_frag_meta` JSON-rewrite-
the-world pattern was the next floor under composition + Raft log.
Every chunk mutation rewrote the entire metadata table to JSON, atomic-
renamed it, and called `device.sync()`. O(N) per write in store size,
which capped the in-process-persistent PUT bench at ~36 k op/s — the
ceiling stayed flat past concurrency 4 because workers all queued for
the next save_meta turn.

### Decision

Move chunk + fragment meta off JSON to fjall (ADR-022 rev-4). Same
backend the composition store and the Raft log use (rev-2 / rev-3).
Pools (~10 entries, admin-rate mutations) stay out of fjall — moved
to a `DashMap` for sharded concurrent reads on the per-PUT
durability-strategy lookup, persisted to a small `pools.json` file
on the rare admin mutation.

### What changed

- `PersistentChunkStore::save_meta` + `save_frag_meta` deleted.
  Each mutating op (`write_chunk`, `increment_refcount`,
  `decrement_refcount`, `set_retention_hold`, `release_retention_hold`,
  `gc`, `write_fragment`, `delete_fragment`, `delete_chunk_force`)
  now writes ONE record through `FjallMetaStore` — O(1) per op.
- New module `kiseki-chunk::persistent` mirrors the
  `kiseki-composition::persistent` layout: `encoding` (wire format,
  schema-versioned postcard records, single source of truth) +
  `fjall_meta` (`FjallMetaStore` impl + `FjallMetaFlusher` off-thread
  fsync handle).
- Two fjall keyspaces inside one database: `chunks` (32-byte
  `chunk_id` keys → `ChunkRecord`) + `fragments` (36-byte
  `chunk_id || fragment_index_be` keys → `FragmentRecord`).
- `delete_chunk_force` uses one fjall WriteBatch across both
  keyspaces — strictly stronger atomicity than the prior
  sequential JSON pair where a crash between the two rewrites left
  hung state.
- `pools: Mutex<HashMap>` → `pools: DashMap`. The per-PUT
  `pools.get(pool)` durability-strategy lookup no longer serialises
  every fabric writer through one global lock.

### Path layout change

| pre-rev-4                                  | rev-4                          |
|--------------------------------------------|--------------------------------|
| `<data_dir>/chunks/data.dev`                | `<data_dir>/chunks/data.dev` (unchanged) |
| `<data_dir>/chunks/meta.json`               | `<data_dir>/chunks/meta/`      |
| `<data_dir>/chunks/meta.json.frag`          | (folded into the same fjall keyspace) |
| (none — pools were memory-only)             | `<data_dir>/chunks/pools.json` |

The runtime canonical-paths layout test
(`runtime::tests::store_layout_paths_are_distinct_and_under_data_dir`)
counts 5 paths now (up from 4) — `chunks/meta` + `chunks/data.dev`
collapse into the same first-level subdir.

### Measurement (single-host, in-process-persistent floor, 64 KiB)

Scaling sweep — the previous plateau at concurrency ≥ 4 is gone:

| concurrency | rev-3 (JSON save_meta) | rev-4 (fjall) | factor |
|-------------|------------------------|---------------|--------|
| 1           |  16 728 op/s           |  23 095 op/s   | 1.4×   |
| 2           |  31 892 op/s           |  44 262 op/s   | 1.4×   |
| 4           |  36 893 op/s           |  81 082 op/s   | 2.2×   |
| 8           |  35 408 op/s           | **125 299 op/s** | **3.5×** |
| 16          |  35 655 op/s           |  98 747 op/s   | 2.8×   |
| 32          |  35 242 op/s           |  98 788 op/s   | 2.8×   |
| 64          |   —                    |  99 201 op/s   |        |

Per-shape (c=16, 15s):

| shape       | rev-3        | rev-4               | factor | p99 rev-3 | p99 rev-4 |
|-------------|--------------|---------------------|--------|-----------|-----------|
| put-heavy   |  35 215 op/s | **95 203 op/s**     | **2.7×** | 1.0 ms | 0.6 ms |
| get-heavy   | 193 940 op/s | 192 229 op/s        | 1.0×   | 0.5 ms | 0.5 ms |
| mixed 70/30 |  47 196 op/s | **104 468 op/s**    | **2.2×** | 1.0 ms | 0.6 ms |

GET unchanged (reads never went through `save_meta`). Writes lifted
2–3× as expected once the O(N) JSON rewrite was gone. p99 latency
also dropped on the write paths because workers no longer queue for
the JSON-serialise + write+rename critical section.

### Scope of the rev-4 swap (what stayed on redb)

- **View store** (`kiseki-view`'s `PersistentRedbStorage`):
  watermark checkpoints, low-frequency.
- **Small object store** (`kiseki-chunk`'s `SmallObjectStore`):
  ADR-030 inline-data path, low-frequency.
- **Tuning store** (`kiseki-server`'s `RedbTuningStore`):
  ADR-025 cluster tuning params, low-frequency.

These stay on redb because the cost of migration would not return
any measurable performance lift; their bottleneck (if any) is
upstream contention, not commit cost.

### Rollback path (rev-4)

`git revert` of the rev-4 commits. The pre-rev-4 implementation
(JSON save_meta + save_frag_meta + Mutex<HashMap> pools, ~280 LOC
in `persistent_store.rs`) is in git history. As with rev-2 / rev-3,
no on-disk migration tool ships — operators wipe + re-replicate
from peers if a rollback is needed (the deployment target is 10–100+
nodes with R-3 / EC-4+2; per-node loss windows are recoverable from
the cluster).
