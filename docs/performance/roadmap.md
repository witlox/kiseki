# Performance roadmap — closing the gap on the 6-node GCP `default` cluster

Companion to [`targets.md`](targets.md) (what "good" is, derived from
hardware) and [`README.md`](README.md) (latest measured numbers). This
doc is the **gap + plan** layer: where each protocol stands on the
6-node `default` profile, why, and the prioritised work to close it.

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

**One problem dominates everything: writes are commit-bound.** A single
PUT (S3 / native / NFS COMMIT) costs ~p50 180 ms and the cluster tops out
at **~250 op/s · ~15 MB/s** aggregate, versus a derived target of
**5.2 GB/s (S3) / 10 GB/s (pNFS) / 360 k op/s (native)**. Reads already
scale (**23 k op/s · 1.4 GiB/s**, 0 err).

**One root cause dominates the writes: every write is its own Raft
consensus round, awaited synchronously — no batching, no pipelining.**
At ~30–45 ms consensus RTT per entry, throughput is bounded by
`(shards × concurrency) ÷ RTT`, not by NIC/disk/CPU. Until writes batch
into shared consensus rounds, no amount of NIC/disk headroom helps.

The single highest-leverage change is **group/batched Raft commit**
(coalesce concurrent writes on a shard-leader into one consensus entry).
Everything else (distributed leaders, hydrator throughput, per-chunk
fsync, read concurrency) is real but secondary.

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

### W1 — Batched / pipelined Raft commit (the lever) · **#126**
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
- **Status:** root cause for #126 — see that issue for the fix-design
  discussion.

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

## What "getting there" looks like, in order

1. **W1 batched Raft commit** — the one change that moves writes from
   ~250 op/s into the ~10 k op/s order. Everything else is noise until
   this lands. (ADR-grade; design on #126.)
2. **W3 hydrator throughput + W4 chunk group-commit** — sustain W1 under
   load, fix p99.
3. **Re-measure the 6-node matrix** — with W1+W3+W4 the write rows
   become meaningful; reads get an honest high-∥ number.
4. **R1/R2/R3 read tuning** — close the read bandwidth gap (concurrency,
   single-stream decrypt path, in-process backlog B1/B2).
5. **Fabric / transport** (RoCEv2, libfabric/cxi per `targets.md`) — only
   relevant *after* the software write path stops being the bottleneck;
   on `default` the NIC is the cap anyway, so this is a `compact` /
   `transport` / on-prem story, not a `default` one.
