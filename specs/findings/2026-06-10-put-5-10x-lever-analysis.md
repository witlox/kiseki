# 2026-06-10 — Native PUT 5-10× lever analysis (internals only)

26-agent deep read of the write path (gateway, Raft round, ADR-047
substrate, chunk/composition/hydrator legs), the full attempt ledger
(git history + all GH issues + perf snapshots + ADR-046/047), the
durability contract, and the measured profile evidence. Six candidate
levers synthesized; each adversarially verified through three lenses
(code premise, history collision, arithmetic + invariants).

Baseline: 6,054 op/s PUT at conn=1/conc=16×3 clients (4 KiB, 6-node
GCP default profile, 2026-06-04); 9,873 at conn=16. Target for 5-10×:
30-100k op/s.

## Verdict summary

| Lever | Score | Disposition |
|---|---|---|
| **C1 SmallObjectStore group commit** | 5/6 | **Recommended #1** |
| **C2 Buffered intent durability (spec-approved default)** | 5/6 | **Recommended #2** |
| C3 Overlap local intent write with peer fan | 4/6 | Subsumed by C2; fold in if trivial |
| C5 Committer size-wake + hydrator drain | 4/6 | Committer half refuted; **hydrator half is a mandatory guard rail** |
| C6 Voter set 6-of-6 → 3-of-6 | 4/6 | Real but needs ADR + placement work; rank last |
| C4 Drop save_committed fsync | 3/6 | **Refuted** — the W6-ON A/B (2026-06-01) already ran with it removed; flat |

## The measurement precondition (binds everything)

The 2026-06-04 run was Little's-law-exact: 48 in-flight × 7.93 ms =
6,054 op/s. Of the ~7.9 ms client-visible service, only ~2.5-4 ms is
server-side; the remainder is client↔server dispatch gap. **At fixed
48 in-flight, an infinitely fast server still caps at ~7.5k op/s.**
These levers raise *ceilings*; the A/B that evaluates them MUST
include the never-run saturation sweep (conc 64/128/256 ×
connections 1/4/8, warm both arms) or any result will read as
neutral for the same reason W12's batches stayed near fill 1.

Instrumentation rules for the A/B: trust only `chunk_fan_inner` /
`intent_fan_inner` / `parallel_fan_wall` + `pif.*` / `sm.*` spans —
the legacy `chunk_write` / `raft_commit` / `composition_record`
labels are aliased (#164, `mem_gateway.rs:3214-3240`, `:3300-3305`).
The gateway small-store put (C1's ack-path target,
`mem_gateway.rs:~2804`) is currently **untimed** — add a histogram
before the A/B or the win is invisible.

---

## Lever 1 (C1) — SmallObjectStore group commit

**The only store whose per-write fsync was never relaxed.**

`SmallObjectStore` defaults `sync_per_write=true`
(`crates/kiseki-chunk/src/small_object_store.rs:96`). The sole
production relax call (`crates/kiseki-server/src/runtime.rs:1203`)
targets `PersistentChunkStore`; the small store created at
`runtime.rs:644-665` keeps `PersistMode::SyncAll` per write. One
Arc'd store is shared by the gateway (`runtime.rs:2141-2147`) and
every locally-hosted shard SM (`runtime.rs:762-764`) → **one fjall
journal `Mutex<Writer>` with fsync held under the lock**, contended
by the ingress hot path and all local appliers.

Per 4 KiB (inline-tier, #129) PUT it is hit with SyncAll:

1. **Gateway ack path, serial, before the try_join fan** —
   `mem_gateway.rs:2799-2822`. A blocking fsync executed directly on
   a tokio worker. This is NOT the W6 trap: W6 coalesced a fsync that
   sat *parallel* to equally-long branches; this one is sequential
   ahead of the fan.
2. **Every replica's SM apply, under the per-shard SM mutex** — the
   #129 inline-payload put loop (`state_machine.rs:498-512`; leader
   usually dedup-skips since the gateway already wrote the chunk_id —
   `small_object_store.rs:152-160`) and `append_delta_inner`'s
   always-unique key (`state_machine.rs:585-596`) which **always
   commits on every replica**.

Measured: `sm.append_delta_inner` mean 4.22 ms on GCP vs 0.6-1.5 µs
loopback (`pprof-out/gcp-2026-06-04/hotpath-mid.txt`); the 4.22 ms is
journal-contention-inflated (42 entries × 4.22 ms ≫ the 33 ms
`committer.sink_incorporate` wall — consistent only as cross-shard
queueing on the shared journal, which itself confirms the mechanism).
`sink_incorporate` = 33 ms per 42-85-entry batch ≈ a serialized-fsync
pipeline that caps phase-2 incorporation at roughly **8-15k op/s
cluster-wide** (possibly lower).

**Fix shape** (mirrors the landed chunk-store pattern at
`runtime.rs:1196-1228` and composition pattern at `:1903-1949`; the
toggle + flush APIs already exist at `small_object_store.rs:105-138`,
`:261-270`):

- `set_sync_per_write(false)` + periodic flusher + registration in
  the `fsync_pending` hook chain.
- Fold the per-entry apply puts into **one fjall batch per
  IncorporateIntents batch** (~85 inline fsyncs → 0 inline +
  1 periodic).
- Add the missing gateway-put histogram first.

**Durability argument:** at ack the identical inline bytes are
quorum-durable in the fsynced intent (`mem_gateway.rs:3173-3175`);
the small store is a local materialization. One correction from
review: "SM replay re-seeds the store" does NOT hold for the
applied-index-ahead crash case — the change needs a flush-ordering
note (small-store flush before applied-index persist / log
truncation) and a kill -9 recovery e2e. Slightly beyond "zero spec
work" but small.

**Expected lift:** 4-6× on the sustained incorporation ceiling
(corrected band: ~25k static-batch to 50-150k with batch growth);
~1.4× on server-side ack latency. Per-PUT cluster-wide SyncAll count
drops from ~6-13 (RF-dependent) to ~0 inline.

**History check:** clean — no commit, issue, ADR, or perf spec ever
touched this store's durability mode. W6's rejection does not apply
(serial vs parallel-masked); S2's lesson (only fsync *count* matters
on fjall) is exactly what this follows.

---

## Lever 2 (C2) — Ship buffered intent durability as the multi-node default

**The code is stricter than the spec.** ADR-047 rev-2 O4 specifies
the default ack-durability point as *page-cache on min_acks=2 nodes*
(I-L5 group-commit window). The code instead fsyncs every coalesced
intent batch on the producer (`intent.rs:680`, `:704-711`, `:816-823`)
AND on each fanned peer (`intent_sync.rs:173-234`). The
`KISEKI_INTENT_FLUSH_INTERVAL_MS` knob (737a724) already implements
the mechanics but is diagnostic-gated.

**⚠ Durability bug found in the landed knob:** relaxed mode commits
with batch durability `None` (`intent.rs:705-710`), which leaves
acked batches in fjall's **user-space BufWriter** — not OS page
cache (fjall-3.1.4 `journal/writer.rs:25,177`). I-L5/O4 approve
page-cache-on-min_acks only. The ship form (and the existing knob)
must use `PersistMode::Buffer` (flush-to-OS, no fsync). The
discriminating falsifier is a **simultaneous kill -9 of min_acks=2
nodes** — passes with Buffer, fails with None. A single-node kill
proves nothing (min_acks=2 already covers it).

**Fix shape:** flip the default for multi-node deployments with
`PersistMode::Buffer`, wire `FjallIntentStore::flush`
(`intent.rs:697-700`) into the gateway `fsync_pending` chain
(crosses the LogOps crate boundary — the per-shard stores live
behind `RaftShardStore`), update `docs/operations/durability.md`
loss-window table.

**Expected lift:** ~1.3-2× standalone on the ack/fan cycle (the
W7/W11/W12 history shows fsync-shaving yields sub-proportional gains
while #207's unaccounted round latency survives), **multiplicative
with C1** — they attack two distinct fsync populations (small-object
journal vs intent journal). Combined post-C1+C2 computed ceilings:
~50-96k (intent fan) and ~84k+ (phase-2).

---

## Mandatory guard rail — hydrator (the #133 residue)

Past the compaction horizon the hydrator enters **permanent halt
requiring an operator metadata wipe** (`hydrator.rs:276-285`).
Optimistic ceiling today ≈ 10k deltas/s/shard
(window 1000 / poll interval); sustained 30k+ PUT without the fix
bricks follower visibility. `KISEKI_HYDRATOR_POLL_MS`
(`hydrator_registry.rs:62-65`) already exists — A/B it at 1-10 ms
with zero code change; the real fix is a drain-while-full re-poll
(~5 lines). Note: post-53dd58f hydration throughput was never
re-measured, so confirm the per-delta cost first.

The committer half of #208 was refuted in review: `Committer::run`
drains ALL pending per pass in `DRAIN_BATCH_CAP` chunks
(`intent_committer.rs:182-191`) — the 50 ms tick is a latency
quantum, not a throughput cap; batch fill self-scales with arrival
rate. A size-based wake adds ~0 throughput.

## Refuted / deferred

- **C4 (drop save_committed fsync):** mechanism verified
  (`fjall_raft_log_store.rs:219-221` is a real per-commit SyncAll,
  openraft's own default is a no-op) but the 2026-06-01 W6-ON A/B
  effectively ran without it on every node and was throughput-flat.
  Folding it into C1's flusher is fine as hygiene; it is not a lever.
- **C6 (3-of-6 voters):** confirmed that every shard enrolls all 6
  nodes as voters (`runtime.rs:761` → `raft_shard_store.rs:976-984`).
  Halves the phase-2 fan budget but does NOT shrink the
  min_acks-bounded intent fan (would slightly increase remote sends),
  needs membership/placement/recovery work + ADR. Revisit only if
  post-C1+C2 profiling shows the replication fan as the next binder.

## Execution order

1. Add gateway small-store put histogram; re-baseline current main
   warm (numbers move across snapshots: intent fan 9.3 → 4.9 →
   2.48 ms under different shapes).
2. C1 (small-store group commit + batched apply puts) — A/B with
   saturation sweep.
3. C2 (+C3 fold-in) with `PersistMode::Buffer` + the two-node
   kill -9 falsifier.
4. Hydrator drain-while-full before any sustained-load soak.
5. Re-profile; C6 only if the replication fan is now the binder.

Full workflow output (7 maps, 6 candidates × 3 verdicts):
session artifact `w62a3hd2k`, 2026-06-10.
