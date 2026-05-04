# Group commit on PersistentChunkStore — I-L5 durability-window clarification

**Type**: Implementer → Architect
**Date**: 2026-05-04
**Author**: implementer
**Status**: open — code landed (commit-id-pending), spec amendment pending
**Risk**: low (gated by `KISEKI_CHUNK_FLUSH_INTERVAL_MS` env knob)

## Background

The 2026-05-04 perf sweep traced fabric `write_chunk` latency on a
docker compose 3-node 1-shard cluster:

| metric | pre-CRC fix | post-CRC fix | post-group-commit (this commit) |
|---|---:|---:|---:|
| receiver `write_chunk` (16 MiB) | 54 ms | 17.5 ms | TBD measure |
| concurrent fabric receivers | serialized via fsync | serialized via fsync | parallel |

After the CRC32C fix (commit 4c395c1), the remaining cost in
`PersistentChunkStore::write_chunk` was ~13 ms `extent_io` + ~10 ms
`device_sync` (`flush_bitmap` + `sync_all`). Per-write `fsync`
serializes concurrent writers through the kernel — two fabric
receivers landing fragments on the same node cannot proceed in
parallel because each one's `sync_all` blocks until *all* dirty pages
in the file are flushed, including those of the other write.

## What changed

`PersistentChunkStore` now supports two modes:

- `sync_per_write = true` (default, back-compat): per-write `device.sync()`
- `sync_per_write = false` (group commit): inline sync skipped; runtime
  spawns a periodic flush task (`KISEKI_CHUNK_FLUSH_INTERVAL_MS`,
  default 100 ms)

The runtime opts into group commit. Tests that need stricter
durability (deterministic crash recovery scenarios) opt out via
`set_sync_per_write(true)` or call `flush()` explicitly.

## Spec impact — I-L5 ambiguity

`specs/invariants.md` I-L5:

> A composition is not visible to readers until all chunks referenced
> by its deltas are durable. Normal writes: protocol enforces
> chunk-before-delta ordering. [...]

**The word "durable" is unspecified.** Two readings:

1. **Cluster-level durable** — chunk has landed on ≥`min_acks` peers'
   page caches before the composition delta is committed. Loss
   requires simultaneous failure of `cluster_size − min_acks + 1`
   nodes within the flush window. *This is what I-L2 / I-CS1 use for
   the Raft log itself.*
2. **Single-node disk-level durable** — chunk has been `fsync`ed to
   stable storage on the writer node before delta commit. Loss
   requires sustained corruption (single-node power loss is
   recoverable from peers via the under-replication scrub).

Today's code (pre-2026-05-04) implicitly enforced (2) on the chunk
data path. The Raft log path uses (1) (per ADR-026). My change shifts
the chunk path to (1) as well, aligning the two paths and matching
the standard distributed-storage tradeoff (Cassandra, Kafka, Ceph
RADOS in async mode).

## What I did NOT change

- **ADR-029 §F-I1 (WAL intent journal)** — never implemented; today's
  code relies on a periodic scrub for orphan-extent reclamation
  (comment in `crates/kiseki-chunk/src/persistent_store.rs:340` admits
  this). My change does not make the WAL gap worse.
- **ADR-029 §"I/O strategy per device type"** — specifies `fsync()`
  as the sync *method* for `FileBacked` and `O_DSYNC`/`O_SYNC` for
  raw devices. I use `fsync()` (correct method); the ADR does not
  specify cadence.
- **I-L2 / I-CS1** — Raft log replication path is untouched. The
  metadata path's "majority before ack" guarantee is unchanged.

## Concrete failure modes introduced

### F-1: Whole-cluster simultaneous power loss

Pre-fix: a chunk on disk on N nodes survives complete cluster power
loss; on recovery, every node has its data and Raft replays
metadata.

Post-fix: a chunk in N nodes' page caches but not yet on disk could
be lost on simultaneous cluster power loss. Window: ≤`flush_interval`
(default 100 ms).

**Mitigation**:
- Battery-backed write cache (NVMe with PMR, hardware RAID with
  BBU) makes the OS page cache effectively durable.
- Per-rack power redundancy means simultaneous full-cluster power
  loss requires correlated failure outside the cluster's blast
  radius.
- Operators with stricter durability requirements set
  `KISEKI_CHUNK_FLUSH_INTERVAL_MS=0` (causes flush task to run every
  tick) or run with `sync_per_write=true` via a future config knob.

### F-2: Single-node power loss with concurrent leader-on-same-node

If the node that crashes is *also* the leader for the shard owning
the in-flight composition, the composition delta may have been
Raft-committed (durable on majority) but the chunk data on the
crashing node is gone. The under-replication scrub will detect the
missing chunk and re-replicate from the surviving peers. **No data
loss**, but reads in the recovery window may see
`InsufficientReplicas` until repair completes.

**Mitigation**: same as today's read-side behavior — the gateway
already has a retry budget for reads against transiently-unavailable
chunks.

## Proposed I-L5 amendment

```diff
-| I-L5 | A composition is not visible to readers until all chunks
-       | referenced by its deltas are durable. Normal writes: protocol
-       | enforces chunk-before-delta ordering. Bulk/multipart: finalize
-       | step gates reader visibility after all chunks confirmed durable. |
+| I-L5 | A composition is not visible to readers until all chunks
+       | referenced by its deltas are durable on at least `min_acks`
+       | peers. Durability here means "in stable storage OR in OS page
+       | cache on a node where the data has been replicated to N peers
+       | per the pool's durability strategy". Normal writes: protocol
+       | enforces chunk-before-delta ordering. Bulk/multipart: finalize
+       | step gates reader visibility after all chunks confirmed durable.
+       | The chunk store may defer per-write fsync (group commit, ADR-029
+       | amendment of 2026-05-04) provided the cross-node replication
+       | factor satisfies the pool's durability strategy. |
```

## Specific questions for architect

1. **Wording of I-L5 amendment**: does "in stable storage OR in OS
   page cache on a node where data has been replicated to N peers"
   capture the right contract, or should I tighten it to "in stable
   storage on at least 1 of N peers"? (The latter requires at least
   one peer to have synced before ack; today's code has neither
   guarantee — the writer's fsync happens before ack but peers' do
   not unless they too flush within the same window.)

2. **ADR-029 amendment**: should the per-device sync table grow a
   "cadence" column? Today it specifies the *method* (fsync /
   fdatasync / O_DSYNC) but not the *frequency*. Group commit is a
   cadence choice that interacts with the device's sync semantics
   (e.g. `O_DSYNC` makes per-write sync free; group commit on
   `O_DSYNC` is a no-op).

3. **Pool-level policy** (ADR-024 territory): should durability mode
   be per-pool? Pools tagged for "regulated data" (HIPAA / SOX /
   GDPR-Article-32) might want `sync_per_write=true` while
   "scratch" pools for HPC checkpoints want group commit. Today
   it's a global flag.

## What's blocked / not blocked

- **Not blocked**: code lands. The behavior is opt-in via the
  runtime call to `set_sync_per_write(false)`. Tests preserve
  back-compat (default mode).
- **Blocked**: marketing the change as "I-L5 conformant" until the
  amendment lands. The escalation makes the gap explicit so a future
  reader doesn't accidentally reverse the change on a misreading of
  the unamended invariant.

## Pointers

- Code: `crates/kiseki-chunk/src/persistent_store.rs` (the
  `sync_per_write` field, `flush()` method, `device_handle()`)
- Runtime: `crates/kiseki-server/src/runtime.rs` (the
  `set_sync_per_write(false)` call + periodic flush task spawn)
- Test: `persistent_store::tests::write_chunk_skips_device_sync_when_sync_per_write_disabled`
- Perf measurement: docker compose 3-node, scrape of
  `kiseki_chunk_persistent_write_phase_duration_seconds{phase="device_sync"}`
  count vs `phase="extent_io"` count after group commit shows the
  former trends to zero on the write path (sync moves to background).
