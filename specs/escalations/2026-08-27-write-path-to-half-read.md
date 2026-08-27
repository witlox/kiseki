# Write path to half-of-read — feasibility, blocked at the design layer

**Date:** 2026-08-27
**HEAD:** `925a1be` (main; last write-path commit `88d9d53` per #226, 2026-06-11)
**Asked of:** analyst/adversary — "the architecture promises a write
performance the implementation doesn't deliver. Can we lift writes to
at least half of reads?"
**Stance:** adversary. Every load-bearing claim verified against commit
history and the dated snapshot record, not against the live roadmap doc
(which has been revised several times and is, per its own header,
superseded by the 2026-06-01 GCP passes).

## Cross-references

- [`2026-05-29-adr047-committer-redesign.md`](2026-05-29-adr047-committer-redesign.md) — the design this analysis concludes is the load-bearing lever and simultaneously not buildable as specified.
- [`../performance/2026-06-10-gcp-p123-validation.md`](../performance/2026-06-10-gcp-p123-validation.md) — latest measured snapshot (PUT ceiling 22.3k, GET 296.8k).
- [`../../docs/performance/targets.md`](../../docs/performance/targets.md) §360k-retraction — the audited budget (100–200k conditional).
- [`../../docs/performance/roadmap.md`](../../docs/performance/roadmap.md) — live gap+plan layer (overtaken; see §5).
- [`../../specs/architecture/adr/042-native-gateway-data-service.md`](../../specs/architecture/adr/042-native-gateway-data-service.md) §14 — the per-binding budget the architecture actually commits to.

---

## 1. Verdict

**Half-of-read is numerically inside the architecture's envelope and
practically blocked at the design layer — not at the optimisation
layer.** The 148k target (half of the latest measured 296.8k GET) sits
at the top of the audited burst budget (100–200k). Closing the gap is a
protocol redesign (ADR-047 LeaderSink) plus two state-machine ADRs,
*not* a series of perf tweaks — and every perf tweak that looked
straightforward on paper has been built, measured, and falsified over
the last ten weeks.

The single load-bearing risk, visible only in the commit/snapshot
history rather than the live roadmap: **each write-path lever proposed
in this period has had roughly even odds of being falsified by the next
measurement**, usually because the removed serialisation sat in
parallel with something else of the same length. ADR-047 is the right
*idea*; the adversary's 2026-05-29 review proved it rests on three
mechanisms that do not exist in the tree.

## 2. The numbers

Latest snapshot ([2026-06-10 — P1/P2/P3 validation](../performance/2026-06-10-gcp-p123-validation.md), `default` profile, 6 × c3-standard-22-lssd + 3 clients):

| Op | Measured | Status |
|---|---:|---|
| GET (get-heavy, conn1/conc16 × 3) | **296,808 op/s** aggregate · p99 ~240 µs | target met (+2.3%) |
| PUT (put-heavy 4 KiB, conn1 × 3, cold throwaway) | **22,275 op/s** · p99 132 ms (warm) | bar (48k) **NOT met** |
| PUT warm conc64 | 22,272 op/s · p99 132 ms | ceiling (no decay) |
| PUT warm conc128 / conc256 | 16,282 / 18,041 op/s | single serialisation point remains |

**Read/write ratio: 296.8k / 22.3k ≈ 13×.** The asked target (write ≥
half of read) is **~148k** — a **6.6× lift** on writes.

The audited budget (targets.md §360k-retraction, GH #226):

> per-op ingress CPU floor ≈13 µs (crypto 3.5 µs FIPS-irreducible) →
> **100–200 k is inside the architecture's envelope**, but: burst-100k
> needs the named engineering levers …; **SUSTAINED-100k additionally
> needs two ADR-grade SM changes** (disk-backed `cluster_chunk_state` —
> RAM grows O(objects), ~2 h OOM horizon at 100 k/s — and off-mutex
> incremental snapshots) plus #261 visibility backpressure; **200 k+
> needs the ingress batch-quorum protocol change.** Measured today:
> 25.6 k fresh / 10.6 k at 4 M objects — the gap to budget is **65×
> WAIT (blocking occupancy 2.6 ms/op vs ~40 µs CPU), not work.**

148k is therefore inside burst, just outside sustained-100k, and well
under the 200k+ protocol-change line. **Numerically yes; the path is
steep.**

## 3. The trial-and-error ledger (the load-bearing evidence)

The live roadmap lists levers; the commit history shows what happened
when they were built. This is the evidence the user asked me to surface,
and it is the part the live docs under-weight.

| Lever | Built (SHA / PR) | Measured result | Verdict |
|---|---|---|---|
| **W1** batched Raft commit (ADR-046) | `3fd8cc8`, reverted 2026-05-29; retraction `92e5c5a` | flat 254 vs 264 op/s local A/B | **REVERTED.** openraft already auto-batches (`max_payload_entries: 300`); W1 amortised what openraft amortises. The premise (35 ms RTT/write) was the wrong number — the round is ~1 ms. |
| **W6** log fsync coalescing (#152) | `a46d808`, measured Pass 4 2026-06-01 | fsync 12.4→12.4 ms; server floor 8.1→8.2 ms; p50 3.95→3.89 ms (noise); p99 108→93 ms (−14%, tail-only); sched-switches −41% (as predicted) | **REJECTED at production parameters.** The fsync wait sits in a `try_join!` fan *in parallel* with `chunks.write_chunk` and the AppendEntries RTT. Removing one branch by 3 ms does nothing when the others are the same length. Code stays in, gated off (`KISEKI_RAFT_FSYNC_WINDOW_US`). |
| **PR #182** L1+L2 receiver coalescer + producer timeout | reverted in `be27d33` (#184) | — | **REVERTED.** |
| **#267** blind-write SmallObjectStore hot paths | `6d97baa` (2026-06-12) | 1.579M vs 1.557M ops (+1.4%, noise); at-volume intent-store commits 72 ms vs 55 ms (worse) | **FALSIFIED.** The at-volume stall hits even the deliberately-shallow epoch-partitioned intent store → the contention is **device-level** (compaction I/O saturating the shared disk, queueing every store's journal commit), not the LSM reads the hypothesis claimed. |
| **W2** distributed shard leaders (#99/#111/#114) | landed | 6 leaders, one per node | **Landed.** Multiplier W1 batches within; no further work. |
| **W5** ForwardToLeader WARN→DEBUG (#149) | landed 2026-05-31 | 22k WARN/run → 0; ~0.05% per-write CPU | **Landed (signal clarity, not a perf lever).** |
| **#226** P1/P2/P3 (ack-path journal write removed, batch apply, bounded SM delta history) | `41b23b5` (2026-06-10) | ceiling 6k→22.3k (3.7×); decay eliminated; `inline_cache_insert` 5,300→7.3 µs; `sm.append_delta_inner` 4,220→0.43 µs | **Landed.** The two binders below are what remain. |
| **#253** batch-aware pipelined submission + topup rescue-save | `5317a36` (2026-06-11) | 894 errors@60s → 0 | **Landed (correctness, not throughput).** |
| **#255** byte-budget catch-up replication + frame-cap escape hatch | `be746cb` (2026-06-11) | — | **Landed (recovery, not steady-state).** |
| **#256** O(1) pending-overlay removal via by_id index | `e00c9e5` (2026-06-11) | perf-gate baseline bumped here | **Landed (removes a decay class).** |

**Pattern.** Of the four write-path levers that looked like clear wins
on paper (W1, W6, #182, #267), **four were falsified by measurement**,
each time because the real bottleneck was elsewhere (a parallel branch
of equal length, or device-level I/O rather than the named hot spot).
The landed wins (#226, #253, #255, #256) moved the ceiling 3.7× and
eliminated the decay/tail classes — but the two remaining binders are
each ~1,000× the rest of the path, and the "65× WAIT, not WORK" framing
means the next lever is a queueing/serialisation redesign, not a
"make-the-code-faster" change.

## 4. The two surviving binders (2026-06-10 profile)

From `specs/performance/2026-06-10-gcp-p123-validation.md`, 1.03M
ingress ops, phase decomposition:

| Phase | Mean | vs. rest of path |
|---|---:|---|
| `inline_cache_insert` (P1) | 7.3 µs | ×726 (was 5,300 µs) |
| `encrypt` | 8.1 µs | — |
| `chunk_fan_inner` | 0.07 µs | inline: no fan |
| `sm.append_delta_inner` (P2+P3) | 0.43 µs | ×9,800 (was 4,220 µs) |
| **`pif.total` ≡ `intent_fan_inner`** | **12.0 ms** | **THE wall (`parallel_fan_wall`)** |
| **`composition_record`** | **13.5 ms** | **second binder** |

Everything P1/P2/P3 targeted collapsed by 3–4 orders of magnitude. The
ack wall is now **entirely** the intent-fan leg (12.0 ms) with
`composition_record` (13.5 ms) beside it — the latter is the fjall
composition store under ingress load despite 100 ms buffered mode
(#200-class: journal mutex + per-op overhead). **No finer `pif.*`
sub-spans were exported in this build** (only `pif.total`); in-fan
attribution (ingress→leader forward hop vs. coalescer cap 16 vs.
per-peer pool vs. remote intent-put service time) is the first task of
the next pass. A live A/B of `KISEKI_INTENT_FAN_BATCH_MAX=128` was
prepared but not run (operator called teardown).

**Implication for half-of-read.** Even if ADR-047 eliminates the
intent-fan leg (the forward hop is the main suspect at 5/6 of writes),
the second binder (`composition_record`, 13.5 ms) is *not* addressed by
ADR-047 — it is fjall journal-mutex contention on the shared
SmallObjectStore Database under ingress load. The same
parallel-branch-of-equal-length pattern that killed W6 is the most
likely failure mode for ADR-047: remove the 12 ms forward, leave the
13.5 ms journal, ceiling barely moves. **The next lever after ADR-047
is known and is the KV-separation / shard-the-small-object-store item
called out in the #267 verdict and the #226 audit's lever 3.** ADR-047
without that is a candidate for the same fate as W6.

## 5. ADR-047 is the right shape — and is not buildable as specified

The 2026-05-29 ADR-047 deliberation ([`2026-05-29-adr047-committer-redesign.md`](2026-05-29-adr047-committer-redesign.md)) concludes LeaderSink (single-incorporator-on-leader, drop the watermark, ack on the `min_acks` floor) is the right mechanism. The adversary review in the same doc found the design **not implementable as written**, with four Critical findings:

| # | Finding | Severity | What it means for half-of-read |
|---|---|---|---|
| A | Compaction is NOT the LWW resolver for named objects; the name index is, and it has no seq guard. The architect's "load-bearing fact" is a red herring. | Critical | The correctness argument for dropping the watermark is invalid as stated. The real resolver needs a NEW durable `(ns,name)→perspective_seq` column. |
| B | The per-name perspective-seq guard (the watermark's replacement) does not exist, is under-specified, and must be applied in **three** places (hydrator in-batch, persistent apply, `stage_create` re-bind), not one. Get any wrong and the Finding-A reorder silently loses an acked write. | Critical | The "one hardening footnote" is in fact NEW LARGE WORK: a wire-format version bump + a durable replicated column + guards at three bind sites + a deletion seq-guard. |
| C | R1/R5 no-loss is **unwired**: `ShardCommitter::recover` is invoked only in tests; there is no leadership-change hook and the committer is spawned on every node, not leader-only. The architect's "recovery is the guarantee, the forward is just an optimisation" rests on dead code. | Critical | The no-loss spine does not run in production today. Wiring it (openraft `metrics()` watch → per-shard start/stop → fence-before-recover-before-resume) is the single biggest gap between the doc and the tree, and is NEW, not reused. |
| D | A new B4-class hole: ingress crashes post-ack/pre-forward, leader stable and not in the `min_acks` set → the intent sits invisible indefinitely (until an unrelated election). Strictly worse on this axis than the current broken every-node committer. | High | R4 (no permanent invisibility) violated. Requires `put_intent_and_fan` to count the leader among `min_acks` (preferred), or a periodic leader-pull. |
| E | Idempotency dedup index does not exist; `idempotency_key` is dropped at the sink boundary. F-2/O3 exactly-once unprovable as designed. | High | Either ship the dedup index (NEW) or strike the "two layers" claim and document at-least-once + LWW-collapse. |
| F | Follower/ingress intent stores grow without bound: no prune signal reaches non-leader holders. | High | Hard production blocker for sustained workloads. Followers can self-prune against their own applied `max_incorporated_seq` (no broadcast needed) — but this is unspecified. |
| G | The append-gate (skip-if `seq ≤ floor`) is missing, and the floor is read from a possibly-stale leader cache, not applied state. Double-incorporation window on the leader itself. | High | Must read the floor from the *replicated* state machine and gate the append on it. |

**What this means in one sentence.** ADR-047 passed the project's
internal review gates (analyst → architect → adversary → synthesis)
with the conclusion "build LeaderSink." The adversary review, conducted
*inside the same diamond*, then proved the conclusion rests on three
mechanisms (recovery-on-election, idempotency index, per-name seq
guard) that **do not exist in the tree** and one load-bearing
correctness argument (compaction as LWW resolver) that is **false
against the code**. The internal consistency loop closed; the code↔spec
loop did not. This is the concrete instance of the broader
workflow-level gap (spec is verified against spec; nothing in the
diamond verifies spec against the actual code for "reused / already
exists" claims).

## 6. The path to half-of-read (sequenced, honest about falsification rate)

The order is forced by the dependency: ADR-047's correctness gaps must
close before its perf claim is worth measuring, and the second binder
must move in the same window or the measurement will look like W6.

1. **Close ADR-047's four Critical + three High findings.** New work,
   not reuse: the per-name `perspective_seq` column + three-site guard
   (Finding B), the leadership-change hook + leader-only committer +
   fence-before-recover (Finding C), the idempotency index or the
   at-least-once contract (Finding E), and the leader-in-`min_acks`
   rule (Finding D). **This is where the design currently lives.**
2. **Implement LeaderSink.** `INTENT_FORWARD` aux RPC, leader-only
   committer loop, off-ack-path forwarding. Reuses `put_intent_and_fan`
   (the ack path that works) and `IncorporateIntent` (modified to gate
   on the floor).
3. **Measure against `pif.*` decomposed, not against `pif.total` alone.**
   The 2026-06-10 build did not export `pif.*` sub-spans; the first
   re-measurement after LeaderSink MUST restore them, or the result
   will be the same "the wall moved, we don't know why" shape as #267.
4. **In the same window, attack `composition_record` (the second
   binder, 13.5 ms).** The known lever is fjall KV-separation (4 KiB
   values out of the compaction path) or sharding the SmallObjectStore
   into N Databases. **Without this, LeaderSink risks the W6 fate** —
   removing a 12 ms serialisation that sat in parallel with a 13.5 ms
   serialisation.
5. **The two sustained-100k SM ADRs** (disk-backed `cluster_chunk_state`
   — RAM grows O(objects), ~2 h OOM horizon at 100 k/s — and off-mutex
   incremental snapshots) **plus #261 visibility backpressure.**
   Required to hold 148k sustained, not just burst.
6. **Re-measure on the 6-node `default` profile.** If 148k is reached
   at burst but not sustained, the gap is the two SM ADRs (step 5). If
   148k is not reached even at burst, the second binder was not the
   only one and the next profile pass (step 3's `pif.*` decomposition)
   names the third.

**Time.** Steps 1–4 are the ADR-047 implementation done honestly:
design-amend (close A–G) + leader-only committer + per-name guard +
idempotency index + leader-in-`min_acks` + KV-separation. That is a
multi-week protocol change, not a perf sprint. Step 5 is two more
ADR-grade state-machine changes on top. **A quarter is plausible if the
falsification rate holds at the historical average; aggressive if it
does not.**

## 7. Risk assessment

The historical pattern (§3) is the single most predictive signal
available, and it is not encouraging for a single-lever plan:

- **W1, W6, #182, #267** — four for four falsified by the next
  measurement. The common failure mode: the lever targeted a
  serialisation that sat in a `try_join!` fan or parallel I/O path with
  another branch of comparable length, so removing it moved the ceiling
  by noise.
- **ADR-047** removes the intent-fan leg (12.0 ms) but does **not**
  address `composition_record` (13.5 ms). The two sit on the same
  per-write critical path. This is structurally the same risk that
  killed W6.
- **The "65× WAIT, not WORK" framing** (blocking occupancy 2.6 ms/op
  vs. ~40 µs CPU) is double-edged. It says the ceiling is movable
  (nothing is CPU-bound at 40 µs). It also says the next bottleneck is
  *queueing*, which is exactly the class of problem that hides behind
  the first serialisation you remove — you don't see the second until
  the first is gone.

**Mitigation.** Step 3 (measure `pif.*` decomposed, not `pif.total`)
and step 4 (attack the second binder in the same window) are
non-negotiable if ADR-047 is to avoid the W6/#267 pattern. The 2026-06-10
build not exporting `pif.*` sub-spans is the single biggest reason the
next pass is a profile pass, not an implementation pass.

## 8. What this implies for the workflow

This is the second time in the write-path thread that an internally-reviewed design (ADR-046 approved → built → falsified; ADR-047 approved → adversary found it rests on non-existent code) has been shown to diverge from the actual tree. The diamond's auditor role checks spec↔code depth (STUB/SHALLOW/MOCK/NETWORK); it does not check architect↔code "reused / already exists" claims. Two cheap additions would have caught ADR-047's gap at gate 1 instead of gate 2:

1. **An architect "reused" checklist** — every `reused` / `already exists` / `extends existing` claim in a design doc must cite a `file:line` and a `grep` proving a production caller exists. Finding C (recovery never called in production) and Finding E (idempotency index does not exist) would have failed this check immediately.
2. **An adversary "load-bearing fact" probe** — every claim a design's correctness argument *depends on* (not merely asserts) must be verified against the code, not the spec. Finding A (compaction is not the LWW resolver for named objects) is the load-bearing fact in ADR-047 §1/§2, and it is false against `composition_hash_key` / `create_with_name` / `name_insert`.

Neither is in the current `.claude/roles/` definitions. Both are
captured here, not in a workflow doc, because the user asked for the
analysis in specs and the workflow doc is out of scope for this
escalation.

---

## TL;DR

- **Yes, half-of-read is inside the architecture's envelope** (148k vs.
  a 100–200k audited burst budget).
- **No, it is not "lift writes with optimisations."** It is: close
  ADR-047's four Critical + three High design gaps (the design is
  currently not buildable as specified), implement LeaderSink, AND in
  the same window attack the second binder (`composition_record`,
  13.5 ms) — otherwise ADR-047 repeats W6's "removed a parallel branch
  of equal length" failure.
- **The historical pattern is the dominant risk.** Four of the last
  four write-path levers were falsified by measurement. The next
  measurement MUST decompose `pif.*` (the 2026-06-10 build did not),
  or the result will be the same "the wall moved, we don't know why"
  shape as #267.
- **Two sustained-100k SM ADRs** (disk-backed `cluster_chunk_state`,
  off-mutex incremental snapshots) **plus #261 backpressure** are
  required to hold 148k sustained. 200k+ needs the ingress batch-quorum
  protocol change.
- **A quarter is plausible if the falsification rate holds at the
  historical average.** Aggressive if it does not. The first signal
  will be the post-ADR-047 profile pass: if `composition_record` did
  not move in the same window, stop and fix it before claiming
  half-of-read.
