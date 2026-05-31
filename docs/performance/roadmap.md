# Performance roadmap — closing the gap on the 6-node GCP `default` cluster

Companion to [`targets.md`](targets.md) (what "good" is, derived from
hardware), [`README.md`](README.md) (latest measured numbers), and
[`competitive-targets.md`](competitive-targets.md) (where those same
targets sit vs Lustre / Ceph / VAST on identical hardware). This doc
is the **gap + plan** layer: where each protocol stands on the 6-node
`default` profile, why, and the prioritised work to close it.

Baseline measurements are the 2026-05-28 `default` matrix (RUN 3) —
6 × `c3-standard-22-lssd` + 3 clients, EC-4+2,
[`specs/performance/2026-05-28-gcp-matrix.md`](../../specs/performance/2026-05-28-gcp-matrix.md).

> Honest framing (per the project's perf-reporting posture): the
> minimal target is the `targets.md` derived number — `min(NIC, storage,
> CPU, fabric) ÷ replication`. The "ideal" is that number at full
> multi-stream saturation. We are nowhere near either on **writes**;
> **reads** are within striking distance with concurrency.

---

## TL;DR

**Writes top out at ~250 op/s · ~15 MB/s aggregate** on the 6-node run
(vs derived targets 5.2 GB/s S3 / 10 GB/s pNFS / 360 k op/s native);
reads scale (23 k op/s · 1.4 GiB/s).

**2026-05-29 — the cause is NOT the consensus round** (this supersedes
the "commit-bound / batched-commit is the lever" framing kept below for
history). The isolated openraft `client_write` round is **~1 ms** (2-node
loopback, no server load — `multi_shard_transport::
measure_openraft_round_latency`, mean 973 µs). The single-node full write
path is **240 µs**. Yet the full multi-node path is **~15 ms (local) /
~42 ms (GCP)**. That delta is a **stacked off-CPU wait** in the commit
pipeline under full-server load — and its mechanism is **NOT yet proven**:
it is *not* the openraft round (1 ms), *not* the gateway work (240 µs),
and *not* cleanly co-location (GCP on dedicated HW was also lowball, and a
CPU flamegraph shows idle, not hot stacks). The right probe is **off-CPU /
async-aware** (tokio-console per-task poll/wait, or per-step timing spans
in the loaded server) — NOT a flamegraph.

**W1 (batched Raft commit) was built and REJECTED.** openraft already
auto-batches concurrent `client_write`s (`max_payload_entries: 300`),
proven by 13× throughput scaling conc-1→16 *without* W1 — so coalescing
adds nothing against a 1 ms round. Reverted; ADR-046 kept as a rejected
record.

**Path to target (no consensus changes):** one server per box (removes
the co-located-test contention that produced the local 15 ms) +
**client-side route-to-leader** (removes the ~9 ms forward hop the GCP run
paid on 5/6 of writes) + concurrency × shards. Per-write floor ≈ round
(1 ms) + gateway work (240 µs) ≈ **~1.5 ms** → 18 shards × ~30 conc /
1.5 ms ≈ 360 k. **First prove the stacked-wait mechanism** (off-CPU
profile) before building the route-to-leader change.

---

## Where each protocol stands (6-node `default`)

| Protocol / op | Measured (RUN 3) | Minimal target | Gap | Bound by |
|---|---:|---:|---:|---|
| **native PUT** (64 KB, ×3∥) | 243 op/s · 15 MB/s | 360 k op/s | ~1500× | commit-bound (Raft RTT/write) |
| **native GET** (64 KB, ×3∥) | 23 k op/s · 1.4 GiB/s | 16 GB/s bulk¹ | ~11× (IOPS, not bw) | read concurrency / small-obj IOPS |
| **native mixed** | 307 op/s | — | — | commit-bound (write leg) |
| **S3 PUT** (50 MB, 1∥) | ~190 MB/s² | 5.2 GB/s aggregate | ~27× | commit-bound + single-stream |
| **S3 GET** (50 MB, 1∥) | ~180 MB/s | 16 GB/s aggregate | ~90× | single-stream TCP + S3 HTTP path |
| **NFSv3 write** | 5.9 MB/s | — | — | commit-bound (per-COMMIT) |
| **NFSv4.2 write** | 4.5 MB/s | 10 GB/s (pNFS) | ~2000× | commit-bound (per-COMMIT) |
| **pNFS write** | 4.6 MB/s | 10 GB/s | ~2000× | commit-bound |
| **NFS read** (O_DIRECT 32 MB) | 113–206 MiB/s | NIC-bound | ~12–20× | single-stream + per-op gateway read |
| **FUSE read** (16 MB) | 94 MB/s | NIC-bound | ~25× | single-stream + organic-cache flush timing |

¹ The 16 GB/s GET target is **bulk** bandwidth; the 64 KB get-heavy is
IOPS-bound (23 k op/s × 64 KB = 1.4 GiB/s). For an apples-to-apples
read, a large-object GET is the right comparison — currently only the
~180 MB/s single-stream S3 GET exists, which is itself low (see reads
below).
² S3 single-stream PUT looks fast at 190 MB/s only because a single
50 MB object is one composition / a few chunks — it does not exercise
the per-object commit tax that crushes the small-object aggregate.

---

## The write critical path (diagnosis)

Traced through `mem_gateway::write` → `log_bridge` →
`append_forwarder` → `openraft_store`:

1. **Encrypt + chunk write, per-chunk fsync** on the writer node
   (`mem_gateway.rs:2346`, `chunks.write_chunk`) — ~5–10 ms/chunk, and
   **serialised per chunk** when chunks share a device.
2. **Composition create** — in-memory, no fsync (good; eventual
   durability) (`mem_gateway.rs:2378-2462`).
3. **Raft consensus round** — `self.raft.client_write(cmd).await`
   (`openraft_store.rs:~539`), awaited synchronously: AppendEntries to
   the shard's followers + quorum log-fsync, ~30–45 ms RTT. **One Raft
   entry per write. No batching, no pipelining, no coalescing.**
4. If the write landed on a non-leader, add a forward RTT to the leader
   (`append_forwarder.rs:39-66`) before step 3.

The p50 180 ms (vs a ~36–57 ms single-write path) is the **16 concurrent
writers queueing on the per-shard single-entry consensus pipeline** plus
hydrator-lag tail (below). The path is **~360× too slow** for the native
target.

---

## Write fix plan (prioritised)

### W1 — Batched / pipelined Raft commit · **#126** — ❌ REJECTED (2026-05-29)
**Built (ADR-046, R+R+1) and reverted.** openraft already auto-batches
concurrent `client_write`s (`max_payload_entries: 300`) — measured 13×
throughput scaling conc-1→16 *without* W1 — so coalescing into one entry
amortises a round that's already amortised, against a round that's only
~1 ms anyway. No measurable lift (flat local + GCP). The premise below
(35 ms RTT/write) was the wrong number; the round is ~1 ms. Kept here as
the rejected analysis:

Coalesce concurrent writes arriving at a shard-leader into a **single
Raft entry per consensus round** (a bounded micro-batch: flush on N
entries or T µs, whichever first). One ~35 ms RTT then amortises across
the whole batch instead of one write.
- **Expected lift:** with a 64-deep batch, per-shard write throughput
  goes from `1/RTT` (~25 op/s) to `batch/RTT` (~1.5 k op/s); × 6 shards
  ≈ **~10 k op/s aggregate** — a ~40× lift, into the same order as reads.
- **Where:** the `append_chunk_and_delta` submit in `openraft_store` /
  `raft_shard_store`; add a per-shard batching queue ahead of
  `client_write`. openraft already pipelines AppendEntries at the
  transport layer — the gap is the gateway awaiting each proposal
  serially.
- **Effort:** ADR-grade (changes the write→commit contract; interacts
  with idempotency_key and the I-CP invariants). **Risk:** Medium-High.
- **Status: profile-first.** The 2026-05-28 code audit (below) found the
  consensus-layer batching/fsync are *already* in place, so coalescing
  is the *likely* fix but must be confirmed by a pprof decomposition of
  the 180 ms before it's worth the ADR. Not landed this pass —
  deliberately, to avoid a speculative durability-path change. #126.

### W2 — Distributed shard leaders · #99 / #111 / #114 (landed)
Spreading leaders across all 6 nodes (`namespace-create --shards 6`)
already parallelises writes across shards — verified live (6 leaders,
one per node). This is the multiplier W1 batches *within*. Keep it; it's
why aggregate isn't worse. **No further work**, but note W4.

### W3 — Hydrator throughput (F-1) · **#133**
The composition hydrator applies ~50 deltas/s under burst vs a ~10 k/s
theoretical ceiling (200× under) — its RED SLA test
`hydrator_drains_5k_delta_burst_within_5s` pins this. Under sustained
writes the backlog saturates and follower reads stale. Independent of
W1 but compounds it (batched commits produce deltas faster). Fix the
per-delta apply cost (lock contention / per-delta decode) in
`hydrator.rs`.
- **Expected lift:** removes the sustained-write ceiling + the p99 tail;
  no steady-state op/s change on its own.

### W4 — Per-chunk fsync → group commit · folded into #126
Chunk writes fsync per chunk and serialise on a shared device
(`mem_gateway.rs:2346`). Batch chunk fsyncs (one fsync per device per
flush window) the way composition group-commit already does
(`fsync_pending` hooks, off by default). Compounds with W1.
- **Expected lift:** ~5–10 ms/chunk → amortised; matters most for
  multi-chunk (>64 MiB) objects.

### W5 — Forward-to-leader logged at WARN · **#123** (cheap)
The per-write forward logs at WARN (22 k lines/run). WARN→DEBUG. Pure
hygiene but it's on the hot path and pollutes signal.

**Write target trajectory (6-node):** today ~250 op/s → **W1 ≈ 10 k
op/s** → W3+W4 sustain it under load and cut p99 → the native 360 k
gate needs the in-process backlog (B1 DashMap etc.) *on top of* batched
commit, and is realistically a post-W1 re-measure decision, not a
near-term number.

---

## The read path (within reach)

Reads already scale (23 k op/s · 1.4 GiB/s, 0 err) but sit below the
bandwidth targets. They are **not** commit-bound; the gaps are
concurrency + per-protocol overhead, not a structural wall.

### R1 — Read concurrency / multi-stream
get-heavy at ×3 clients × 16 = 1.4 GiB/s is ~18 % of one node's NIC per
client. The targets assume ≥ 8-way per client at 90 % NIC. More client
concurrency + connection-pool depth scales this; it's a measurement +
tuning lever, not a code fix. **Re-measure at higher ∥ before
concluding a read gap.**

### R2 — Bulk single-stream reads (S3 GET 180 MB/s, NFS 113–206 MiB/s)
Single-stream bulk reads are ~10× below the ~1.8 GB/s single-stream NIC
target. Suspects: per-op gateway read (decrypt path), S3 HTTP framing,
NFS per-op round-trips. The [2026-05-09 Arc-wrap fix]
already lifted single-stream GET 8.2 → 16.4 Gbps in-process; the
multi-node single-stream needs the same scrutiny (is the 64 MiB chunk
decrypt cloned/serialised on the gateway read path?). **Profile a
single-stream bulk GET on the cluster** before optimising.

### R3 — Small-object read IOPS (native 23 k → 360 k gate)
The native read gate (ADR-042 §14) is 60 k op/s/node TCP-framed; 23 k
aggregate is below even one node's gate. Same in-process backlog (B1
DashMap on `CompositionStore`, B2 per-thread counters) applies — these
are documented in [`optimization-backlog.md`](optimization-backlog.md)
and were measured at 82 k op/s in-process; the multi-node delta is
network + the read-resolution path.

---

## Bugs & findings collected this pass

| Ref | What | Status |
|---|---|---|
| **#126** | Commit-bound writes — **root cause added** (no batched Raft commit) + W1/W4 fix plan | updated this pass |
| **#133** | Hydrator applies ~50 deltas/s vs ~10 k ceiling; RED SLA test; saturates sustained writes (was F-1) | filed this pass |
| **#123** | forward-to-leader at WARN on the hot path | open (W5) |
| **#132** | forwarded writes (#114) skip fabric PutFragment quorum (durable via Raft) — interacts with W2 | open |
| **#64** | FUSE write+fsync+drop_caches → size 0 (read-before-async-flush; matches the RUN-3 organic-cache timing) | open |

Per-protocol functional state (post-#116): #127 (pNFS/NFS read-by-name)
and #130 (NFSv3 write) are **fixed + verified live**; #128 (NFSv3 mount)
fixed. So the perf path is now unblocked on correctness — the remaining
work is throughput, dominated by W1.

---

## Code-audit findings (2026-05-28) — the roadmap above was written from
## numbers; the code says most "fixes" are already in or need profiling

A deep read of the write path (gateway → log → raft → state machine)
before attempting to land the fixes turned up that **the optimisations
the gap analysis assumed were missing are largely already implemented**:

- **Raft log fsync is already batched per AppendEntries**, not per entry
  (`kiseki-raft/src/fjall_raft_log_store.rs:127` — "fjall's `commit` is
  all-or-nothing on the batch… [vs] `inner.append` per entry, burning N
  fsyncs per replication round"). The W1-as-"no batching" framing was
  wrong at the consensus-durability layer.
- **openraft batching is on** — `max_payload_entries: 300`
  (`kiseki-raft/src/config.rs:22`); it coalesces up to 300 entries into
  one AppendEntries round and pipelines replication.
- **The hydrator already batches at commit** — one `HydrationBatch` per
  poll (up to 1000 deltas staged, one atomic storage commit), not
  per-delta (`hydrator.rs:304-422`). #133's ~50 deltas/s is a *per-delta
  staging* cost, not a missing-batch — needs profiling to localise.
- **B1 (CompositionStore HashMap → DashMap) is effectively already done**
  — the store has sharded `name_locks` + `id_locks`
  (`composition.rs:573-581`), an `RwLock` namespace map, and an LRU
  read-cache. The remaining serialisation is the **I-CP1**-mandated
  single redb transaction (`last_applied_seq` + state changes atomic),
  which can't be sharded away without breaking crash-consistency.
- **B4 (pre-alloc) is a no-op** — `landed.iter().map().collect()`
  (`mem_gateway.rs:2366`) already pre-sizes via `size_hint`.
- No gateway-side lock is held across `client_write`
  (`openraft_store.rs:539`); concurrent writes are *not* serialised by us.

**So the ~180 ms write latency is not explained by a missing
code-level batching/contention fix.** It is a genuine consensus-path
cost (replication RTT + per-shard serial apply + fsync + possible
hydrator feedback) whose decomposition **requires cluster profiling
(pprof on the GCP `default` cluster), not a blind code change.** The
honest conclusion of "try to land the fixes": the landable code-level
fixes are already in; W1/W3 are **profiling-blocked**, and shipping a
speculative change to the Raft commit/durability path would violate the
correctness-over-velocity posture for no measured gain.

**Revised W1** is therefore *profile-first*: run a CPU + off-CPU
(blocking) profile of a sustained write under the matrix, decompose the
180 ms into {replication RTT, log fsync, apply, hydrator wait, lock},
and *then* pick the targeted fix (application-level write coalescing
into one `LogCommand` batch — ADR-grade — vs an apply-path change vs a
config tune). #126 carries this.

## What "getting there" looks like, in order

1. **W1 — write coalescing → [ADR-046](../../specs/architecture/adr/046-write-coalescing-batched-commit.md).**
   The 2026-05-28 work pin pointed the bottleneck: openraft log-fsync +
   replication are already batched/pipelined; the gap is the **serial
   per-shard apply with no coalescing of independent writes into a
   shared consensus entry**. ADR-046 specifies the fix (a
   `LogCommand::BatchChunkAndDelta` + per-shard coalescing queue + per-
   item result fan-out, atomicity/idempotency/I-CP preserved). ~40× per
   cluster (≈250 → ~10 k op/s); 360 k needs coalescing × heavy sharding
   (ADR-041). Needs adversary gate-1 before impl. #126. The `raft_commit`
   put-phase histogram (landed) measures the lift.
2. **#2 large-object write parallelism + #3 hydrator (#133) — confirmed
   DOWNSTREAM of W1, deferred.** The chunk loop only bites >64 MB objects
   and is masked by the per-PUT commit; the hydrator drains 5 k deltas in
   0.03 s in-memory (~166 k/s) and the GCP "~50/s" is just it keeping
   pace with the commit-bound write rate. Both become measurable/worth
   doing only *after* W1 raises the write rate. Re-measure then.
3. **Re-measure the 6-node matrix** — with W1+W3+W4 the write rows
   become meaningful; reads get an honest high-∥ number.
4. **R1/R2/R3 read tuning** — close the read bandwidth gap (concurrency,
   single-stream decrypt path, in-process backlog B1/B2).
5. **Fabric / transport** (RoCEv2, libfabric/cxi per `targets.md`) — only
   relevant *after* the software write path stops being the bottleneck;
   on `default` the NIC is the cap anyway, so this is a `compact` /
   `transport` / on-prem story, not a `default` one.
