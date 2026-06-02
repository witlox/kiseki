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

**2026-06-01 — three GCP passes settled the "where's the time"
question** (this supersedes both the "stacked off-CPU wait of unknown
mechanism" and the "client-side route-to-leader saves 41 ms" framings
that lived in earlier revisions of this doc — both were wrong):

> **The per-write floor under aggregate load is ~8 ms.** That's
> ~3-4 ms fjall WAL fsync + ~3-4 ms quorum-replication RTT.
> Under-loaded clusters look slower (12 ms mean at 1 client / 16 conc)
> because each openraft AppendEntries round amortises over too few
> entries. Optimally-loaded clusters approach the 8 ms floor — proven
> by mean dropping 12.3 → 8.1 ms when load went from 1 client to
> 3 clients × distinct leaders. openraft's `max_payload_entries: 300`
> batching is doing its job.

**There is no "stacked off-CPU wait" to hunt.** The off-CPU profile
under aggregate load is dominated by `kiseki-raft` threads
processing inbound AppendEntries — which is what we want them
doing — and the 47× DROP in sched-switches between 1-client and
3-client runs proves threads stay on CPU under more load. No
contention, no idle puzzle, no hidden serialisation. See
[`specs/performance/2026-06-01-gcp-partial.md`](../../specs/performance/2026-06-01-gcp-partial.md)
for the actual numbers.

**There is no 9-ms / 41-ms client-side-route-to-leader headroom.**
The "Pass 1 4 ms direct vs 42 ms forwarded = 10× lift available"
framing was a comparison across binaries (the 42 ms came from
2026-05-28's pre-#149 binary, the 4 ms from current main with the
WARN→DEBUG demote). When measured against the same binary under
the same load, the forward hop adds ~30% — fully amortised by
openraft batching at aggregate concurrency. **Client-side
route-to-leader would help native + FUSE only, by a small amount,
while leaving NFS / pNFS / S3 unchanged.** Don't ship that as
"the headline fix."

**W1 (batched Raft commit) was built and REJECTED.** openraft already
auto-batches concurrent `client_write`s (`max_payload_entries: 300`),
proven by 13× throughput scaling conc-1→16 *without* W1 — so
coalescing adds nothing against a 1 ms round. Reverted; ADR-046 kept
as a rejected record.

**The actual multi-protocol lever is log group-commit / fsync
coalescing.** Pass 1's off-CPU profile under sustained load: 12.2 %
of all sched-switch events were `kiseki-committer` in D state
(uninterruptible kernel wait on local NVMe fsync completion). Each
shard has its own `fjall::Database` today → each shard's per-AE-round
fsync hits the device queue independently. Coalesce N fsync requests
within a small window (500 µs / 32 entries) into **one** physical
`Database::persist(SyncAll)` call. **Benefits every protocol equally**
because all of NFS / S3 / pNFS / native / FUSE land on the same
`emit_chunk_and_delta` → openraft `client_write` →
`FjallRaftLogStore::append` path.

Expected lift: 8 ms server floor → ~3 ms (replication RTT alone with
fsync ~1 ms inside the same window). 5–8 ms p50 → ~2 ms p50. p99 75 ms
→ ~10-15 ms. **All protocols benefit by the same factor.**

**Path to target:** log group-commit (fixes the fsync floor for all
protocols) + concurrency × shards (already pipelined via openraft's
batching). Per-write floor with group-commit ≈ ~3 ms → 6 shards × ~30
conc × 6 nodes / 3 ms ≈ 360 k aggregate, matching the ADR-042 §14
native target without consensus changes. Tracked at the "Write fix
plan" section below.

---

### Historical TL;DR (superseded 2026-06-01, kept for context)

The pre-2026-06-01 framing said:

> "The cause is not the consensus round … the delta is a stacked
> off-CPU wait … mechanism not yet proven … client-side
> route-to-leader removes the ~9 ms forward hop the GCP run paid on
> 5/6 of writes … per-write floor ≈ 1.5 ms → 18 shards × 30 conc /
> 1.5 ms ≈ 360 k. First prove the stacked-wait mechanism (off-CPU
> profile) before building the route-to-leader change."

The "stacked wait" theory was the right next step at the time (we
hadn't profiled). When we did profile (Pass 1 + Pass 3 of the
2026-06-01 GCP run), the mechanism turned out to be fjall WAL fsync
in the committer thread — a known I/O wait, not an unidentified
scheduler bug. The 41 ms was an artefact of comparing two binaries
with different log-line costs on the hot path; the same-binary
forward hop is ~30 % and gets amortised by openraft's batching at
aggregate concurrency.

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

### W5 — Forward-to-leader logged at WARN · **#123** ✅ LANDED 2026-05-31
PR #149 — `log_append_err` demotes routine `ForwardToLeader` /
`LeaderUnavailable` to DEBUG. 22 k WARN lines/run → 0. Unblocks
profiling signal but only ~0.05 % per-write CPU back (not a perf lever
on its own — the headline win is signal clarity).

### W6 — Log fsync coalescing · #151 / PR #152 LANDED, **measured + REJECTED at production parameters** (Pass 4, 2026-06-01)
**Implementation correct, hypothesis wrong, code stays in gated off.**

#### Predicted lift (2026-06-01 framing) vs measured (Pass 4, same day)

| metric | predicted | measured | verdict |
|---|---:|---:|---|
| per-write fsync (1 client) | 3 ms → 0.3 ms | 12.4 ms → 12.4 ms | ❌ no change on critical path |
| server-side floor (3 clients) | 8 ms → 3 ms | 8.1 ms → 8.2 ms | ❌ no change |
| p50 (1 client) | 4 ms → 2 ms | 3.95 ms → 3.89 ms | ❌ noise |
| p99 (1 client) | 75 ms → 10-15 ms | 108 ms → 93 ms | ⚠️ tail-only −14 % |
| sched-switches | −50 % | 1.7 M → 1.0 M (−41 %) | ✅ as predicted |

Snapshot: [`specs/performance/2026-06-01-gcp-w6-rejected.md`](../../specs/performance/2026-06-01-gcp-w6-rejected.md).

#### Why the hypothesis was wrong

The Pass 1 off-CPU profile **correctly identified** that 12.2 % of
sched-switch events were `kiseki-committer` in D state on local NVMe
fsync. W6 reduced those events as predicted (−41 % at 1-client load
on the same node).

But **the fsync wait sits in parallel with `chunks.write_chunk` and
the AppendEntries replication RTT inside the `tokio::try_join!` fan
in `mem_gateway::write_impl`**. Removing one parallel branch by 3 ms
does nothing when another parallel branch is the same length. Pass 4
confirmed the `chunk_write` / `raft_commit` / `composition_record`
put-phase histograms ALL show the same mean — they all stop at the
moment both fans complete.

The Pass 1 "off-CPU wait identified" was real but **off the critical
path**. We needed an on-CPU + per-fan timing breakdown to see this,
not just sched_switch.

#### Code disposition

PR #152 stays merged. The coalescer is correct (7 unit tests pass,
including a timing assertion proving callers actually park inside
the window). Two reasons:

1. **Useful for the rare workload where Raft-log fsync IS critical
   path** — high-write single-shard, small object, no EC fan, no
   replication. Niche, but real.
2. **Removing it is more risk than leaving it gated off.** The
   `fsync_barrier()` tri-state preserves the existing inline-SyncAll
   shape when env vars are unset (the default).

#### Where it lives

- `crates/kiseki-raft/src/fsync_coalescer.rs` — the coordinator
- `crates/kiseki-raft/src/fjall_log_store.rs` —
  `with_fsync_coalescing(window_us, max_batch)` opt-in builder
- `crates/kiseki-raft/src/fjall_raft_log_store.rs` —
  `open_with_fsync_coalescing(...)` openraft wrapper
- `crates/kiseki-log/src/raft/openraft_store.rs` — env-var gate
  (`KISEKI_RAFT_FSYNC_WINDOW_US`, `KISEKI_RAFT_FSYNC_BATCH`); logs
  `info!` when enabled

#### What the real lever is (next diagnostic, ~$3)

> **Pass 5 — hot_path span decomposition.** Same binary, same env,
> single client, snapshot the `kiseki_gateway_hot_path_*` histograms
> before/after. Separates `chunk_write` (chunk-store fan) from
> `raft_commit` (intent fan). Whichever has the higher mean is the
> real target.

Three plausible outcomes:
- **chunk_write dominates** → fix is in `kiseki-chunk` (per-chunk
  fsync, EC fan latency); skip EC for inline-tier sizes; analogue of
  W6 on the chunk store
- **intent fan dominates** → openraft replication RTT is the floor;
  network tuning, h2 flow-control, or per-shard transport batching
- **Balanced (within ~1 ms)** → both need work; W6 deserves re-test
  after one of them moves

### W7 — Forward-path decomposition · **profile pass, not landed**
Pass 3's data says the forward hop adds ~30 % under aggregate load
(amortised by openraft batching, not the 9-ms-per-write earlier
reading). Still worth profiling once W6 lands to see if there's a
config-tune left (channel-pool reuse, etc). One follow-up GCP
mini-pass (~30 min, ~$2). Probably not the headline lever; capture
the numbers to confirm.

**Write target trajectory (6-node):** today ~3 k op/s aggregate
(Pass 3 measured)
→ **W6 ≈ 8–12 k op/s aggregate** (fsync coalesced, floor drops to
~3 ms) → W3 sustains it under burst → the native 360 k gate needs
heavier sharding (6 shards × ~30 concurrency × 6 nodes × 1.5 ms
post-W6 floor ≈ 360 k), realistically post-W6 re-measure.

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
