# Durability

This document is the operator's reference for what each performance
knob trades against data durability. **Read this before turning on any
of the group-commit flags** — most of them shift latency from per-write
fsync to a periodic background flush, which means a power loss can
discard writes that the application has already seen succeed.

**Production target is 10-100+ nodes** with Replication-3 or EC-4+2 (or
larger codes). At that scale Raft replication + the under-replication
scrub recover any per-node loss window automatically — the group-commit
flags are a pure throughput win with no operator-visible durability
cost. The single-node and 3-node configurations documented below are
dev / smoke-test setups; they tolerate the group-commit loss window
because they don't carry production data, not because anyone has
designed them to be safe under power loss.

---

## Default behavior

With **no env vars set**, Kiseki provides the following durability
guarantees:

| Operation | Durability |
|-----------|------------|
| `gateway.write()` returns | composition row + chunk fragments are on disk (fsync per write) |
| `gateway.delete()` returns | tombstone is on disk |
| FUSE / NFS `close(2)` | best-effort flush — POSIX permits losing writes that weren't `fsync`'d |
| FUSE / NFS `fsync(2)` | data is on disk before the call returns |
| Process crash / SIGKILL | no loss — kernel still owns the dirty pages |
| Power loss / kernel panic | no loss — every accepted write was fsynced |

This is the conservative configuration. It is also the slowest. The
local single-node FUSE put-heavy benchmark hits ~2 800 op/s under this
mode (May 2026 matrix). Group commit moves the ceiling to ~10 000+
op/s in exchange for a bounded loss window.

---

## Performance flags that trade durability for throughput

> Looking for `KISEKI_DISABLE_PNFS_LAYOUT`? It's a perf-correctness
> knob (NFSv4.1 fast-path workaround), not a durability knob —
> documented in [`performance.md`](performance.md#kiseki_disable_pnfs_layout-force-mds-only-nfsv4).


### `KISEKI_CHUNK_FLUSH_INTERVAL_MS` (chunk-store flush interval)

| Aspect | Value |
|---|---|
| Default interval | `100` ms |
| Group commit | **always on** when `KISEKI_DATA_DIR` is set (since `681de37`) |
| Disable group commit | not exposed — would require code change to `set_sync_per_write(true)` |
| Recommended for production | default (`100` ms) |
| Recommended for perf bench | default |

When `KISEKI_DATA_DIR` is set, the chunk store ALWAYS runs in group-
commit mode: per-write fsync is skipped, and a periodic flush task
issues one `device.sync()` every N ms. The env var only tunes N.
The gateway's `fsync_pending` hook also drives an explicit
`device.sync()` so FUSE / NFS callers using `fsync(2)` still get
POSIX-compliant durability.

This was landed in `681de37` because per-write fsync was serializing
parallel chunk writers through the kernel's writeback queue (5
parallel 16 MiB PUTs went from ~620 ms total to ~370 ms after the
fix). Reverting to per-write fsync is not exposed via env var
because no production workload has been identified that benefits
from it — the under-replication scrub handles the loss window
mitigation on multi-node, and single-node deployments without
replication shouldn't be hosting durable data anyway.

**Loss window on power loss:** up to N ms of chunk data that the
gateway accepted but the kernel hadn't yet flushed.

**Multi-node mitigation:** the under-replication scrub (`kiseki-chunk-
cluster::scrub_scheduler`) walks `cluster_chunk_state` rows on
restart. Any chunk whose local copy is missing gets re-fetched from
peers via `GetFragment`. For Replication-3 with at most one
simultaneous power loss, every lost chunk has 2 surviving copies.
For EC-4+2 the under-replication scrub reconstructs from parity.
At the production scale (10-100+ nodes) simultaneous loss across a
quorum is the dominant constraint, not the per-node loss window.

**Single-node:** dev / smoke-test only. Loss window is a feature of
the group-commit pattern; not a target.

---

### `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS` (composition redb group commit)

| Aspect | Value |
|---|---|
| Default | unset (per-write fsync via `Durability::Immediate`) |
| Recommended value when set | `100` ms |
| Recommended for production | unset on single-node, `100` on multi-node |
| Recommended for perf bench | `100` |

When set, the composition redb runs at `Durability::None` — every
`txn.commit()` skips fsync and stays in the WAL. A periodic task
issues one `Immediate`-durability commit every N ms, forcing the
WAL to disk. The gateway's `fsync_pending` hook drives an immediate
flush so FUSE / NFS `fsync(2)` callers are POSIX-compliant.

This was added to fix the FUSE p99 = 160 ms tail observed in the
May 2026 single-node matrix — root cause was per-write composition
redb fsync contending with the chunk store's periodic device.sync
on the same disk.

**Loss window on power loss:** up to N ms of composition metadata
that the gateway accepted. The chunks are durable (per the chunk-
store group-commit guarantee above), but the `composition_id →
chunks` mapping may be lost — meaning the user-visible file is
unrecoverable even though the bytes are on disk.

**Multi-node mitigation:** the composition hydrator
(`kiseki-composition::hydrator`) replays Raft-committed deltas on
restart. Any composition whose local copy is missing gets
reconstructed from the per-shard Raft log replayed by peers. At
production scale this means the per-node loss window is invisible
to applications.

**Single-node:** dev only — same caveat as the chunk flag.

---

### `KISEKI_RAFT_FLUSH_INTERVAL_MS` (Raft log group commit)

| Aspect | Value |
|---|---|
| Default | unset (per-write fsync via `PersistMode::SyncAll`) |
| Recommended value when set | `100` ms |
| Recommended for production | `100` (multi-node Raft re-replicates the loss window) |
| Recommended for perf bench | `100` |

When set, the Raft log (`kiseki-log::persistent_store::PersistentShardStore`)
opens via the `open_eventual` constructor — every log append commits
with `PersistMode::Buffer` (queues in the WAL, no fsync) and a
periodic background task drives one `PersistMode::SyncAll` every
N ms. When unset, every Raft append fsyncs inline.

Per the inline comment in `runtime.rs`: **"PUT path was fsync-bound
at ~31 k op/s with sync-per-write"** — setting this knob is what
unlocks the in-process-persistent matrix's 125 k op/s. Equivalent
shape to the chunk + composition group-commit flags above; landed
in `d5c56ad` (perf: eventual durability for FjallLogStore via
flush interval).

**Loss window on power loss:** up to N ms of Raft log entries
that the leader accepted but hadn't yet flushed. Followers may have
seen those entries via AppendEntries replication and persisted them;
quorum durability still holds at the cluster level even when the
leader's local copy is briefly behind disk.

**Multi-node mitigation:** Raft replication itself is the answer.
On restart, a node that lost the last 100 ms of its log catches up
from peers via the standard append-entries / install-snapshot
machinery before it can vote or serve reads. Production is safe.

**Single-node:** dev only. Same caveat as the chunk + composition
flags — single-node group commit moves writes the application
already saw succeed into the loss window.

---

### `KISEKI_OBSERVABILITY=off` (perf-cluster opt-out)

| Aspect | Value |
|---|---|
| Default | unset (on) |
| Set to `off` / `0` / `false` / `disabled` | bypass `InstrumentedLogOps` + `InstrumentedKeyManager` wrappers |
| Recommended for production | unset (on) |
| Recommended for perf bench | `off` (clean baseline) |

Disables the metric-record + tracing-span wrappers around `LogOps` and
`KeyManagerOps` calls. Pure observability change — **does not affect
durability**. Use for perf-cluster runs (`infra/gcp/transport`) where
you need a clean baseline.

The hot-path `#[tracing::instrument]` macros are already at
`level = "debug"`, so production `RUST_LOG=info` / `warn` settings
already short-circuit span creation. The wrappers themselves cost
~100 ns per call (one atomic counter increment + one histogram
observation). For a 100 k op/s workload that's ~0.01 % overhead;
turn off only when measuring against a sub-µs baseline.

---

### `KISEKI_HYDRATOR_TRANSIENT_RETRIES` (composition hydrator backoff)

| Aspect | Value |
|---|---|
| Default | `5` |
| Range | any non-negative integer |
| Recommended for production | default |

Number of retries before the hydrator declares a delta `stuck` and
halts. Lower values fail faster on a malformed delta; higher values
absorb more transient failures. **Does not affect durability of
already-applied deltas** — only how aggressively the hydrator gives
up on a problematic one. A `stuck` hydrator surfaces via the
`kiseki_composition_hydrator_halted` gauge.

---

### `KISEKI_RAFT_THREADS` (Raft worker pool size)

| Aspect | Value |
|---|---|
| Default | `max(num_cpus / 2, 4)` |
| Recommended | default |

Worker thread count for the dedicated Raft tokio runtime. **Does not
affect durability** — every Raft write still goes through the
configured per-shard log store (redb-backed when `KISEKI_DATA_DIR`
is set, in-memory otherwise). Tuning is purely a CPU-allocation
trade-off between Raft and the data-path runtime.

---

## fsync(2) semantics

POSIX defines `close(2)` and `fsync(2)` differently:

- **`close(2)`**: releases the file descriptor. No durability
  guarantee. POSIX explicitly permits losing data that was written
  but not `fsync`'d before close.
- **`fsync(2)`**: blocks until all of this file's data and metadata
  is on stable storage.

Kiseki's FUSE driver follows POSIX:

- `FUSE_FLUSH` (called on `close(2)`) drains the dirty buffer through
  the gateway but does **not** force fsync on the backing stores.
  Group commit's bounded loss window applies.
- `FUSE_FSYNC` (called on `fsync(2)`) drains the dirty buffer **and**
  drives `gateway.fsync_pending()` — every registered fsync hook
  runs (composition redb + chunk-store device). The call only
  returns once data is durable.

Apps that need durability must call `fsync(2)` after critical writes.
This matches the contract of every other Linux filesystem (ext4,
btrfs, XFS); apps that rely on `close(2)` for durability are buggy
even on those filesystems.

NFS clients get the same behavior via the `COMMIT` operation
(`NFSv3::commit`, `NFSv4::commit`) — the server-side handler invokes
the same `gateway.fsync_pending()` path. (NFSv3 with `O_SYNC` writes
also forces durability per write.)

---

## Failure-mode matrix

Three columns to track:
- **Default** = `KISEKI_DATA_DIR` set, no other env vars. Chunk group
  commit is on (always — see above), composition + Raft log are at
  per-write fsync.
- **+composition GC, single-node** = `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100`.
- **+composition GC, multi-node R-3** = same env var on a 3-node
  Replication-3 cluster.

`KISEKI_RAFT_FLUSH_INTERVAL_MS=100` widens the per-node loss window
to also cover the last ≤ 100 ms of Raft log appends. On multi-node
R-3 the recovery story is identical to the composition column —
followers refill from peers via AppendEntries on restart, so the
cluster-level durability is unchanged. On single-node it adds the
Raft loss window to the existing chunk + composition windows; not
recommended outside dev / perf-bench.

| Failure | Default | +composition GC, single-node | +composition GC, multi-node R-3 |
|---|---|---|---|
| Process crash / SIGKILL | no loss | no loss | no loss |
| OS reboot, no power loss | no loss | no loss | no loss |
| Power loss after `close(2)` | up to 100 ms of chunk data (chunk GC always on; composition is durable per-write so the comp→chunks mapping points at chunks that may be lost) | up to 100 ms of chunk data + composition metadata; comp+chunks may not align — orphan checks at startup | no loss (Raft re-replicates both chunks and compositions from peers) |
| Power loss after `fsync(2)` | no loss (`fsync_pending` forces device.sync) | no loss (`fsync_pending` forces both device.sync + redb commit) | no loss |
| Single disk failure | no loss (RAID / EC handles) | no loss | no loss |
| Single node failure (multi-node) | no loss (Raft majority) | n/a | no loss |
| Quorum loss (≥ 2 of 3 down) | accepted writes durable, new writes blocked | n/a | same |
| Whole-cluster power loss | per-node loss window depends on most-recent chunk fsync; operator runs scrub at startup | per-node loss window of chunk + composition state | same as default for chunk; composition recovers from Raft log on each node — cluster recovery reconciles via leader log |

Key invariant: **`fsync(2)` is always durable**, regardless of which
flags are on. The group-commit flags only affect the implicit
durability of writes that the application didn't explicitly flush.

---

## Recommended configurations by deployment

### Single-node dev / laptop

```bash
# Defaults. Chunk group commit is always on (100 ms loss window on
# unclean power loss); composition is durable per-write. Apps that
# need durability call fsync(2) explicitly.
unset KISEKI_COMPOSITION_FLUSH_INTERVAL_MS
unset KISEKI_OBSERVABILITY
```

### Multi-node production (3+ nodes, Replication-3 or EC)

```bash
# All three group-commit flags ON. Raft re-replication + chunk
# under-replication scrub recover the ≤ 100 ms loss window from
# peers on restart.
export KISEKI_RAFT_FLUSH_INTERVAL_MS=100
export KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100
export KISEKI_CHUNK_FLUSH_INTERVAL_MS=100
```

### Perf cluster (GCP `compact` / `transport` profile, throughput baseline)

```bash
# All three group-commit flags ON + observability OFF for clean baseline.
# Without KISEKI_RAFT_FLUSH_INTERVAL_MS the PUT path saturates at
# ~31 k op/s on Raft fsync; with it set the in-process-persistent
# matrix lifts to ~125 k op/s.
export KISEKI_RAFT_FLUSH_INTERVAL_MS=100
export KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100
export KISEKI_CHUNK_FLUSH_INTERVAL_MS=100
export KISEKI_OBSERVABILITY=off

# Until pNFS DS supports WRITE + persistent sessions, kernel pNFS
# clients pay per-file EXCHANGE_ID/CREATE_SESSION/RECLAIM_COMPLETE
# overhead per OPEN — capping NFSv4.1 seq-read at ~0.5 MB/s in the
# multi-node compose harness. Disabling layouts makes NFSv4 fall
# back to the MDS metadata-stream READ/WRITE path (~182 MB/s on the
# same workload). Not a durability knob — see `performance.md`.
export KISEKI_DISABLE_PNFS_LAYOUT=true
```

### Single-node FUSE benchmark

```bash
# All three group-commit flags ON, observability OFF. Accepts the
# 100 ms loss window — single-node has no peers to recover from,
# so dev / smoke-test only.
export KISEKI_RAFT_FLUSH_INTERVAL_MS=100
export KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100
export KISEKI_CHUNK_FLUSH_INTERVAL_MS=100
export KISEKI_OBSERVABILITY=off
```

---

## Verifying durability under group commit

After enabling either group-commit flag, sanity-check the fsync hook
path:

```bash
# 1. Write a file via FUSE.
echo "hello world" > /mnt/kiseki/durability-test.txt

# 2. fsync(2) explicitly.
fsync_test /mnt/kiseki/durability-test.txt   # any tool that calls fsync

# 3. Hard kill (NOT a graceful shutdown — kill -9 the kiseki-server).
sudo kill -9 $(pgrep kiseki-server)

# 4. Restart and read back.
kiseki-server &
sleep 2
cat /mnt/kiseki/durability-test.txt   # MUST print "hello world"
```

If step 4 fails, the fsync hook isn't wired. File a bug with the
`/metrics` snapshot showing `kiseki_chunk_persistent_write_phase_duration`
and the runtime log line `fsync hook: composition redb registered`.

---

## Cross-reference

- Performance numbers and the May 2026 fix sweep: [`docs/performance/README.md`](../performance/README.md)
- ADR-029 §Group commit for the chunk-store rationale
- ADR-040 §D6 / §D8.1 for the composition redb / hydrator design
- `commit 681de37` — chunk-store group commit landing
- `commit 56ec297` (todo, after this work) — composition redb group commit landing
