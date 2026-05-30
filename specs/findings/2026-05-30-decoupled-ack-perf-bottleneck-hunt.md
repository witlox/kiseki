# 2026-05-30 — Decoupled-ack perf bottleneck hunt

The ADR-047 decoupled-ack rewrite landed (commit history on `main` from `8ef8b4e`
onward). The GCP A/B run on 2026-05-30 measured:

| | baseline (sync) | decoupled-ack | Δ |
|---|---:|---:|---:|
| native put-heavy | 631 op/s | 712 op/s | **+12.8 %** |
| native mixed | 783 op/s | 820 op/s | +4.7 % |
| S3 put-heavy | 1 004 op/s | 998 op/s | −0.6 % (noise) |
| S3 mixed | 1 297 op/s | 1 411 op/s | +8.8 % |
| native get-heavy | 28 201 op/s | 28 698 op/s | +1.8 % (noise; reads unchanged ✓) |

Single-digit gains, not the multi-× we predicted. **The assumption that the
cross-node Raft round is the dominant write cost was wrong**; we built the
rewrite on that assumption, shipped it, and the data refuted it. This document
is the post-mortem methodology fix: stop guessing, profile the write path.

## What we know so far (data, not theory)

- 1-client × 16-concurrency native PUT = **631 op/s = 1.6 ms per write**.
- 1-client × 16-concurrency native GET = **28 k op/s = 36 µs per read**.
- Ratio: writes are **~45× slower than reads**. Removing one Raft round on
  a GCP LAN (~1 ms RTT, ~200 µs apply) shaved at most ~200 µs from the 1.6 ms.
- We added: per-shard supervisor thread, perspective-seq mint, fan-includes-leader
  RPC, durable IntentStore fsync, in-SM dedup-set apply, per-intent prune.
  Cumulative cost is ~80 µs of recovered headroom out of 1.6 ms = where we sit.

The remaining 1.4 ms per write is unaccounted for. It is what we have to find.

## Hypothesis priors (informed, unverified — to be checked, not assumed)

In rough order of suspicion based on what we know about the code:

1. **Chunk fan-out RPC to `min_acks=2` peers** is fully synchronous before the
   delta is even emitted. Two parallel RPCs ≈ one RTT = ~1 ms on GCP LAN. This
   is almost certainly the floor we are sitting on.
2. **AES-256-GCM encrypt of the 64 KiB payload.** SIMD-accelerated but still
   100–500 µs per write at this size.
3. **Composition store lock contention** on the gateway's `InMemoryGateway`
   composition map under 16-concurrency.
4. **fjall / redb WAL appends.** Should be batched per the 100 ms
   `KISEKI_*_FLUSH_INTERVAL_MS` settings, but worth verifying — a missed
   batch boundary or a sync write that bypasses the buffer is invisible
   without a flame graph.
5. **Per-write postcard / prost encode** on the chunk fragment RPC and
   the IntentSync put RPC. Small individually, may compound.

These are **priors**, not conclusions. The point of the work below is to
either confirm or kill each one with measured data.

## Methodology

The discipline going forward: **no more "this should be faster"; only "we
measured X, attacked Y, measured again, it moved by Z."** No optimization
ships without a before/after flame graph and a hot-frame named in the commit
message.

### Step 1 — Extend `kiseki-profile` to support multi-node local

The current driver (`crates/kiseki-profile/src/harness.rs`) spawns one
`kiseki-server`. The BDD `ClusterHarness`
(`crates/kiseki-acceptance/tests/steps/cluster_harness.rs`) already spawns a
3- or 6-node real cluster (release binary, multiplexed Raft transport).

Lift the cluster-spawn pattern into `kiseki-profile` as a new `harness::Cluster`
type. Keep the existing single-node `ProfileServer` — the multi-node mode is
opt-in via `--nodes N` on `kiseki-profile run`. With `KISEKI_PPROF_OUT` set,
each spawned server writes its own flamegraph SVG (the per-server env
passthrough already exists, just needs per-node `OUT.{node}.svg` naming).

Acceptance criterion: `kiseki-profile run --protocol native --shape put-heavy
--nodes 3 --duration-secs 30 --pprof OUT/` produces three SVG flamegraphs
(one per server) plus the throughput / latency line we already emit.

### Step 2 — Profile the write path on multi-node local

Run the put-heavy load against the 3-node setup and capture, for both the
leader and one follower:

- A CPU flamegraph (pprof).
- A heap / allocation profile (dhat).
- The Prometheus metrics snapshot at /metrics (already wired) for any
  per-component timing we already record.

Capture them with the rewrite (which is already on main). We are no longer
A/B testing decoupled vs sync — that question is settled. We are asking:
**what does the write path actually spend its 1.4 ms on?**

### Step 3 — Read the flame graphs against the priors

For each prior in §"Hypothesis priors", look at the leader's flamegraph and
the follower's, and **either find the prior in the hot frames or remove it
from the list**. Output: a ranked list of hot frames, % of CPU time each,
mapped to the code path. Where the leader and follower differ tells us
about the asymmetry of the fan-out path.

### Step 4 — Validate the top hot frame with an experiment

Drop, mock out, or batch the suspected dominant cost (one at a time) and
re-measure. Examples per prior:

1. *Chunk fan-out*: temporarily bypass the fan and write only the local
   fragment. If `put-heavy` jumps 3–5×, the fan is the floor.
2. *Crypto*: temporarily use a 1-byte payload (skip the encrypt of the
   bulk data). If the per-op cost drops sharply, crypto is the floor.
3. *Lock contention*: drop concurrency to 1 and divide observed throughput
   by 16. If concurrent-16 is meaningfully lower than 16 × single-thread,
   there is contention.
4. *WAL fsync*: raise `KISEKI_*_FLUSH_INTERVAL_MS` to 1 s and re-measure.
   If throughput jumps, fsync is in the hot path.
5. *Serialization*: replace postcard / prost encode with a no-op stub on
   the fragment RPC and re-measure.

Each experiment is local (no GCP), takes < 5 min, and either confirms
or refutes a single hypothesis. **One signal at a time.**

### Step 5 — Build the next optimization on the measurement

Whatever survives step 4 is the next thing to attack. Write a fresh ADR
(or extend the perf roadmap) **only after** the hot frame is named and
the experiment shows the predicted improvement is achievable. The
deliverable is a flame graph showing the before, a description of the
fix, a flame graph showing the after, and a throughput number that moved.

## What this fixes about how we've been working

The ADR-047 rewrite was a 3-week effort built on an unverified assumption
about the bottleneck. We had `kiseki-profile` with CPU / heap profiling
support the whole time and never pointed it at the multi-node write path.
We had the BDD `ClusterHarness` for multi-node and never used it to
profile, only to functionally test. That is the methodology hole.

The fix is to make **multi-node profiling** a routine local capability
(Step 1) and to require **flame-graph evidence** as the gate on every
perf claim (Step 4 / Step 5). The work is grouped under the existing
[`docs/performance/roadmap.md`](../../docs/performance/roadmap.md)
priorities; this finding feeds back into how we pick the next item there.
