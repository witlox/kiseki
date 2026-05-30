# 2026-05-30 — ADR-047 decoupled-ack perf: where does 10× live?

We built decoupled-ack (`8ef8b4e`…`453160c` on main) on the assumption it would
break through the commit-bound write throughput. It moved native PUT from 261
to 712 op/s on a 6-node GCP cluster — **+13 %, not 10×**. This deliberation
asks the diamond-workflow roles (analyst, then architect, then adversary if
warranted) to look at the actual data and propose the structural design
options that could give 10×.

## PART 0 — What we built

Async-ack surfaces (S3, Native — per `WriteSurface::is_async_ack_eligible`):

1. Ingress mints `perspective_seq` (HLC).
2. Chunks fanned to `min_acks` peers (via `ClusteredChunkStore::write_chunk`).
3. Composition delta created LOCALLY at the ingress (`comps.create`).
4. `WriteIntent` quorum-written via `put_intent_and_fan` (durable on local
   IntentStore + fanned to `min_acks − 1` peers, including the current leader).
5. FAST-ACK to client.

Off the ack path: per-shard leader committer drains its IntentStore into the
Raft log (`IncorporateIntent`) which replicates to followers, whose hydrators
rebind the `(ns,name) → comp_id` index.

Synchronous-ack surfaces (POSIX/NFS/FUSE — per ADR-013): unchanged — full
synchronous Raft commit before ack. Out of scope here.

POSIX-sync stays untouched. The question is whether S3/native can go 10×.

## PART 1 — The data (this is what the design must answer to)

### Flame graph (multi-node local, put-heavy, 16 concurrency)

Captured via `kiseki-profile run --nodes 3 --pprof ...` against current main.
Category coverage on the leader (highest % matching frame per category):

| % | Category | Top frame |
|---:|---|---|
| **19** | crypto / HMAC-SHA256 | `kiseki_crypto::chunk_id::derive_chunk_id` + `aws_lc_rs::hmac::sign` |
| 6 | fjall / lsm_tree | `crossbeam_skiplist::Range`, `lsm_tree::InternalKey::cmp` |
| 6 | Raft path | `kiseki-raft` symbols (replicate, RPC) |
| 6 | tokio runtime | worker scheduler |
| 5.5 | IntentStore write | `InstrumentedLogOps::put_intent_and_fan` |
| 5.2 | Composition | `CompositionStore::create_with_name` |
| 4.5 | Gateway routing | `TcpFramedListener::run` |
| **0.69** | Fabric / chunk fan | `ClusteredChunkStore::write_chunk` |
| 0.32 | Network framing | `postcard::ser::serialize_with_flavor` |

**Chunk fan-out is NOT on the hot path** (the dedup-hit observation below
explains why). AES-GCM encrypt doesn't register meaningfully.

### Wait / timing (Prometheus histograms from `/metrics`, mid-run snapshot)

Per-op critical path on localhost (3 nodes, put-heavy, 16 concurrency):

| Phase | mean per call | count | per-write? |
|---|---:|---:|---|
| `kiseki_raft_transport_rpc{op=append_entries}` | **113 ms** | 674 (per shard) | NO — batched ~340 writes per RPC. openraft already batches. |
| `gateway_put_phase{phase=composition_record}` | 858 µs | 227 764 | YES — WRAPS `comps.create` + payload build + `put_intent_and_fan` |
| `gateway_put_phase{phase=raft_commit}` | 682 µs | 227 764 | YES — currently labels `put_intent_and_fan` (post-rip name carry-over) |
| `composition_hydrator_apply` | ~3 ms | 193 / shard | follower — apply replicated delta |
| `gateway_put_phase{phase=encrypt}` | 174 µs | **5** | dedup-hit — see note |
| `gateway_put_phase{phase=chunk_write}` | 1 185 µs | **5** | dedup-hit |
| `fabric_op{op=put}` | 1 027 µs | 10 | dedup-hit |

Per-write critical path budget (localhost, dedup-hit steady state):

```
derive_chunk_id (HMAC-SHA256, 64 KiB)    ≈ 100 µs (CPU)
comps.create + encode + build append     ≈ 176 µs (CPU + local fjall)
put_intent_and_fan                       ≈ 682 µs (local store + cross-node fan)
                                         -------
                                         ≈ 1.0 ms per write
```

Bench measured p50 = 1.2 ms; matches.

### The dedup-hit observation

`chunks.write_chunk` ran **5 times for 227 764 writes** — 0.002 %. The bench
client appears to write degenerate / highly repeated content; in steady state
the chunk path is short-circuited by dedup. **Real-workload numbers with
unique-chunk writes would put `chunks.write_chunk` (1.2 ms localhost) back on
the critical path** — adding ~1 ms / write to the budget.

### GCP vs localhost

- Localhost 3-node, put-heavy 16-conc: 14 110 op/s, p50 = 1.2 ms.
- GCP 6-node, put-heavy 16-conc (2026-05-30): 712 op/s, p50 ≈ 22 ms.

Localhost CPU + local I/O = ~1.5 ms per write. GCP adds ~20 ms over
that = network RTTs in the critical path. With **2 sequential cross-node
RTTs per write** (chunk fan, intent fan), at typical GCP LAN ~1 ms RTT, the
intent fan alone is several ms. The `put_intent_and_fan` cost on GCP is
not 682 µs; it's ~5 ms.

### What `chunks.write_chunk` actually does on a unique-content write
(important for non-bench workloads)

`ClusteredChunkStore::write_chunk`: fans `PutFragment` to all replicas in
parallel and waits for `min_acks` (default 2-of-N). This is *one* cross-node
RTT per write (parallel fan, not sequential per peer). So a non-dedup write
adds one RTT for the chunk fan to the critical path.

## PART 2 — Constraints the design MUST satisfy

These are not negotiable without explicit ADR amendments:

| # | Constraint | Source |
|---|---|---|
| C1 | **No-loss for async surfaces.** An acked write is durable on `≥ min_acks` replicas at ack time. Crash + recovery never loses an acked write. | I-L2, I-CS1, ADR-047 O1/O2 |
| C2 | **Chunks-before-metadata.** Chunks min_acks-durable before the metadata intent that references them. | I-L5, ADR-047 F-P4-1 |
| C3 | **Per-key LWW correctness.** Concurrent writes to the same `(ns, name)` converge to the HLC-newest. | I-L1 (some total order), ADR-047 R2 + the per-name seq-guard |
| C4 | **Per-shard total order.** One recoverable order, no split-brain. | I-L1 |
| C5 | **Idempotency.** Re-incorporation never double-applies. | F-2 (recent-incorporated set + ancient cutoff, PART 8 reframe) |
| C6 | **POSIX surfaces stay synchronous.** Close-to-open semantics unchanged. | ADR-013 |
| C7 | **Bounded staleness for async cross-node reads.** Visibility lag is bounded; no permanent invisibility. | I-CS2, ADR-047 R4 |

Tradeable (the user has explicitly said these are negotiable for perf):

- `min_acks = 2` for the intent fan. Could be 1 (local-only durable) if a
  different mechanism re-establishes the durability promise.
- The bench-current shape of `WriteSurface::is_async_ack_eligible` — could
  expose new surface profiles with weaker guarantees if explicitly chosen
  by the client.
- The 100 ms `KISEKI_*_FLUSH_INTERVAL_MS` group-commit intervals.

Not tradeable: C1–C7 above. POSIX-sync (C6) is the contract we promised the
NFS/FUSE clients.

## PART 3 — ANALYST FRAMING

Mode: DESIGN. Project: Brownfield with baseline. Role: analyst. Reason:
framing the 10× design problem for the architect (no winner picked).

### 3.1 Problem statement, sharp

"10× perf" is two distinct problems and the architect must treat them as
such; collapsing them hides the cheapest lever.

- **P-A — Throughput (op/s, aggregate, single-host bench shape).** Native
  PUT at 16-concurrency on the 6-node GCP default profile, 64 KiB
  put-heavy, **rises from 712 op/s → ≥ 7 000 op/s** while keeping C1–C7.
  This is the headline number the user cares about.
- **P-B — Single-write tail latency (per-op, cross-node).** p50 latency
  on the same workload **drops from 22 ms → ≤ 2.2 ms** (and p99 in
  proportion). This matters for interactive surfaces (FUSE close, S3
  PUT) and is the bound P-A respects under low concurrency.

P-A and P-B are different problems: P-A is dominated by *what is
batched/coalesced on the critical path* (concurrency × 1/latency_per_op
hits a ceiling when every op contends for the same single-writer
resource); P-B is dominated by *what is sequential* on the critical
path. A change can move one without moving the other:

- Pure batching (more ops per Raft round, more intents per fan) lifts
  P-A but leaves P-B near the per-op floor.
- Pure pipelining (fewer sequential RTTs per op) lifts P-B and also
  lifts P-A only proportionally — it does not amortize a fixed cost.

The architect MUST name which of P-A and P-B each candidate is solving,
and how the other moves. The user's stated metric is op/s (P-A); P-B is
the safety check that the answer to P-A is not "queue everything and
charge 30 s per write."

Baseline (frozen, for the metric):
- Localhost 3-node, put-heavy 16-conc: 14 110 op/s, p50 = 1.2 ms.
- GCP 6-node, put-heavy 16-conc, 64 KiB: 712 op/s, p50 ≈ 22 ms.
- Workload caveat: **dedup-degenerate** (PART 1 — `chunks.write_chunk`
  fires 0.002 % of writes). A unique-content workload would add ~1
  cross-node RTT (one EC/replication fan) to every per-op budget. See
  §3.5(d).

### 3.2 Latency budget on GCP, decomposed (measured vs inferred)

The honest decomposition of the 22 ms p50 GCP write, attributing every
millisecond we can, and flagging the rest.

| ms | Component | How known | On critical path? |
|---:|---|---|---|
| ~0.1 | `derive_chunk_id` (HMAC-SHA256, 64 KiB) | Measured (localhost flame) — CPU; assumes GCP CPU ≈ localhost CPU | YES |
| ~0.18 | `comps.create` + delta encode (local fjall) | Measured (localhost flame); GCP local-disk write latency unknown | YES |
| ~0 (dedup) / ~1 ms (unique) | `ClusteredChunkStore::write_chunk` (parallel fan, 1 cross-node RTT, waits `min_acks=2`) | Measured on dedup-degenerate workload (0.002 % fire-rate); the **1 ms is inferred** from localhost `chunk_write` (1.2 ms) extrapolated to GCP — *the GCP cross-node RTT cost for chunks is not directly measured because the bench almost never fires that path* | YES (when unique content) |
| ~5 ms (inferred) | `put_intent_and_fan` (leader-first sequential fan + parallel top-up, **2 sequential cross-node hops worst case** — leader RTT, then parallel-RTT to remaining peers) | Inferred: `gateway_put_phase{phase=raft_commit}` mean rises from 682 µs localhost to a GCP value not directly captured in this snapshot; per PART 1 estimate ~5 ms | YES |
| ~16 ms | **Unattributed** | The arithmetic gap between 22 ms p50 and the ~6 ms accounted above. Candidates: (i) GCP per-RTT cost > our 1 ms assumption, (ii) intent fan is sequential not parallel under contention because the leader-first hop is mandatory (`MF-3`), so **2 sequential ~1 ms RTTs + queueing**, (iii) under 16-conc contention `put_intent_and_fan`'s leader-first hop serializes — every ingress targets the same per-shard leader, queueing in the connection pool / single Raft writer, (iv) async runtime scheduling tail under load. All four are **plausible, none measured**. | YES |

**Methodology gap (call it out loudly):** we do not have per-component
GCP wait-time decomposition. We have `gateway_put_phase` histograms
that prove `raft_commit` (the label currently on `put_intent_and_fan`,
per PART 1) dominates locally, but we are missing:

1. Per-phase GCP timings (PART 1 only includes localhost histograms).
2. Distinction between intent-fan parallel time vs leader-first
   serialization time.
3. Wait-time vs CPU-time split inside `put_intent_and_fan`.
4. Contention curves: does p50 stay flat or climb with concurrency?
5. Per-shard leader queue depth — is one shard's leader the funnel?

Before the architect commits to a design, an instrumentation pass on
the GCP cluster that breaks `put_intent_and_fan` into (local-put,
leader-fan-RTT, top-up-RTT, queue-wait) is the cheapest way to halve
the unattributed 16 ms.

### 3.3 What 10× would require, structurally

P-A target 7 000 op/s on 16 concurrent clients ⇒ per-op latency budget
= **16 / 7 000 = 2.3 ms** if every op is independent and the critical
path is the only serialization point.

Current GCP p50 = 22 ms ⇒ must remove **≥ 19.7 ms (90 %)** from the
critical path, OR break the per-op model so that 7 000 ops happen
inside a per-op latency window > 2.3 ms (i.e. **amortize**: many ops
share one Raft round, one intent fan, one chunk fan).

Two structurally distinct paths:

**Path A — Shrink the per-op critical path (attack p50, P-B → P-A).**
- Minimum to remove: ~19.7 ms of sequential work per op.
- The intent fan (~5 ms inferred) + the unattributed (~16 ms) are the
  whole removable budget. Even eliminating intent fan entirely
  (~5 ms) only gets us 22 → 17 ms ≈ 1.3×. **Therefore p50 → 2.2 ms
  on GCP is impossible without resolving the unattributed 16 ms
  first** — we cannot design Path A without first measuring it.
- If the unattributed 16 ms is leader-queue contention under
  16-concurrency, then sharding / removing the funnel (per-ingress
  intent stores; eliminate leader-first hop for the intent path) is
  Path A. If it is GCP RTT > 1 ms (e.g. 3–4 ms across zones), then
  Path A requires removing both intent-fan RTTs.

**Path B — Amortize so 7 000 ops fit a fatter per-op latency window
(attack throughput, P-A directly).**
- If each "intent fan" carries N intents and the fan still costs ~5 ms,
  then per-op intent cost ≈ 5 ms / N. To get 7 000 op/s on the current
  ~22 ms per-batch, **N must be ≥ 7000 × 0.022 / 16 ≈ 10**
  (intents-per-fan, per concurrent client). That is a modest coalescing
  ratio — well within reach of W1-style batching applied at the intent
  layer.
- This path keeps p50 latency near current (22 ms; can even rise
  modestly if the batch wait is bounded), but lifts op/s. The user
  signalled P-A is the metric — this is fair game.

**Which C-constraints does each path stress?**
- Path A stresses C7 (visibility lag — if we cut the intent fan we
  must replace it with *something* that keeps the recoverable order)
  and potentially C1 (if the cut is "ack on 1 local copy").
- Path B stresses C7 only indirectly (a batched intent is still
  durable before ack; staleness window grows by at most one batch
  interval) — and **does not stress C1 at all** if the batched fan
  still hits `min_acks` for every intent in the batch.

This is why Path B is the cheaper-to-justify direction: a Raft-style
batch that fans N intents in one RPC payload and waits for
`min_acks` peers to ack the *batch* keeps C1 intact, keeps C2
(chunks-before-intent) intact, and only widens C7's window by the
batch's accumulation latency.

**The hard math conclusion:** 10× P-A is reachable *with Path B alone*
even before we attack Path A. 10× P-B is NOT reachable on the current
budget without (a) the GCP measurement gap closing and (b) either C1
relaxation for some surface or an architectural change that removes
the intent fan from the critical path entirely.

### 3.4 The option space — 4 candidate structural directions

Each option lists: idea, constraints it stays inside vs bends, latency
removable, biggest risks. **No winner picked.**

#### Option 1 — Batched intent fan (W1 applied at the intent layer)

**Idea.** `put_intent_and_fan` currently sends one intent per RPC.
Add a coalescing buffer at the ingress with a short window (e.g.
~1 ms or N=64 intents, whichever first) that emits one `PutIntents`
RPC carrying many intents. Peers stamp `min_acks` on the *batch*; the
ingress acks all intents in the batch when quorum is met. Analogous
to W1 batched Raft commit (ADR-046) but at the intent layer, which
**openraft does NOT batch for us** because intents are not Raft
commands — they ride a leaderless quorum write alongside the log.

**Constraints.**
- C1: stays inside — every intent in the batch is `min_acks`-durable
  before its individual ack.
- C2: stays inside — chunks still fan per write (or could
  ride the same batch, see Option 2).
- C3 (per-key LWW): stays inside — perspective-seq is per-intent;
  batch ordering does not affect LWW resolution.
- C4 (per-shard total order): stays inside — committer still drains
  in seq order.
- C5: stays inside — idempotency key per intent.
- C6 (POSIX sync): N/A — POSIX surfaces stay sync-apply per ADR-013;
  batching is an async-surface lever only.
- C7 (bounded staleness): bends slightly — batch window adds at most
  the coalescing interval (~1 ms) to visibility lag. Within I-CS2's
  spirit, but the bound MUST be published.

**Latency removed from critical path.** The per-op intent-fan cost
drops from ~5 ms → ~5 ms/N. For N=10 this is ~0.5 ms/op. Direct hit on
P-A: at N=10 the per-op intent-fan contention falls 10×; the total
critical path moves from 22 ms → ~17.5 ms per op, but each "op" inside
the batch shares this cost, so effective throughput climbs ≥10×.

**Biggest risks.**
1. Coalescing window adds tail latency at low concurrency (one
   straggler waits for the timer). Mitigation: max-wait bounded by a
   small constant; flush-on-idle.
2. A failed batch fails *all* intents in it. Need batch-granularity
   error reporting + idempotent retry per intent.
3. Doesn't help P-B (per-op latency stays similar or worsens).

#### Option 2 — Pipelined critical path (chunk fan ∥ intent fan)

**Idea.** Today chunk fan completes, then composition delta is built,
then intent fan runs. Pipeline: speculatively start the intent fan in
parallel with chunk fan, **but the intent stays "armed not live" on
peers** until the ingress confirms `min_acks` chunk durability. A
final tiny "arm" RPC (or a piggybacked flag on the next intent fan,
or implicit on a watermark heartbeat) flips the intent live. C2
preserved because peers refuse to incorporate an unarmed intent.

**Constraints.**
- C1: stays inside — ack still requires both quorums.
- C2 (chunks-before-intent): **bent but preserved at the visibility
  layer.** Intent is durable in flight, but not incorporable until
  chunks confirmed. The ADR-047 ordering of "intent records chunk_ids
  that are already durable" requires an addendum: peers MAY hold an
  unarmed intent whose chunks are not yet known to be durable, but
  MUST NOT apply it. The chunk-durable check moves from
  ingress-pre-intent to peer-pre-apply.
- C3, C4, C5: unchanged.
- C6: N/A.
- C7: tighter window — pipelining shrinks visibility lag.

**Latency removed.** ~1 ms (one chunk-fan RTT) on unique-content
writes. On dedup-degenerate writes (the bench): zero, because chunk
fan is already short-circuit. Direct hit on P-B; modest hit on P-A.

**Biggest risks.**
1. Unarmed intents complicate recovery: on leader election, what
   becomes of an unarmed intent whose chunks never confirmed? Needs
   a TTL + GC story (or a chunk-presence probe).
2. Adds a new RPC verb (or a new tag on an existing one) and a new
   peer-side state. Surface area for bugs.
3. Doesn't help if the unattributed 16 ms is leader-queue contention
   (Option 1's territory) rather than RTT serialization.

#### Option 3 — Local-only durable ack + scheduled replication

**Idea.** Ack on `min_acks = 1` (local intent + local chunk only).
A separate primary-write WAL (per-ingress) streams intents to peers
in background, re-establishing `min_acks ≥ 2` within a bounded
window (e.g. 50–500 ms). Per-tenant or per-surface contract
publishes the loss-window: "if you ack on this surface, your write
survives this node, but a node-failure within W ms can lose up to W
ms of writes."

**Constraints.**
- C1 (no-loss-at-ack): **broken as currently stated**, replaced with
  a publicly contracted bounded-loss-window per surface. This is the
  big trade. Acceptable to user iff (a) loss window is explicit and
  bounded, (b) at least one surface keeps the strict promise.
- C2: stays inside — local chunk + local intent locally ordered.
- C3, C4, C5: unchanged (per-key LWW on seq).
- C6: N/A (POSIX surfaces stay strict).
- C7: visibility lag widens to the replication window (same as the
  loss window).

**Latency removed.** Both intent fan (~5 ms) and chunk fan (~1 ms
on unique content) leave the critical path. Per-op latency floor
drops to ~0.3 ms (just local CPU + local fjall). Direct hit on
both P-A and P-B — **the only candidate that gets to 2.2 ms p50.**

**Biggest risks.**
1. Reputational: "kiseki acks before durable on quorum." The
   bounded-loss-window contract MUST be load-bearing in
   documentation, not a footnote.
2. Defines a new surface or modifies existing async-surface contracts.
   The user's split on S3 vs Native vs POSIX (ADR-014, ADR-013)
   needs a third axis: "best-effort-async."
3. Re-replication backpressure: if writes outpace background
   replication, the loss-window stretches and the contract is
   silently violated. Needs admission control.

#### Option 4 — Bypass the per-write composition delta

**Idea.** On the ack path write only: (a) the chunk(s), (b) the
intent (a small record naming chunks). **Do NOT create the composition
delta per write.** A background coalescing flusher converts batches
of intents into composition-delta entries in the log (one log entry
per N writes, not per write). Readers still see correct state because
intents resolve in the read path (POSIX already does this per ADR-047
§6); S3 reads consult intents too until the flush incorporates them.

**Constraints.**
- C1: stays inside — intent is `min_acks`-durable.
- C2: stays inside — chunks still before intent.
- C3, C4, C5: unchanged.
- C6: N/A.
- C7: visibility-lag widens by the flush window. POSIX surfaces
  already have the pending-intent read path; S3 surfaces inherit it
  (or accept the staleness within I-CS2).

**Latency removed.** ~176 µs of `comps.create` on the critical path
(small but consistent) + the *committed-log entry count* drops by N×,
which lifts the global Raft throughput ceiling. Indirect hit on P-A
via reduced log pressure; small direct hit on P-B.

**Biggest risks.**
1. Read path complexity grows: every surface needs robust
   pending-intent resolution under apply lag.
2. Crash recovery must rebuild compositions from intent state —
   already true under ADR-047 §4, but the coverage of "what state
   is in the log vs the intent store" widens.
3. Smallest absolute latency win of the four; only structurally
   interesting if combined with Option 1.

#### Composition matrix (analyst view, no winner)

| Option | P-A win | P-B win | C1 cost | C7 cost | New mechanism count |
|---|---|---|---|---|---:|
| 1 — Batched intent fan | **High** (10×) | None | None | ~1 ms | 1 (coalescer) |
| 2 — Pipelined chunk ∥ intent | Modest | **Moderate** (~1 ms off) | None | Negative (tighter) | 2 (arm-RPC, GC) |
| 3 — Local ack + scheduled replication | **High** | **High** (~2.2 ms achievable) | **Bounded loss window** | Wider | 3 (surface, WAL, admission) |
| 4 — Bypass composition delta on ack | Modest (via log pressure) | Small | None | Wider | 2 (flusher, read-resolve coverage) |

### 3.5 Hard tensions the architect must resolve

#### (a) C1 vs the perf gap — where is the line?

The user's tradeable list in PART 2 includes `min_acks = 2 → 1`
"if a different mechanism re-establishes the durability promise."
**The analyst reads this as: C1 is tradeable for some async
surfaces, NOT all.** S3 and Native are async-eligible (ADR-047
§F-3); both are candidate surfaces. POSIX is off-limits (C6).

Open question for the architect:
- Is there appetite for a **third async surface tier** — "async
  bounded-loss" — distinct from "async bounded-stale"? If yes,
  Option 3 is on the table; if no, only Options 1/2/4 remain.
- If yes, what is the maximum loss window the user will publish?
  50 ms (page-cache window scale)? 500 ms? Until an explicit
  number is set, Option 3 is **under-specified, not rejected**.

#### (b) C2 under pipelining (Option 2 specifically)

Today the intent records chunk_ids that the ingress has *already
confirmed* `min_acks`-durable. If we fan the intent in parallel with
the chunks, the intent on a peer references chunks the peer cannot
prove are durable yet. **C2 is preserved only if peers refuse to
incorporate (apply, make visible) the intent until chunk durability
is independently confirmed.**

Open questions:
- What is the confirmation mechanism? An "arm" RPC, a separate
  watermark, a chunk-store-presence probe on apply?
- What is the timeout for unarmed intents? After how long does a
  peer drop an unarmed intent?
- Does the ingress have to keep state until the intent is armed
  on `min_acks` peers, or can it ack as soon as the chunks confirm
  locally?

#### (c) C7 under coalesced composition delta (Option 4)

How long can a write be "acked + locally visible but not in the
per-shard total order?" Today's worst case is the
`KISEKI_*_FLUSH_INTERVAL_MS` window (~100 ms) — coalesced composition
delta extends this to N × that, or to a separate flush interval.

Open questions:
- Does I-CS2 ("bounded staleness, acceptable per view descriptor")
  cover this, or does the bound need to be re-stated as an
  explicit numeric ceiling?
- If a reader on a different node consults the intent store
  (ADR-047 §6 already permits this for POSIX), does it get
  cross-node-current data, or is there a per-shard refresh lag too?

#### (d) Bench-degenerate-content (the elephant)

`chunks.write_chunk` ran 5 times in 227 764 writes. **The bench's
content is degenerate** — content-addressed dedup short-circuits the
chunk path. Two consequences:

1. Any design that optimizes "intent fan" without "chunk fan" looks
   100 % effective on bench and may look 50 % effective on real
   workloads (where chunk fan re-enters the budget). Option 1
   particularly: a batched intent fan that doesn't also batch the
   chunk fan leaves a per-op cross-node RTT in the critical path
   that the bench never exposed.
2. Real-workload p50 on GCP is probably > 22 ms (the
   missing chunk-fan ~1 ms RTT). The 7 000 op/s target
   should be validated against a unique-content workload before the
   architect commits to a candidate.

Open question for the architect:
- Should the design target be re-stated as "7 000 op/s under
  unique-content load," and the bench harness fixed to write
  unique content before any candidate ships?
- Whatever the answer, **the design must not rely on dedup for its
  perf model.**

### 3.6 Non-negotiables vs tradeable (analyst's read)

Re-stating PART 2 with the analyst's interpretation of the user's
signals. The architect should challenge any of these by name if they
disagree.

| # | Constraint | Analyst position | Justification |
|---|---|---|---|
| C1 | No-loss-at-ack | **Tradeable for async-surfaces ONLY if the per-surface contract publishes an explicit bounded loss window.** | User's PART 2 tradeable list names `min_acks=2 → 1` as on the table; ADR-047 already split surfaces by consistency; a third "async-bounded-loss" tier is consistent with that pattern. **Default position: keep C1**; relax only with explicit user sign-off. |
| C2 | Chunks-before-metadata | **Stays** — but may be moved from "ingress-pre-intent" to "peer-pre-apply" (Option 2 pattern) if I-L5 is restated to "no visible reader sees a composition whose chunks are not min_acks-durable" rather than "no intent is written before its chunks are min_acks-durable." | I-L5 is about reader visibility; the current ingress-side check is a sufficient but not necessary enforcement. |
| C3 | Per-key LWW | **Stays.** | Foundation of correctness; ADR-047 §1; not tradeable. |
| C4 | Per-shard total order | **Stays.** | I-L1; not tradeable. |
| C5 | Idempotency | **Stays.** | Required by every option (retry-safety). |
| C6 | POSIX surfaces synchronous | **Stays unconditionally.** | User contract with NFS/FUSE clients; ADR-013. Off the perf-design table. |
| C7 | Bounded staleness | **Stays, but the bound is a tunable.** | I-CS2 already permits per-view configuration; any option may widen the bound, but it must publish the new bound. |

Tradeable list (PART 2, with analyst notes):
- `min_acks=2 → 1`: tradeable ONLY in Option 3 territory. Requires
  explicit loss-window contract.
- `WriteSurface::is_async_ack_eligible` shape: tradeable; Option 3
  needs a new tier; Option 4 may need a new "async-resolved-via-intent"
  flag on read paths.
- `KISEKI_*_FLUSH_INTERVAL_MS` window: tradeable; all four options can
  use it as the coalescing knob.

### 3.7 Pointers for the architect (file:line)

- **Decoupled-ack write branch** (P-A/P-B critical path origin):
  `/home/witlox/kiseki/crates/kiseki-gateway/src/mem_gateway.rs:2637-2713`
  (the `if req.surface.is_async_ack_eligible()` block; this is where
  Options 1/2/3 would intercept).
- **Perspective-seq mint:**
  `/home/witlox/kiseki/crates/kiseki-gateway/src/mem_gateway.rs:621`
  (`next_perspective_seq`; the HLC tick on which all options depend).
- **Intent fan + leader-first hop** (the suspected GCP bottleneck;
  Options 1 and 2 attack this directly):
  `/home/witlox/kiseki/crates/kiseki-log/src/raft_shard_store.rs:393-495`
  (`put_intent_and_fan`; note the **MF-3 leader-first sequential
  hop** at lines 453-464 — this is the single most likely contention
  point under 16-concurrency on GCP).
- **Chunk fan (parallel; Option 2 pipelining target):**
  `/home/witlox/kiseki/crates/kiseki-chunk-cluster/src/lib.rs:710-...`
  (`ClusteredChunkStore::write_chunk`; EC branch dispatches to
  `write_chunk_ec`, replication branch fans peers via
  `FuturesUnordered` and waits for `min_acks`).
- **Intent committer** (the place coalesced incorporation already
  happens; Option 4's flush target is one level up from here):
  `/home/witlox/kiseki/crates/kiseki-log/src/intent_committer.rs`
  + `/home/witlox/kiseki/crates/kiseki-log/src/shard_committer.rs`
  + `/home/witlox/kiseki/crates/kiseki-log/src/raft_intent_sink.rs:22`
  (`IntentSink` — already batches `IncorporateIntents`; the
  ingress side is the asymmetry).
- **Recent-incorporated SM gate** (idempotent re-incorporation; any
  option must preserve this):
  `/home/witlox/kiseki/crates/kiseki-log/src/raft/state_machine.rs:318`
  (`recent_incorporated_seqs`) + line 402 (insert on incorporate)
  + line 422 (ancient-cutoff trim).
- **Composition record (Option 4's removal target):**
  `/home/witlox/kiseki/crates/kiseki-gateway/src/mem_gateway.rs:2603-2613`
  (`encode_composition_create_payload`; today it runs on the ack
  path inside the async branch at line 2641).

### 3.8 Open questions for the architect (sized)

These are the questions the architect's next pass must answer before
committing to a candidate. Listed in the order they should be
resolved:

1. **GCP measurement gap (cheapest, blocking):** instrument
   `put_intent_and_fan` to split (local-put, leader-fan-RTT,
   top-up-parallel-RTT, queue-wait) and re-run the 6-node 16-conc
   GCP put-heavy. Until this lands, every latency claim about Options
   1/2 is inferred from localhost.
2. **Workload realism (blocking for any % claim):** rebench with
   unique-content writes. Without this, every option that doesn't
   touch the chunk path looks better than it is on real workloads.
3. **C1 trade authorization (blocks Option 3):** does the user
   accept a third async surface tier with a published
   bounded-loss-window contract? If yes, what window?
4. **C2 placement (blocks Option 2):** is moving chunk-durability
   enforcement from ingress-pre-intent to peer-pre-apply consistent
   with the user's reading of I-L5?
5. **C7 ceiling (cross-cutting):** what is the maximum acceptable
   visibility lag (numeric) under each async surface? Today this is
   left to "bounded per view descriptor" — every candidate widens
   the window, and the bound needs to be explicit before the
   architect can rank them.

---

**Analyst handoff to architect.** The frame is: 10× P-A is reachable
within C1–C7 via Option 1 (batched intent fan) alone, *if* the bench
workload accurately models the perf shape — which it doesn't (§3.5d).
10× P-B is **not** reachable without (a) closing the GCP measurement
gap or (b) authorizing the C1 trade (Option 3). The cheapest next
step is not a design; it is the instrumentation in §3.8(1) + the
workload fix in §3.8(2). The architect should not commit to an
option before those two land.

---

## PART 4 — ARCHITECT DESIGN

Mode: DESIGN. Project: Brownfield with baseline. Role: architect.
Reason: pick the 10× answer for P-A within C1–C7, bound P-B, hand
to adversary.

The headline up front: the chosen design is **Option 1 (batched
intent fan) as the spine, with one well-contained piece of Option 4
(skip-the-extra-Create-encode-on-the-ack-path)** as a free rider. The
design is conservative on constraints — C1 stays strict, the only
amendment is to C7 to publish an explicit bound. No new surface tier
is introduced. **Option 3 is NOT chosen** — the loss-window trade is
not needed to reach P-A 7 000 op/s, and the analyst's read on C1 is
"default keep, relax only with explicit sign-off." We are not asking
for that sign-off. Option 2 (pipelined chunk ∥ intent) is **deferred**
until measurement (Step 1) tells us whether unique-content writes are
in the perf target — if they are, Option 2 layers on cleanly later
without re-architecture.

### (1) Measurement work required BEFORE this design ships

The analyst's §3.8 items (1) and (2) are blocking. Both are cheap
(< 1 day each, no production code), and the design's expected lift
numbers in (3) below are *contingent on what they return*.

**M-1. GCP per-phase scrape (extends `kiseki-profile`, no production
code change).** Add a `MetricsSnapshotter` that, on a configurable
interval (default 1 s) for the duration of a profile run, scrapes
`/metrics` from EVERY node (the harness already prints
`metrics_url()` per node — `crates/kiseki-profile/src/harness.rs:241`
and `:455`), parses the `kiseki_gateway_put_phase_duration_seconds`
histogram per phase, and emits a per-node time-series CSV. **Expected
output:** confirms or refutes whether `raft_commit` (the label
currently on `put_intent_and_fan`) is the dominant GCP cost and whether
it concentrates on one node (the leader-funnel hypothesis, MF-3). On
the 6-node GCP put-heavy run, we expect to see:

  - If MF-3 is the funnel: one node's `raft_commit` p50 ≫ the others'
    (the 6-node leader holds ~all the contention), AND the leader's
    p50 climbs sharply with concurrency while followers stay flat.
  - If it is not the funnel: `raft_commit` p50 is roughly uniform
    across all 6 nodes and grows uniformly with concurrency.

**Fail-fast criterion.** If M-1 shows `raft_commit` p50 < 5 ms on
EVERY node at 16-concurrency on the 6-node GCP run, the unattributed
16 ms is NOT in the intent fan. In that case **this design is wrong**
— we re-open the candidate selection (the GCP RTT or runtime
scheduling tail is the cost, not amortisable by batching). The
adversary must explicitly check that we did not commit on
unmeasured-cost assumptions.

**M-2. Split the `raft_commit` phase into sub-phases (one hour of
production code, observability only, no logic change).** Inside
`put_intent_and_fan` (`crates/kiseki-log/src/raft_shard_store.rs:393`)
add four metric observations:

  - `intent_fan{step=local_put}` — wraps `store.put(intent.clone())`
    at line 421.
  - `intent_fan{step=leader_first}` — wraps the leader-first hop at
    lines 453–464.
  - `intent_fan{step=parallel_topup}` — wraps the `FuturesUnordered`
    drain at lines 468–485.
  - `intent_fan{step=total}` — the whole function.

This is < 30 lines of code and gives the adversary the exact wait-time
decomposition the analyst flagged as missing (§3.2 methodology gap
items 1–4). **Required before the production design lands** so the
gate-1 adversary pass can attribute lift to a step, not to a function.

**M-3. Unique-content bench mode (workload fix).** Add a flag to
`kiseki-profile`'s native PUT driver to fill payloads with a per-op
unique 8-byte prefix (cheap PRNG) before encryption, defeating
dedup. Rerun the 3-node localhost baseline AND the 6-node GCP run
with this mode. **Expected output:** localhost CPU baseline rises
by ~1 ms/write (the `chunks.write_chunk` re-entry the analyst
predicted in §3.5(d)); GCP rises by ~2–3 ms/write (1 cross-node RTT
for chunk fan). **Gates the P-A target language**: if M-3 shows
unique-content GCP p50 = 22 + ~2 = ~24 ms baseline, the target
becomes "≥ 7 000 op/s on unique-content workload" and the implementer
benches against that, not the dedup-degenerate number.

**Sequencing.** M-1, M-2, M-3 are all parallelisable. M-2 takes the
single touch in production code; M-1 and M-3 are harness-only.
Estimate: < 1 day end-to-end, < ½ day for one engineer if all three
are taken at once. **None of (2)–(8) below ships until all three
land and we have read their output.**

### (2) Chosen design — "Coalesced Intent Fan" (CIF)

**Summary.** The single `WriteIntent → put_intent_and_fan` per write
(today: one RPC per write to the leader, then a parallel top-up) is
replaced by a per-shard, per-ingress *coalescing buffer*. Each ingress
node maintains, per shard it serves, a small in-memory queue of
`WriteIntent`s waiting on the next batched fan. A dedicated
shard-coalescer task flushes the queue on the earlier of (a) a
**maximum coalescing window** (default 500 µs, configurable), or (b)
**N intents accumulated** (default N = 64). The flush emits a SINGLE
`PutIntentBatch` RPC carrying all N intents to each fan target
(including the leader-first hop), and waits for `min_acks` peers to
ack the batch. Each intent in the batch is `min_acks`-durable when
the batch ack returns; the per-intent waiter is resolved at that
moment. **The fast-ack ordering and the post-ack committer drain
are unchanged.** The intent store's per-shard ascending iteration
(already in place) means the per-shard committer sees no behavioural
difference — the intents simply arrive in a burst rather than as a
steady drip.

Free-rider piece of Option 4: **the redundant per-ack
`encode_composition_create_payload` call** (`mem_gateway.rs:2641` —
the *second* encode, after the synchronous-shape one at line 2603)
is eliminated. The async path needs the encoded payload only once;
today both encodes happen on the critical path because the seq is
minted between them. We hoist the seq mint above the first encode
(or compute the delta payload once and patch the seq in place). This
saves the ~176 µs `composition_record` cost analytically observed
on localhost. It is a textual refactor, not a structural one — small
ride-along win.

**Steady-state per-write flow (one ingress, one shard, post-CIF):**

```
client PUT (64 KiB)
  │
  ▼
[1] derive_chunk_id (HMAC-SHA256)        ~100 µs CPU   ← unchanged
  │
  ▼
[2] chunk fan (ClusteredChunkStore)      ~0 dedup / ~1 ms unique RTT
  │                                        ← unchanged
  ▼
[3] mint perspective_seq + build intent  ~50 µs CPU
  │                                        ← seq + payload built ONCE
  ▼
[4] enqueue intent into per-shard CIF    ~5 µs (lock + push)
  │
  │  ──────── critical-path crossover ────────
  │  • Coalescer flushes when: queue.len() ≥ N=64
  │    OR oldest-in-queue.age ≥ W=500 µs.
  │  • Flush packs N intents into ONE PutIntentBatch RPC.
  │  • Local intent-store batch put: ONE fjall write batch
  │    for all N (already supported — fjall WriteBatch).
  │  • Fan in parallel: leader-first single RPC + topup to
  │    peers, each carrying the SAME batch payload.
  │  • Each peer's PutIntentBatch handler does ONE local
  │    fjall write batch for all N, ONE ack for all N.
  │  • On min_acks batch-ack: every waiter in the batch
  │    resolves Ok concurrently.
  ▼
[5] FAST-ACK to client     →  per-op critical path measured
                              from [1] to here.
```

Off the ack path (unchanged from today):
  • Per-shard leader committer drains the local intent store, emits
    `IncorporateIntents` batches into Raft (already batched per
    `raft_intent_sink.rs`).
  • Followers' hydrators apply the replicated delta; PART 8 ancient
    cutoff + recent-incorporated set are untouched.

**Critical-path arithmetic per write, GCP, dedup-degenerate, at
16-conc with N=10–16 effective batch size:**

```
[1] derive_chunk_id        100 µs
[2] chunk fan             ~0  µs   (dedup; ~1 ms on unique-content)
[3] seq + intent build     50 µs
[4] enqueue                 5 µs
[5] amortised fan cost:
      one-shot fan RTT  /  N
      = 5 000 µs / 12  =  ~420 µs (effective per intent)
                                  (1 ms RTT * 2 sequential hops + queueing
                                   amortised over 12 intents-per-fan)
                            ─────
                            ~575 µs per op
```

If M-1 confirms the unattributed 16 ms is leader-queue contention
(the MF-3 funnel hypothesis), the CIF flush model further amortises
the queueing — one batched RPC enters the leader's pool per coalesce
window per ingress, instead of N. The leader's RPC concurrency drops
~Nx, which collapses queueing time. If M-1 refutes the funnel
hypothesis, the design still delivers the 5×–10× lift via the
fan-RPC amortisation alone, but P-B improves less.

### (3) The 10× math

**P-A target.** 712 op/s → ≥ 7 000 op/s on the 6-node GCP profile at
16-concurrency, 64 KiB put-heavy.

  - **Current** budget on the bench shape: 22 ms/op p50, 712 op/s. At
    16-conc, this matches: `16 / 22 ms ≈ 727 op/s` — the system is
    latency-bound, not bandwidth-bound, with one op in flight per
    concurrent client.
  - **Post-CIF** per-op effective latency on the same bench shape:
    `575 µs` per op (above) + the unattributed 16 ms IF it is not
    leader-queue. Two scenarios:
    - **Scenario A — funnel hypothesis confirmed (M-1 says yes).**
      Unattributed-16 ms is leader-queue contention. CIF cuts leader
      RPC entry rate by N≈12×, queueing collapses, effective per-op
      latency ≈ 1.0–1.5 ms. 16-conc throughput: `16 / 1.25 ms = 12 800
      op/s`. **~18× headline, comfortably clears 10×.**
    - **Scenario B — funnel refuted, the 16 ms is GCP RTT + scheduling
      tail.** CIF still amortises the fan RTT 12×, but the
      unattributed 16 ms is not coalescable. Per-op latency ≈
      575 µs + 16 ms ≈ 16.6 ms. 16-conc throughput: `16 / 16.6 ms ≈
      960 op/s`. **~1.35× — does NOT hit 10×.** In this scenario CIF
      is the wrong design; Option 2 + pipelining + a separate fight
      against runtime scheduling tail would be required, and we
      would re-open the candidate selection.

  This is why M-1 is the load-bearing measurement: it picks between
  CIF delivering 18× (A) and CIF delivering 1.35× (B). The architect
  is COMMITTING to CIF for scenario A. The adversary should attack
  the assumption that M-1 lands cleanly in A.

**Per-fan amortisation factor needed for 10×.** 712 → 7 000 = 9.83×.
At 16-conc, the per-op-shareable-cost N must satisfy:

```
new_p50 ≤ 16 / 7000 = 2.29 ms
amortised intent-fan cost = (current ~5 ms fan + leader queueing) / N
N ≥ (current per-op shareable cost) / (2.29 - per-op-CPU-floor)
  ≥ ~21 / 2.0  ≈  10.5     ← Scenario A (CIF eats both fan-RTT and queue)
```

  N=10 is comfortably reachable: at 16 concurrent clients each
  emitting at the steady-state rate, the average queue at flush-time
  is `16 × (W / inter-arrival)` — with W = 500 µs and a steady arrival
  of one op per concurrent client per ~1.5 ms (post-CIF), expected
  N ≈ 5–8 per ingress per flush, **but the same shard's coalescers
  on the OTHER 5 nodes are also flushing**, so the leader receives an
  effective batch arrival rate of ~16/W from across ingress. The
  10× math is therefore reachable per ingress at N ≈ 8, comfortably
  above the floor.

**P-B (bound, not target).** P-B is bounded by the coalescing window
W. Worst-case single-op latency on a steady-state batched path
= chunk_fan + W + one fan RTT + local-put RTT ≈ 0 + 0.5 ms + 2 ms +
0.2 ms ≈ 2.7 ms. **The 22 ms p50 falls to ~3 ms p50** under Scenario A.
Under Scenario B, P-B is unchanged (the unattributed 16 ms still
dominates). Either way, **CIF does NOT make P-B worse**: the W
window only delays an op iff that op would otherwise have flushed
alone — and `gateway_put_phase{phase=raft_commit}` already shows
the alone-flush cost is dominated by RTT/queue, not by enqueue
delay.

**Low-concurrency tail-latency clamp.** A single straggler at 1
concurrent client must NOT wait W = 500 µs on the timer. The
coalescer's flush rule includes a `flush_on_idle = true` mode: a
shard whose intent rate over the last 100 ms is below a threshold
(e.g. < N=4 intents per W=500 µs sustained) skips the timer wait
and flushes immediately. This collapses to today's per-op fan on
quiet shards. Result: no straggler penalty under low load. The
threshold is a tunable knob; default sized so the bench's 16-conc
workload stays in the batched regime and a 1-conc workload stays
in the per-op regime.

### (4) Constraint accounting

| # | Constraint | Status | How (and what amendment if any) |
|---|---|---|---|
| **C1** | No-loss-at-ack | **Preserved** | Every intent in the batch is `min_acks`-durable before the batch's ack returns. The local-put step is a single fjall WriteBatch covering all N; peers do likewise. A failed batch fails ALL waiters — none are acked. The intent-fan quorum semantics are byte-for-byte the same as today, just on a wider unit of work. NO C1 amendment. |
| **C2** | Chunks-before-metadata | **Preserved** | Chunks fan and confirm `min_acks` durability BEFORE step [3]'s intent build, EXACTLY as today (the order of [2] before [3]–[4]–[5] is preserved in the CIF flow). No change to I-L5 enforcement; the coalescing happens strictly AFTER the chunks are already durable. |
| **C3** | Per-key LWW | **Preserved** | `perspective_seq` is minted per intent (step [3]), not per batch. The intent-store iteration order, the SM apply gate, and the MF-9 per-name seq-guard all consume seqs individually. CIF does not reorder seqs within a shard. |
| **C4** | Per-shard total order | **Preserved** | The committer still drains the intent store in ascending seq order and the SM still applies via the existing per-item gate. CIF affects fan, not order. |
| **C5** | Idempotency | **Preserved** | Per-intent `idempotency_key` is unchanged. The local fjall batch insert is per-intent inside the WriteBatch; the dedup pointer per intent commits atomically with its row (this already works under fjall's WriteBatch — see `FjallIntentStore::put`'s single-batch contract, lines 576–581). The PART 8 ancient-cutoff + recent-incorporated set is untouched: SM apply still sees one intent at a time. |
| **C6** | POSIX-sync | **Preserved unconditionally** | `WriteSurface::is_async_ack_eligible` is the only switch; POSIX surfaces continue to bypass CIF entirely and run synchronous Raft commit as today. Off the perf-design table. |
| **C7** | Bounded staleness | **AMENDED — publish the new bound.** | The visibility lag of an acked write is *at most* `W + one fan RTT + one committer drain interval + Raft round`. Pre-CIF: `~0 + one fan RTT + drain + Raft`. Post-CIF: `~W + one fan RTT + drain + Raft`. The W contribution to staleness is bounded by the configured ceiling (default 500 µs). **Amendment to I-CS2 spec:** publish per-async-surface a numeric visibility-lag ceiling. Proposed: ≤ 5 ms on async surfaces under steady-state. ADR-047 §F-3 already permits this; we are making the number explicit. |

**No C1 loss window.** This design does NOT trade C1. The user's
PART 2 tradeable on `min_acks=2 → 1` is on the table for a future
Option 3 design but is NOT exercised here. The adversary should
specifically verify we did not silently weaken C1 in any failure
mode (see (7), (9)).

### (5) Leader contention handling

Even if MF-3 is not the bottleneck on the current bench, 9 shards
× 6 nodes × all writes-funnelling-through-per-shard-leaders is a
structural concern. CIF's response:

  - **Per-ingress, per-shard coalescing locality.** Each ingress
    node owns coalescing for the shards it serves. The leader sees
    `(number of ingresses for this shard) × (1 batched RPC per
    flush)` — for a 6-node cluster where every node ingests, that
    is at most 6 batched RPCs per W window. **The leader's RPC
    arrival rate falls from O(writes/sec) to O(ingresses ×
    1/W) ≈ 6 × 2 000 = 12 000 batched RPCs/sec**, vs today's
    O(writes/sec) which at 7 000 op/s would be 7 000 individual
    RPCs/sec on that leader. The leader's RPC concurrency, file-
    descriptor pressure, and tokio scheduling tail all drop in
    proportion.

  - **Local-put on the leader is a SINGLE fjall WriteBatch per
    batch RPC.** This is the same fjall optimisation that the
    intent store already uses for `put` (lines 576–581) but applied
    at "all-N-in-one-commit" granularity. Expected wins: one fsync
    per batch instead of N when `sync_per_write=true`; one
    `mutations` mutex acquire instead of N; one LSM-level
    `WriteBatch` instead of N. **This is the single largest local-
    cost optimisation in the design.**

  - **Multi-shard ingress is NOT serialised.** Each shard has its
    own per-ingress coalescer task and its own per-flush RPC. A
    busy shard's flush does not stall a quiet shard. The
    coalescer is fully shard-parallel.

  - **Cross-shard fan still per-shard.** CIF does NOT batch across
    shards — the leader-first hop is per-shard. A future "global"
    coalescer across shards (one RPC per ingress per W window
    carrying all shards' intents) is a possible follow-up but is
    OUT OF SCOPE here: it would require a new multi-shard RPC verb
    and a leader-routing fan-out at the receiving end. Keep this
    in the adversary's notes as a future optimisation lever.

### (6) Real-workload (unique-content) cost analysis

The bench's 0.002 % chunk-fire-rate hides the chunk-fan RTT. M-3
will surface the real cost. CIF's behaviour under unique-content:

  - **Chunk fan is per-write, NOT in the CIF batch.** Step [2] (chunk
    fan) runs to completion BEFORE step [4] enqueues the intent.
    Under unique content, [2] adds ~1 ms cross-node RTT per write
    on the critical path, and that 1 ms is NOT amortised by CIF.
    This is correct (C2 demands chunks-durable-before-intent).

  - **Real-workload P-A.** Under Scenario A (funnel confirmed),
    unique-content per-op latency ≈ 1 ms (chunk RTT) + 575 µs (rest)
    ≈ 1.6 ms. 16-conc throughput: `16 / 1.6 = 10 000 op/s`. **Still
    clears 10× headline**, with substantial margin.

  - **The chunk fan re-enters the critical path on real workloads.**
    The 1 ms chunk-fan cost is the floor; CIF cannot lower it. **If
    the user wants > 12 000 op/s on unique-content workloads, the
    next lever is Option 2 (pipelined chunk ∥ intent), which CIF
    composes with cleanly** — the intent enqueue happens BEFORE the
    chunk durability confirms, and a per-batch "armed/unarmed"
    flag fires on chunk-durable confirm. Out of scope here, on the
    architect roadmap as the next step IF M-3 shows real-workload
    headroom is needed.

  - **What if the workload writes MANY chunks per intent (e.g. large
    objects)?** Each `WriteIntent` carries N chunk refs already; CIF
    is orthogonal to chunks-per-intent. The chunk fan parallelises
    across chunks today, so a 16-chunk write is still one round of
    cross-node RTT. No regression.

### (7) Recovery, idempotency, leader change

The hardest part of the design — and the place the adversary should
probe most aggressively.

**Crash mid-batch on the ingress.** The ingress holds a queue of
pending intents in memory between enqueue ([4]) and the next flush.
A crash before the flush loses those queued intents, **but they were
NEVER acked**. C1 is preserved: no acked-but-lost write. Clients see
no response and retry per their own protocol semantics; the
`idempotency_key` (when supplied) deduplicates a retry that arrives
after the crash-recovered ingress restarts.

**Crash mid-batch on a peer (recipient of `PutIntentBatch`).** The
peer's local fjall WriteBatch is all-or-nothing. Either every intent
in the batch is durable on that peer, or none is. The ingress
ack-counter sees a successful peer-ack only if the peer's batch
committed; a partial-batch ack is impossible (fjall guarantees this
under WriteBatch commit semantics — already exercised by the per-put
WriteBatch today).

**Crash on the ingress AFTER quorum acks but BEFORE client-ack.**
Equivalent to today: intent is `min_acks`-durable; client retries;
idempotency dedups. No new failure mode.

**Leader change during a batched fan.** Suppose the leader-first
hop succeeded and the parallel top-up is in flight when leadership
shifts. Two sub-cases:

  - **Old leader's batch was durable on the old-leader replica
    (the leader-first hop committed)** → the new leader's election-
    recovery (`recover_intents`, ADR-047 phase 5b) gathers intents
    from a majority of replicas. As long as the old-leader replica
    is in the majority gather, the batch is recovered intact and
    incorporated through the new leader's path. Same recovery
    contract as per-intent fan today.
  - **Old leader's batch was NOT durable on the old-leader replica
    (the leader-first RPC failed mid-write)** → ingress saw
    `acks < min_acks` and returned `QuorumLost` to all waiters in
    the batch. None were acked. Same as the today behaviour.

  No new orphan-intent class. The MF-3 leader-first-must-include
  invariant is preserved unchanged because the batched RPC carries
  the same fan semantics, just packaged.

**PART 8 recent-incorporated + ancient cutoff.** Untouched. The SM
applies one `IncorporateItem` at a time inside the batched
`IncorporateIntents` Raft command (already the case per
`raft_intent_sink.rs:98`). Each item walks the per-item gate; the
ancient cutoff advances on eviction as today. CIF does not change
the SM contract.

**Per-name LWW (MF-9).** The seq-guard fires on `(ns, name) →
perspective_seq` apply-time comparisons. Each intent in a CIF batch
carries its own seq, and the SM applies them in ascending seq order
within the batch. The MF-9 guard's behaviour is unchanged: it
compares the incoming seq to the live binding and refuses an older
seq. Cross-batch ordering remains the per-shard total order (C4).

**Bounded recent-incorporated set sizing.** CIF can in principle
push 6 × N intents per W onto the SM apply path on a hot shard
(across 6 ingresses), all in a short window. The recent-
incorporated set's per-shard cap (`dedup_window_entries`, default
1024, configurable) must be at least `N × num_ingresses × (apply
lag / W)`. On the 6-node profile at N=64, that is 6 × 64 × 1 ≈ 384
per apply round at steady state — comfortably under default. **No
spec change**, but the implementer must verify the default
`dedup_window_entries` is sized for the post-CIF rate.

### (8) Implementation cost / scope

**Medium.** This is not a quarter of work; nor is it a one-day
refactor. Honest estimate: **3–5 implementer-days of code + 2 days
of bench validation**, gated on the (1) measurement work.

**Crates touched, sized:**

  - **kiseki-log** (medium):
    - New `PutIntentBatch` RPC verb + handler in
      `crates/kiseki-log/src/intent_sync.rs` (mirror the existing
      `PutIntent` verb, body is `Vec<WireIntent>`).
    - `IntentStore::put_batch(Vec<WriteIntent>) -> Result<Vec<PutOutcome>>`
      trait method (default impl loops `put`; `FjallIntentStore`
      overrides with one WriteBatch covering all N). ~80 LOC trait +
      ~120 LOC fjall impl.
    - `RaftShardStore::put_intent_batch_and_fan` — the batched twin
      of `put_intent_and_fan` (lines 393–495). Reuses the same
      leader-first + parallel-top-up structure on a Vec payload.
      ~200 LOC, cleanly named to compose with the per-intent path
      until call sites migrate.
  - **kiseki-gateway** (medium):
    - New `ShardCoalescer` type in `mem_gateway.rs` or a new
      `coalescer.rs`: per-shard `Mutex<VecDeque<(WriteIntent,
      oneshot::Sender)>>`, a per-shard flusher task with the W-timer
      and N-threshold rules, and a `flush_on_idle` shape for
      low-concurrency.
    - Replace the per-write call to `log.put_intent_and_fan` at
      `mem_gateway.rs:2675` with `coalescer.enqueue(intent)
      .await_ack()`. The waiter resolves to `Ok(())` or
      `QuorumLost` from the batch result.
    - Hoist the per-ack composition-create encode (line 2641) out
      of the async branch — emit ONE encode shared with the
      synchronous fall-through path. ~30 LOC textual refactor.
  - **kiseki-server** (small): config knobs
    `KISEKI_CIF_WINDOW_MICROS` (default 500), `KISEKI_CIF_MAX_N`
    (default 64), `KISEKI_CIF_IDLE_THRESHOLD` (default 4).
    Surfaced through the runtime config.
  - **kiseki-profile / `kiseki-acceptance`** (small): M-1 snapshotter
    + M-3 unique-content mode flag (measurement work).
  - **kiseki-acceptance** (small): new `@cif` BDD scenario covering
    the per-batch quorum-shortfall failure mode (all-N-waiters
    receive `QuorumLost`); reuse existing `@flaky` retry pattern
    only if necessary.
  - **specs**:
    - ADR-048 (NEW): "Coalesced Intent Fan" — write it.
    - ADR-047 §F-3 / §C7: amend with the published visibility-lag
      ceiling (≤ 5 ms async).
    - `specs/invariants.md` I-CS2: add the numeric ceiling.
    - `specs/architecture/enforcement-map.md`: I-L2 / I-CS1 / C1
      enforcement now points at the batched WriteBatch on peers,
      not the per-intent WriteBatch.

**Code RIPPED:** none structurally. The per-intent
`put_intent_and_fan` STAYS for the single-shard / single-write
path (POSIX sync, the recovery path, and the legacy single-intent
RPC for backwards compat between rolling-upgraded nodes carrying
N=1 batched intents). The batched RPC is purely additive.

**Code REUSED:** the intent-store contract is unchanged; the per-
shard committer + sink + SM apply path are unchanged; the MF-3
fan structure is unchanged.

**Rolling-upgrade compatibility.** A pre-CIF node receives
`PutIntentBatch` and rejects it as "unknown verb." Solution: the
ingress checks each peer's advertised feature set
(handshake-time) and downgrades the fan to per-intent
`PutIntent` for peers that don't speak `PutIntentBatch`. The
fjall batch on the local node still applies — only the wire
batching downgrades. Lift is reduced during a rolling upgrade
but correctness is intact.

### (9) Risks the architect flags for the adversary to probe

These are the load-bearing assumptions of this design. The
adversary should attack each in priority order:

**R-1 (HIGHEST — measurement-gated).** The 10× lift is conditional
on M-1 returning Scenario A (leader-queue contention is the
unattributed 16 ms). If M-1 returns Scenario B (GCP RTT > 1 ms
fundamentally), CIF delivers ~1.3× and is the wrong design. **The
adversary must verify that the implementer DOES NOT START on
production code before M-1 + M-2 + M-3 have run and reported.**
The PR description must contain the M-1 output and Scenario tag.

**R-2 (high).** The batched WriteBatch on a peer must commit
atomically — all N intents durable or none. fjall's WriteBatch
guarantees this under crash, but the design depends on it. If
the peer's local fjall write succeeds but the ack RPC fails
in-flight, the ingress sees `peer didn't ack` while the peer has
N intents durable. On retry the dedup pointers (per intent) catch
the duplicates and return `PutOutcome::Duplicate`. The adversary
should check the duplicate accounting collapses correctly for
ALL N intents under partial-ack-with-retry. There is a
combinatorial case (M ack, K dedup, batch retried with new seqs
on some — does the seq window collapse correctly?) — adversary
to write a property test.

**R-3 (high).** The `flush_on_idle` threshold sizing. If the
threshold is too aggressive, low-concurrency workloads get the
W=500 µs penalty on every write (P-B regresses). If too lenient,
the bench's 16-conc workload sometimes flushes per-intent and
the lift collapses. The adversary should drive this with a low-
concurrency bench (1–4 conc) AND the headline 16-conc bench AND
a workload that ramps slowly between them, checking that the p99
tail does NOT spike during the transition.

**R-4 (medium).** PART 8 recent-incorporated set sizing post-CIF.
If `dedup_window_entries` (default 1024) is too small for the
sustained CIF throughput, the SM apply-gate's ancient-cutoff
fires prematurely and a delayed-arrival intent reads as ancient.
The adversary should compute the worst-case sustained intent
arrival rate at the SM (it is the post-CIF op/s, ~12 000 on a
hot shard) and verify the default window covers > 1 s of
arrivals.

**R-5 (medium).** The MF-9 per-name seq-guard's interaction with
batched apply. Inside the batch, all intents apply in seq order
under one SM lock acquisition — but `last_per_name_seq` must
update strictly per intent, not "per batch". If the SM batches
the `last_per_name_seq` update across the items, a within-batch
duplicate name with two seqs may collapse to the LATER seq's
binding while the earlier seq's chunks "win" the bind on a
follower replica. Adversary to check the SM apply path applies
the per-name seq-guard per `IncorporateItem`, not per
`IncorporateIntents`.

**R-6 (medium).** Rolling-upgrade peer-feature detection. The
handshake-based downgrade must be safe under network partitions
that toggle peer reachability — a peer that drops `PutIntentBatch`
mid-upgrade because it was rebooted to an older binary on the
wrong side of an upgrade-stop. The adversary should verify the
detect-and-downgrade path runs on EVERY fan, not once at
connection time, and that a downgrade-to-N=1 still preserves C1.

**R-7 (low).** Composition-create encode hoist (the Option 4
free rider). The seq is minted between the two encodes today; if
the hoist mints the seq earlier, ensure the seq's HLC monotonicity
is unchanged (specifically: that the seq mint still happens AFTER
the chunk fan completes, so seq ordering reflects chunk-durability
ordering — otherwise C2 could subtly break under concurrent
multi-ingress on the same shard).

---

**Architect handoff to adversary.** Chosen: Coalesced Intent Fan
(CIF) — Option 1 spine + Option 4 free-rider encode-hoist. Expected
lift on the 6-node GCP put-heavy 16-conc: 712 → ~12 800 op/s
(scenario A, conditional on M-1). Constraints amended: C7 visibility
lag, made explicit (≤ 5 ms). No C1 trade, no Option 3 surface tier.
Top 2 risks: **R-1 — the whole design is contingent on the M-1
measurement returning leader-queue-contention** (the adversary
must enforce measurement-before-code); **R-2 — the all-or-nothing
batched WriteBatch + ack RPC combination has a partial-ack-with-
retry case the implementer must property-test**. Adversary
priorities: probe R-1's measurement-gate enforcement, R-2's failure
matrix, R-5's per-item SM apply gate.

---

## PART 5 — ADVERSARY REVIEW

Mode: REVIEW. Project: Brownfield with baseline. Role: adversary.
Reason: gate-1 review of CIF before any implementer touches code.
One prior perf design in this same area shipped at +13% against a
5-10× prediction (ADR-047 / commit `9290e3b`). The cost of a second
wrong pivot is steep.

Stance: skeptical. The CIF design has a load-bearing measurement
gate (M-1) that the architect calls out, but several other claims do
not survive contact with the code. Findings below in priority order.

### Finding A-1: M-2 instrumentation CANNOT distinguish Scenario A from B

Severity: **Critical**
Category: Correctness > Specification compliance (the gate criterion is unmeasurable as designed)
Location: PART 4 §1 M-2 spec; `crates/kiseki-log/src/raft_shard_store.rs:393-495`

**Description.** PART 3 §3.2 honestly names the unattributed 16 ms.
PART 4 §3 makes the entire 10× claim conditional on M-1 returning
Scenario A ("leader-queue contention") vs Scenario B ("GCP RTT +
scheduling tail"). The fail-fast criterion (§1) reads: "If M-1
shows `raft_commit` p50 < 5 ms on EVERY node at 16-conc, the
unattributed 16 ms is NOT in the intent fan." That is **not enough
to discriminate A from B** — both A and B can produce `raft_commit`
p50 ≥ 5 ms; what differs is WHERE inside `raft_commit` the time is
spent. The M-2 split (`local_put / leader_first / parallel_topup /
total`) helps but does NOT separate `leader_first`'s **leader-queue
wait** from its **network RTT** — both end up inside
`leader_first`'s histogram. A wall-clock histogram around the
leader-first RPC call cannot say "the leader's tokio scheduler made
me wait" vs "the wire took N ms."

**Evidence.** `raft_shard_store.rs:453-464` is the leader-first hop;
`fan_one_intent` is a single async future. Wrapping it with
`Instant::now()` measures the round-trip — queueing + send + leader
SM lock acquisition + leader fjall write + send + receive. Without
an in-RPC server-side timestamp echo (a t-leader-received counter
returned in the response), the ingress cannot decompose the
round-trip into "time on the wire" vs "time stuck on the leader."

**Implication.** The fail-fast criterion in §1 is asymmetric: it
can identify Scenario B (low `raft_commit` p50 → not the fan), but
it CANNOT confirm Scenario A. A high p50 is consistent with A, B,
and C (mixed). Picking CIF under "M-1 returned high p50, must be A"
is the same methodology hole that produced the +13% ADR-047 result.

**Suggested resolution.** Before M-2 ships, augment the
`PutIntent` RPC response with a `server_recv_ts_us` field (HLC at
arrival on the leader). The ingress records `leader_first_wire_ms =
total - (server_recv - send_local)` and `leader_first_queue_ms =
(server_ack - server_recv)`. The fail-fast then reads: "If
`leader_first_queue_ms` < 2 ms p50, the funnel hypothesis is
refuted — CIF is not the right design." Without this, M-2 is
opaque on the load-bearing distinction.

### Finding A-2: Scenario C (mixed) is unaddressed; CIF wins under A, fails under B, unclear under C

Severity: **High**
Category: Correctness > Edge cases (architect modelled binary, reality is a spectrum)
Location: PART 4 §3

**Description.** The architect's expected lift is dichotomous: 18×
under A, 1.35× under B. Real systems on GCP at 16-conc-against-one-
leader produce **both**: a baseline cross-zone RTT (~1-2 ms wire)
PLUS leader-side queueing under contention (proportional to
arrival rate / single-writer drain rate). CIF compresses the
ARRIVAL rate by N×, which collapses queueing — good — but DOES NOT
compress the per-batch wire cost. If half the 16 ms is wire and
half is queue:
  - Pre-CIF: 8 ms wire + 8 ms queue + 6 ms accounted = 22 ms.
  - Post-CIF: 8 ms wire (still) + ~0 queue (collapsed) + 6/N
    accounted ≈ 8.5 ms per batch, but each op's effective
    latency is `8.5 ms / N + arrival_period`. At N=12,
    effective per-op p50 ≈ 1.2 ms — **still hits 10× headline**.
    But the architect's §3 arithmetic uses "575 µs" which
    implicitly assumes the 16 ms is entirely queueable. Under C,
    the per-batch RTT remains and the per-op floor is higher.

The architect has not modeled C. The lift under C is somewhere
between 1.5× and 10× and the design is committed without that
range.

**Suggested resolution.** Add a Scenario C row to PART 4 §3 with
explicit math; state the minimum acceptable lift under C ("if
M-1 returns C, CIF still delivers ≥ 3× — proceed; if < 3×,
re-open candidate selection"). Currently the design has a binary
go/no-go and reality is continuous.

### Finding A-3: M-3 (unique-content workload) is described but NOT a blocking gate before code

Severity: **High**
Category: Correctness > Methodology (same hole that produced ADR-047 +13%)
Location: PART 4 §1 / §6

**Description.** The architect lists M-3 alongside M-1/M-2 and
says "None of (2)–(8) below ships until all three land and we have
read their output." Good — explicitly stated. But §6 ("real-
workload cost analysis") then **infers** the unique-content
behaviour and concludes "still clears 10× headline" WITHOUT M-3
data. The "1 ms" chunk-fan cost on unique-content is itself
inferred from localhost flame data extrapolated to GCP — exactly
the kind of inference §3.5(d) flagged as unsafe.

The bigger problem: the bench harness in `kiseki-profile` today
writes degenerate content (PART 1: `chunks.write_chunk` ran 5 /
227 764). If the implementer ships CIF, hits ~12 800 op/s on the
SAME degenerate bench, and declares victory, the unique-content
performance is still unmeasured. The ADR-047 +13% landed precisely
because no one re-checked workload realism after the predicted
lift was banked.

**Evidence.** §1 "Sequencing" claims M-3 is < ½ day for one
engineer. Yet §6 already pre-states the unique-content P-A as
"≈ 10 000 op/s" — that number is a hypothesis until M-3 runs. PART
3 §3.8(2) explicitly named workload realism as **blocking for any
% claim**; PART 4 §1 says it is blocking, then §6 reads it as
already validated.

**Suggested resolution.** Tighten §1: M-3's output must be in the
PR description AND the headline P-A target must be re-stated post-
M-3 as "≥ 7 000 op/s on the MEASURED unique-content baseline." If
M-3 shows unique-content baseline is 500 op/s (not 712), the
target is recalibrated, not silently held to the dedup number.
The adversary recommends a hard pre-merge gate: post-M-3 bench
results posted as a comment on the implementation PR; the
auditor's gate-2 verifies M-3 ran on the **unique-content** mode.

### Finding A-4: Partial-batch fjall WriteBatch + ack-RPC failure has a silent-loss path under retry

Severity: **High**
Category: Correctness > Concurrency (idempotent retry under partial ack)
Location: PART 4 §7 + `crates/kiseki-log/src/intent.rs:552-583`

**Description.** The architect's recovery story for "crash mid-
batch on a peer" relies on fjall's WriteBatch all-or-nothing
contract. Confirmed: `FjallIntentStore::put` at `intent.rs:576-581`
commits the intent row + dedup pointer atomically. **The single-
intent contract is sound.** The architect's planned `put_batch`
extending this to N intents in one `OwnedWriteBatch` is also
defensible — fjall WriteBatch is transactional.

But §7 conflates two scenarios:
  1. **Peer-fjall-commit failed → ingress sees no ack.** Safe; no
     intent durable on that peer; ingress retries (or quorum-loss).
  2. **Peer-fjall-commit succeeded → ack RPC dropped in flight.**
     Ingress sees no-ack. Ingress retries the batch. The retry now
     hits the dedup pointers per intent — for the **(M of N) intents
     that had `idempotency_key`** these return `Duplicate(stored_seq)`,
     so the per-intent waiter resolves to `Ok` with the *original*
     seq. **For the (N − M) intents that did NOT have an
     idempotency_key** the retry RE-INSERTS them at new seqs —
     producing TWO durable copies of the same intent on that peer
     (under different seq keys).

Today this is technically harmless because the SM apply path
dedups on `perspective_seq` (recent_incorporated_seqs at
`state_machine.rs:318`) — but **the two distinct seqs make the same
intent INCORPORATABLE TWICE.** The two seqs are distinct HLC
values; the dedup set treats them as independent. The result is
two `IncorporateIntent` apply rounds for one logical write — one
binds the name to comp_id_X at seq_1, the other at seq_2. Under
MF-9's per-name LWW guard, the later seq wins, but the cluster has
applied two delta-creates for the same object.

PART 4 §4's claim "C5 preserved — per-intent `idempotency_key` is
unchanged" is true only for intents that CARRY an idempotency_key.
The async write path in `mem_gateway.rs:2665-2668` shows
`idempotency_key` is `None` unless the request supplied **exactly
16 bytes**: variable-length keys, the no-key case (every S3 PUT
that does not set `x-amz-idempotency`), and any non-16-byte key
**fall through to `None`**. Today's TODO at lines 2661-2664
acknowledges this. **Under CIF batching, the partial-ack-retry
path multiplies this gap by N.**

**Evidence.** `mem_gateway.rs:2665-2668`:
```rust
let idempotency_key: Option<[u8; 16]> = req
    .idempotency_key
    .as_deref()
    .and_then(|k| <[u8; 16]>::try_from(k).ok());
```
A 32-byte client-supplied key → `None`. No client key → `None`.
The TODO above says "a stable 16-byte derivation … would give
exactly-once across re-ingress. Left as a follow-up."

**Suggested resolution.** Block CIF on closing the TODO at
`mem_gateway.rs:2661-2664` first: derive a 16-byte
idempotency_key from `(perspective_seq, hashed_key)` so EVERY
intent has one. This is a few lines of code, eliminates the
double-store path under CIF retries, and is independently
useful for the existing per-intent path. **The architect's R-2
"adversary writes a property test" punts this to test
coverage; the adversary's recommendation is to fix the source
issue, then the property test confirms it.**

### Finding A-5: The architect's "free-rider" hoist saves a cost that already conditionally runs, but mis-locates where

Severity: **Medium**
Category: Correctness > Specification compliance (claim doesn't match code)
Location: PART 4 §2 / `crates/kiseki-gateway/src/mem_gateway.rs:2603,2641`

**Description.** PART 4 §2 says: "the redundant per-ack
`encode_composition_create_payload` call (line 2641 — the *second*
encode, after the synchronous-shape one at line 2603) is
eliminated." The architect's diagnosis of WHICH encode is wasted
is backwards.

The actual code shape:
  - Line 2603 unconditionally builds `comp_payload` (no seq).
  - Line 2637 branches on `is_async_ack_eligible()`.
  - Line 2641 (inside the async branch) builds
    `async_comp_payload` (WITH seq).
  - The async branch returns at line 2694 — it never uses
    `comp_payload`.
  - The sync branch at line 2736 uses `comp_payload`.

So on async surfaces (S3, Native — the lift target), the
**line-2603 encode is wasted work**; line 2641's encode IS the one
actually used. The fix is to push line 2603 INSIDE the
`else` branch (or compute lazily). The architect describes hoisting
the seq mint above line 2603, which is the OPPOSITE direction —
that would force the seq onto sync surfaces too, which the comment
at 2598-2602 explicitly says is wrong ("sync surface passes
`perspective_seq = None`").

**Implication.** Low-risk on its own — a textual refactor in the
wrong direction is still a refactor, and the implementer would
spot it. But it indicates the architect did NOT actually trace
the code path before writing PART 4 §2. That same trace-laxness
is the concern across the design.

**Evidence.** Confirmed by reading `mem_gateway.rs:2595-2740`:
`comp_payload` is referenced only at line 2736; `async_comp_payload`
only inside the early-return async branch. The architect's "seq
minted between them" framing reverses cause and effect.

**Suggested resolution.** Re-write §2's free-rider description to:
"Move the unconditional `encode_composition_create_payload` at
line 2603 INTO the `else` branch so async surfaces skip the
unused encode." Estimate is unchanged; semantics flipped.

### Finding A-6: Visibility-lag ceiling (≤ 5 ms) is unenforceable

Severity: **Medium**
Category: Correctness > Specification compliance (testability gap)
Location: PART 4 §4 (C7 amendment)

**Description.** PART 4 §4 amends C7 to publish a **≤ 5 ms**
visibility-lag ceiling. Walking the worst-case path:
ack-at-ingress → CIF window (W ≤ 500 µs) → batched fan + leader-
first hop + leader fjall commit (~2-5 ms GCP) → leader committer
drain → openraft `IncorporateIntents` Raft round (~5-10 ms on the
existing `append_entries` p50 from PART 1, 113 ms mean — and
that is post-batching) → followers apply → hydrator rebinds name
index (currently ~3 ms `composition_hydrator_apply` per PART 1).
Floor sum on a quiet steady state: **~10-15 ms** even under
favourable assumptions. **The 5 ms claim is incompatible with
the existing `append_entries` cost** the same document records.

The 113 ms `append_entries` mean (PART 1) is an aggregate across
~340 writes per RPC — per-RPC latency, not per-intent. Per-intent
incorporation lag is **`(RPC_latency / batch_size) + apply +
hydrator_tick`**, ≈ `113 / 340 + 3 + 1 ≈ ~5 ms` median, with a
heavy tail. Worst case (peer slow, retry, leader change, network
jitter) trivially exceeds 50 ms.

The published bound is testable in principle (a deterministic-
timing BDD harness), but the architect did not specify the test.
Existing concurrent-same-name BDDs use 5 second deadlines (e.g.
`crates/kiseki-acceptance/tests/steps/multi_node.rs:1249,1497`).
Without a deterministic-timing test, the C7 amendment is a number
in a document with no enforcement.

**Suggested resolution.** Either (a) raise the ceiling to a
defensible number (≤ 25 ms p99, ≤ 50 ms p99.9 — matches the
real Raft-bounded path) and add a `kiseki_intent_visibility_lag`
histogram exported per shard with a Prometheus alert, or (b)
keep "bounded" without a numeric and accept that I-CS2 stays
qualitative. Stating 5 ms without a measurement plan is the
worst of both: unenforceable AND wrong.

### Finding A-7: Rolling-upgrade peer-feature negotiation does not exist today

Severity: **Medium**
Category: Robustness > Resource exhaustion (an absent mechanism the design depends on)
Location: PART 4 §8 ("Rolling-upgrade compatibility")

**Description.** §8 reads: "the ingress checks each peer's
advertised feature set (handshake-time) and downgrades the fan
to per-intent `PutIntent` for peers that don't speak
`PutIntentBatch`." A repo-wide grep for `feature_set | advertised
feature | peer feature | capability negotiat` in `kiseki-log` and
`kiseki-server` returns **zero matches**. There is no handshake-
time feature exchange between peers today. This mechanism would
have to be designed and implemented as part of CIF.

The kiseki pre-production stance ("schema/API churn is fine" per
the user's memory) **does not solve this**: rolling upgrade means
two binary versions are concurrently live for the upgrade window.
Without negotiation, the older binary receives an unknown verb
and either crashes the connection or returns an error that the
newer ingress doesn't know how to interpret as "use the old path."

**Suggested resolution.** Either (a) add a "peer features"
exchange to the existing IntentSync gRPC handshake as a scoped
sub-task within CIF (sized; tractable; estimate +0.5 day), or
(b) document that CIF is rolled out cluster-wide via the
existing "stop-the-world" upgrade pattern documented in
`docs/operations/durability.md`. Today neither is specified;
§8's claim is aspirational.

### Finding A-8: M-1 cost is under-estimated and there is no GCP A/B budget

Severity: **Medium**
Category: Robustness > Resource exhaustion (engineering time + cluster spend)
Location: PART 4 §1

**Description.** "M-1 ... < 1 day" and "M-2 ... one hour of
production code" assume the harness already has cross-node
per-node scrape coverage and the GCP cluster is ready to spin
up. PART 0 of this deliberation notes a 2026-05-30 GCP run was
already carried out for ADR-047's A/B. That cluster has been torn
down per the reference runbook ("Tear down fast"). Spinning up
another GCP 6-node `default` profile is ~30 min provisioning +
data-fill + 15-min bench at minimum each, times TWO (Scenario A
vs B requires both old + new code paths instrumented). Realistic
cost: ~½ day GCP spend + ~½ day operator time.

The architect's estimate of "< ½ day for one engineer if all
three are taken at once" is for the CODE work, not the cluster
work. The full M-1 cycle is closer to 1.5 days end-to-end.

**Implication.** Low severity for design correctness, but
material for scheduling. If the implementer reads "< 1 day" and
starts coding on day 2, they have started without the M-1 data.
The R-1 gate the architect identified as highest-priority is
implicitly weakened by the under-estimate.

**Suggested resolution.** Re-state §1 with explicit "harness
work: < ½ day; cluster A/B: ~1 day; total elapsed: ~1.5 days,
gates §(2)-(8)." Budget the GCP run.

### Finding A-9: `flush_on_idle` threshold is a stability hazard at the bench/prod transition

Severity: **Medium**
Category: Robustness > Edge cases (load transitions)
Location: PART 4 §3 ("low-concurrency tail-latency clamp")

**Description.** §3 acknowledges the straggler-at-1-conc problem
and proposes `flush_on_idle = true` keyed on a per-shard rate
threshold ("< N=4 intents per W=500 µs sustained"). The threshold
is not specified beyond the default. At the transition between
idle and contended modes, a workload that oscillates around the
threshold flips between per-op-fan (~22 ms p50) and batched-fan
(~1 ms p50). p99 tail spikes at the transition; the architect's R-3
admits this risk but offers no resolution.

**Evidence.** Hysteresis is not mentioned in §3. The threshold
description is single-valued (no upper + lower hystereses). A
default sized "so the bench's 16-conc workload stays in the
batched regime and a 1-conc workload stays in the per-op regime"
is consistent with the bench but leaves the 2-8 conc range —
where real S3/Native workloads live — undefined.

**Suggested resolution.** Add two-threshold hysteresis (enter-
batched at ≥ N=4, leave-batched at < N=2), and a BDD scenario
covering the transition (load ramp from 1-conc to 16-conc and
back, asserting p99 stays bounded across the sweep). Without
this, the transition behaviour is at the implementer's
discretion.

### Finding A-10: Per-name LWW across batched-apply has a quietly-acknowledged correctness corner

Severity: **Medium**
Category: Correctness > Concurrency (within-batch ordering of same-name binds)
Location: `crates/kiseki-composition/src/persistent/fjall.rs:580-628`

**Description.** PART 4 §4 / §7 claim MF-9 is preserved because
each intent applies through `apply_one_incorporate` individually.
True at the SM level. But the FjallCompositionStore's batched
`apply_batch` at `fjall.rs:580` reads `name_seqs.get(&new_key)`
BEFORE the WriteBatch commit, then proceeds through ALL inserts
in the loop using that pre-batch snapshot. The comment at
lines 585-592 admits: "incremental updates within one batch are
not exercised today (one hydrator tick = one batch = at most one
bind per name in practice ... If multiple binds for the same name
ever land in one batch, the LWW guard's commutativity ... keeps
the outcome correct on next-tick re-apply."

**Under CIF this assumption changes.** If a peer receives a batch
of N intents that includes two intents binding `(ns, name)`, the
hydrator now sees them in the same batch routinely (not
"never"). The "next-tick re-apply" promise relies on the hydrator
being re-scanned, but the loop in `fjall.rs:593-629` commits
exactly once per call — the LWW guard does NOT see the
within-batch precedence. The commutativity argument
(stale-write-loses-regardless-of-order) holds for the FINAL
state but a transient observer between batches sees the
intermediate state.

For S3 GET, the read path consults the persistent name binding;
the transient incorrect bind is observable. Under high
concurrency the architect's claimed "no correctness change" is
not strictly true — the observable correctness changes.

**Evidence.** `fjall.rs:585-592` (comment) + `fjall.rs:596-628`
(loop reads pre-batch snapshot once, writes per-iteration into
same WriteBatch). The persistent-storage in-mem twin at
`storage.rs:705` has the same shape.

**Suggested resolution.** Inside `apply_batch`, when
`batch.name_inserts` contains two binds for the same `(ns, name)`,
collapse them to the seq-max bind BEFORE the loop. < 20 LOC. The
adversary recommends a unit test driving two same-name binds in
one batch and asserting the seq-max wins. This is also the
right fix INDEPENDENT of CIF — it just becomes more likely to
hit under CIF.

### Finding A-11: Leader throughput floor is unverified, not eliminated

Severity: **Medium**
Category: Correctness > Concurrency (leader's local fjall remains the per-shard ceiling)
Location: PART 4 §5 (leader contention handling)

**Description.** §5 argues the leader's batched WriteBatch
collapses N fjall writes into one. True. But the analyst's PART 3
§3.7 raised: "every shard's writes still funnel through the
per-shard leader." CIF batches the RPC arrival rate at the
leader, but the leader's own LOCAL processing rate (Raft append +
SM apply + replication) is unchanged. Today's
`gateway_put_phase{phase=raft_commit}` on the leader for the
LOCAL put step (which is what `put_intent_and_fan` does first —
`raft_shard_store.rs:421`) is one fjall WriteBatch per intent.
Post-CIF, the LOCAL put on the leader is still per-shard
serialized through the leader's mutations mutex
(`intent.rs:557`). The mutex acquire-release is cheap per call;
the **fjall WAL write + fsync** is not, and that scales with N
unless the batched path commits all N in ONE leader-side
WriteBatch.

§5 claims this happens ("ONE fjall WriteBatch for all N") — but
this is a CIF DESIGN OBLIGATION, not an automatic consequence.
The implementer must build `IntentStore::put_batch` with a single
`OwnedWriteBatch`. If they instead loop `put(intent)` N times
(the default impl the architect explicitly proposes at §8
"default impl loops `put`"), the leader-side fsync cost stays
linear and the lift caps at the wire-batching win alone (~3-5×,
not 10×).

**Evidence.** `intent.rs:552-583`: `put` takes `mutations` lock,
builds ONE WriteBatch, commits. Cost is dominated by `batch.commit()`
(the WAL fsync at `sync_per_write=true`). N calls = N fsyncs.

**Suggested resolution.** §8's "default impl loops `put`" is
explicitly insufficient for the perf win. The architect MUST
require that `FjallIntentStore::put_batch` overrides with a
single-batch single-commit implementation. State this as a
non-negotiable in §8, not as an aside. The adversary's stronger
recommendation: add a debug-build assertion `put_batch` actually
runs as ONE commit (count commits during test).

### Finding A-12: CIF is committed without published numeric thresholds for "succeeded"

Severity: **Low**
Category: Robustness > Observability gaps (no objective post-implementation gate)
Location: PART 4 §3, §8

**Description.** The architect's headline metric is 7 000 op/s on
the 6-node default profile, 16-conc, put-heavy. But §6 also
floats "12 800 op/s" (Scenario A) and "~10 000 op/s" (unique-content
under Scenario A). The PR's success criterion is undefined: is 7
000 the bar? 10 000? Does Scenario B at 1.35× = ~960 op/s count
as "success of the design but not the target"? §8 specifies
implementer cost but not a measurement-based stop condition.

**Implication.** The author of the implementation PR will pick
their own success bar. Past behaviour (ADR-047 shipping at +13%
of a 5-10× claim) suggests this is not safe.

**Suggested resolution.** State explicit P-A thresholds for each
M-1 outcome:
  - Scenario A confirmed: post-impl bench must show ≥ 7 000 op/s
    on unique-content. Less than that = re-open.
  - Scenario B confirmed: CIF is not built. Re-open candidate
    selection (likely Option 2 + pipelining).
  - Scenario C / mixed: CIF must show ≥ 3× lift on unique-content
    to merge.

### Severity tally

| Severity | Count |
|---|---:|
| Critical | 1 (A-1) |
| High | 3 (A-2, A-3, A-4) |
| Medium | 7 (A-5, A-6, A-7, A-8, A-9, A-10, A-11) |
| Low | 1 (A-12) |

### Verdict

**ACCEPT-WITH-CHANGES.** The design's spine (batched intent fan via
a coalescing buffer with all-or-nothing per-batch quorum) is
structurally sound and stays inside C1–C6 cleanly. PART 8 + MF-9
interactions hold under per-item SM apply. The leader contention
analysis is honest. But the design ships with: (i) a measurement
gate (M-1) whose discriminator is unmeasurable as currently
specified (A-1), (ii) two missing scenarios in the lift math
(Scenario C, A-2), (iii) a workload-realism gate (M-3) the design
text already pre-judges as passed (A-3), and (iv) a partial-ack-
retry double-store path that exploits the existing
idempotency_key TODO (A-4).

**Must-fix before implementer starts:**
  1. **(A-1)** Add a `server_recv_ts_us` field to the `PutIntent`
     RPC response and re-frame M-2 to split `leader_first` into
     `leader_first_wire` and `leader_first_queue`. Without this,
     the M-1 fail-fast cannot discriminate the funnel hypothesis.
  2. **(A-3)** Re-state P-A targets POST-M-3 against unique-content
     baseline. The implementation PR description must include
     M-3's measured number AND a Scenario A/B/C tag from M-1.
  3. **(A-4)** Close the idempotency-key TODO at
     `mem_gateway.rs:2661-2664` (derive 16-byte key from
     `(perspective_seq, hashed_key)`) BEFORE the CIF batched path
     lands. This is a small change; it eliminates a real
     correctness gap that CIF amplifies.
  4. **(A-11)** Pin `FjallIntentStore::put_batch` as a
     mandatory-override with a single-WriteBatch single-commit
     implementation. Forbid the default loop-`put` impl in
     `kiseki-log`.
  5. **(A-2)** Add Scenario C to the design math with an explicit
     minimum lift threshold; tie the merge gate to it.

**Should-fix before implementer merges:**
  6. **(A-5)** Reverse the direction of the "free-rider" hoist
     in §2 — the wasted encode is line 2603, not line 2641.
  7. **(A-6)** Either drop the 5 ms numeric or replace it with
     a defensible bound + a measurement export.
  8. **(A-7)** Specify the peer-features handshake explicitly,
     or document the cluster-wide stop-the-world upgrade path.
  9. **(A-10)** Add same-name-collapse inside
     `FjallCompositionStore::apply_batch` and a unit test.
 10. **(A-12)** Publish explicit P-A thresholds per M-1 outcome.

**Could-fix (post-merge):**
 11. **(A-8)** Re-state M-1 budget as ~1.5 days end-to-end with
     the GCP A/B run scoped in.
 12. **(A-9)** Add hysteresis to `flush_on_idle` + a BDD covering
     the load-ramp transition.

**Architect handoff back.** The design is salvageable; CIF on the
spine is the right shape. The musts above are not redesigns —
they are gaps the architect should fill before the implementer
opens a PR. R-1 must be **PROVABLE**, not assumed; A-4 must be
closed BEFORE CIF lands, not after; the headline number must be
measured against a non-degenerate workload. Without these, this
ships at +13% again.

