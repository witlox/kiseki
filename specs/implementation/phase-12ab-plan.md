# Phase 12a+12b: Persistent Raft Log + Small-File Placement

## Context

Two remaining features share the redb persistence layer and should be
implemented together:

- **12b (Persistent Raft Log)**: Replace `MemLogStore` with redb-backed
  log store so Raft state (entries, vote, membership) survives restart.
  Currently a restarted node loses all Raft state and must catch up via
  snapshot transfer.

- **12a (ADR-030)**: Small-file content stored in `small/objects.redb`
  on the metadata tier instead of chunk extents on block devices. Gateway
  routes based on inline threshold. State machine offloads inline payloads
  to redb on apply.

Both touch `OpenRaftLogStore`, `RaftShardStore`, and `runtime.rs`. The
persistent Raft log is a prerequisite for correct inline content replay
on restart.

## Implementation Steps

### Step 1: `RedbRaftLogStore<C>` (kiseki-raft)

New file: `crates/kiseki-raft/src/redb_raft_log_store.rs`

- Wraps existing `RedbLogStore` (composition)
- Implements `RaftLogStorage<C>` + `RaftLogReader<C>` from openraft
- Stores log entries in `LOG_TABLE`, vote/committed/last_purged in `META_TABLE`
- Add `truncate_after(after: u64)` to `RedbLogStore` (currently only has `truncate_before`)
- Export from `crates/kiseki-raft/src/lib.rs`
- Tests: persist/reload vote, entries, purge, truncate across reopen

### Step 2: Wire into OpenRaftLogStore (kiseki-log)

Modify: `crates/kiseki-log/src/raft/openraft_store.rs`

- `OpenRaftLogStore::new()` gains `data_dir: Option<&Path>` parameter
- When `data_dir` is Some: use `RedbRaftLogStore`, skip `raft.initialize()` if log already has state
- When `data_dir` is None: use `MemLogStore` (backward compatible)
- Handle type polymorphism via enum wrapper in `crates/kiseki-log/src/raft/log_store.rs`:
  `enum ShardLogStoreBackend { Mem(MemLogStore<C>), Redb(RedbRaftLogStore<C>) }`

Modify: `crates/kiseki-log/src/raft_shard_store.rs`
- Add `data_dir: Option<PathBuf>` to `RaftShardStore::new()`
- Pass through to `OpenRaftLogStore::new()` in `create_shard()`

Modify: `crates/kiseki-server/src/runtime.rs`
- Pass `cfg.data_dir` to `RaftShardStore::new()`

### Step 3: `SmallObjectStore` (kiseki-chunk) — parallel with Step 2

New file: `crates/kiseki-chunk/src/small_object_store.rs`

- Redb-backed KV store: `ChunkId → encrypted content bytes`
- API: `open(path)`, `put(chunk_id, data)`, `get(chunk_id)`, `delete(chunk_id)`, `len()`
- Export from `crates/kiseki-chunk/src/lib.rs`
- Tests: put/get roundtrip, delete, persistence across reopen

### Step 4: State machine offload on apply

Modify: `crates/kiseki-log/src/raft/state_machine.rs`

- `ShardSmInner` gains `small_store: Option<Arc<SmallObjectStore>>`
- In `apply_command(AppendDelta { has_inline_data: true, .. })`:
  - Write payload to `small_store.put(chunk_id, &payload)`
  - Clear `delta.payload.ciphertext` from memory (I-SF5)
- `build_snapshot()`: read inline content back from SmallObjectStore
- `install_snapshot()`: write inline content to SmallObjectStore

Modify: `crates/kiseki-log/src/raft/openraft_store.rs`
- Pass `Arc<SmallObjectStore>` into `ShardSmInner::new()` when `data_dir` is Some

### Step 5: Gateway threshold routing

Modify: `crates/kiseki-gateway/src/mem_gateway.rs`

- Add `inline_threshold: u64` field to `InMemoryGateway`
- In `write()`: if `req.data.len() <= inline_threshold`, set `has_inline_data=true`
  on the delta request, include encrypted payload in payload field. Otherwise use
  existing chunk path.
- In `read()`: if `chunks.read_chunk()` returns NotFound, try `small_store.get()`

Alternative (cleaner): `CompositeChunkStore` wrapping `SmallObjectStore` + `Box<dyn ChunkOps>`,
implementing `ChunkOps` by checking small store first. Created at server boot.

### Step 6: System disk detection + runtime wiring

Modify: `crates/kiseki-server/src/config.rs`
- Add `meta_soft_limit_pct: u8` (default 50), `meta_hard_limit_pct: u8` (default 75)
- Parse from `KISEKI_META_SOFT_LIMIT_PCT`, `KISEKI_META_HARD_LIMIT_PCT`

New: `crates/kiseki-server/src/system_disk.rs`
- `detect_media_type(path) -> MediaType` (sysfs on Linux, fallback Unknown)
- `NodeMetadataCapacity` struct with budget computation
- HDD warning at boot

Modify: `crates/kiseki-server/src/runtime.rs`
- Detect system disk at boot
- Create `SmallObjectStore` at `data_dir/small/objects.redb`
- Create `CompositeChunkStore` (SmallObjectStore + existing chunk store)
- Pass `data_dir` to `RaftShardStore`

### Step 7: GC integration

Modify: `crates/kiseki-log/src/raft/openraft_store.rs`

- In `truncate_log()` and `compact_shard()`: before removing deltas with
  `has_inline_data=true`, call `small_store.delete(chunk_id)`
- Fulfills I-SF6

## Files touched

| File | Steps | Change type |
|------|-------|-------------|
| `kiseki-raft/src/redb_log_store.rs` | 1 | Add `truncate_after()` |
| `kiseki-raft/src/redb_raft_log_store.rs` | 1 | **New** |
| `kiseki-raft/src/lib.rs` | 1 | Export |
| `kiseki-log/src/raft/log_store.rs` | 2 | Enum wrapper |
| `kiseki-log/src/raft/openraft_store.rs` | 2,4,7 | data_dir param, SmallObjectStore, GC |
| `kiseki-log/src/raft/state_machine.rs` | 4 | Offload on apply, snapshot |
| `kiseki-log/src/raft_shard_store.rs` | 2 | data_dir field |
| `kiseki-chunk/src/small_object_store.rs` | 3 | **New** |
| `kiseki-chunk/src/lib.rs` | 3 | Export |
| `kiseki-gateway/src/mem_gateway.rs` | 5 | Threshold routing |
| `kiseki-server/src/config.rs` | 6 | Meta limit fields |
| `kiseki-server/src/system_disk.rs` | 6 | **New** |
| `kiseki-server/src/runtime.rs` | 2,6 | Wiring |

## Parallelism

Steps 2 and 3 can run in parallel (different crates).
Steps 5, 6, 7 touch different files but have logical deps on Step 4.

## Verification

1. `cargo fmt --check && cargo clippy -- -D warnings` — no regressions
2. `cargo test --workspace` — all existing tests pass
3. BDD: 530+ scenarios still green (data_dir=None path unchanged)
4. New unit tests per step (see above)
5. E2e: restart scenario — write inline files, restart, read back
6. E2e: multi-node — inline content replicated via Raft, readable on followers
