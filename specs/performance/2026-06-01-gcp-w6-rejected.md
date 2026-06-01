# 2026-06-01 — GCP Pass 4: W6 fsync coalescer measured + REJECTED at production parameters

## Status: W6 implementation correct, hypothesis **wrong**, code stays in
(env-var gated, off by default) for the rare workload where it might
help, but **W6 is not the multi-protocol perf lever the roadmap claimed**.

## Cluster
6 × c3-standard-22-lssd + 3 × c3-standard-22 clients, europe-west1-b,
EC-4+2, 6-shard distributed-leader namespace.
- Profile binary: SHA `72ec358a8aad7b16e8dace87ba1ef37a1548f3b96cd7d5afeb5c53c7de909a42`
  (\`hot-path-trace\` + \`pprof\`, post-#152)
- W6 env: `KISEKI_RAFT_FSYNC_WINDOW_US=500` + `KISEKI_RAFT_FSYNC_BATCH=32`
- W6 enable verified at boot via `INFO ... FjallRaftLogStore: fsync coalescing ON (#151 / W6) ... window_us=500 max_batch=32`
  for every shard on every node.

## Benches

**A. W6 OFF — 1 client × 16-conc → node-1, 60 s** (baseline)
- 725 op/s · 45.3 MiB/s · **p50 3.95 ms · p99 108 ms** · 2 errors
- Reproduces Pass 3 Bench A (730 op/s · p50 3.8 ms · p99 107 ms) within 1 %

**B. W6 ON — 1 client × 16-conc → node-1, 60 s** (the headline measurement)
- 725 op/s · 45.3 MiB/s · **p50 3.89 ms · p99 93 ms** · 1 error
- vs A: throughput 0 %, p50 −1.5 %, p99 **−14 % (tail only)**

**C. W6 ON — 3 clients × 16-conc → distinct leaders, 60 s** (aggregate)
- c1=1223 op/s (p99 80.5 ms) · c2=759 (p99 108 ms) · c3=760 (p99 107 ms)
- **Aggregate 2741 op/s · 171 MiB/s**
- vs Pass 3 Bench B (W6 OFF, 2710 op/s aggregate): **+1 % (noise)**

## Put-phase decomposition (mean μs per write)

| phase | A (W6 OFF) | B (W6 ON) | C (W6 ON, 3 clients) |
|---|---:|---:|---:|
| composition_record | 12 406 | 12 407 | 8 256 |
| raft_commit        | 12 169 | 12 226 | 8 125 |
| chunk_write        | 12 168 | 12 225 | 8 124 |
| encrypt            | 31     | 31     | 32     |

`composition_record` / `raft_commit` / `chunk_write` all measure from
start of `write_impl` to the moment both parallel fans complete
(`tokio::try_join!`). They're equal because they all stop at the same
moment.

**A vs B is +1 µs (noise).** The aggregate-load run (C) drops to ~8 ms
mean for the same reason Pass 3 saw it: openraft batching amortises
the AppendEntries round across more entries. **W6 does not add or
subtract.**

## Off-CPU profile (sched:sched_switch, 30 s window, node-1)

Sample counts during equivalent workloads:
| | Samples | Δ |
|---|---:|---:|
| Pass 1 (~1 client baseline, no W6) | 1.7 M | — |
| Pass 4 Bench B (1 client, W6 ON)   | 1.0 M | −41 % |

W6 **does reduce context switching** — confirming the fsync wait was
real, and the coalescer compresses it. But the per-write wall time
shows no improvement because:

> **The fjall WAL fsync sits in parallel with `chunks.write_chunk` and
> the AppendEntries replication RTT inside the `tokio::try_join!`
> fan-out in `mem_gateway::write_impl`.** Reducing one parallel branch
> by ~3 ms does nothing when another parallel branch (chunk-store
> fsync + EC fan, or the RTT to a slow follower) is the same length.

The Pass 1 "12.2 % committer-in-D" finding was real but **off-critical
path** — those D-state intervals overlap with other work the gateway
is doing on the same write.

## Verdict

| | Predicted | Measured | Verdict |
|---|---|---|---|
| per-write fsync cost | 3 ms → 0.3 ms | 12.4 ms → 12.4 ms (unchanged on critical path) | ❌ wrong |
| server-side floor | 8 ms → 3 ms | 8.1 ms → 8.2 ms (3-client) | ❌ wrong |
| p50 | 4 ms → 2 ms | 3.95 ms → 3.89 ms | ❌ wrong |
| p99 | 75 ms → 10-15 ms | 108 ms → 93 ms (1-client) | ⚠️ small tail improvement only |
| sched-switches | drop ~50 % | 1.7M → 1.0M (~40 %) | ✅ as predicted |

**W6 reduces the wait it was designed to reduce — but the wait was not
on the critical path.** Pass 1 misled us into thinking it was.

## What the real lever is

Looking at the put-phase decomposition: **the 12 ms (1 client) / 8 ms
(3 clients) mean is the parallel-fan max(chunk_write, raft_commit)**.
Both fans end at the same moment because `try_join!` waits for both.
Whichever fan is slowest sets the floor.

To drop the floor we need to know which fan dominates. The hot-path
spans `gw.put_intent_and_fan_call` and the per-chunk
`chunks.write_chunk` time would tell us. We did not capture those in
Pass 4 (the profile binary has the timers but we only snapshotted the
`put_phase` histogram).

**Next diagnostic (~$3, 20 min):** Pass 5 — same binary, same env,
single client, snapshot the `kiseki_gateway_hot_path_*` histograms
before/after. That separates `chunk_write` (chunk-store fan) from
`raft_commit` (intent fan). Whichever has the higher mean is the real
target.

Three plausible outcomes for Pass 5:
- **chunk_write dominates** → fix is in `kiseki-chunk` (per-chunk
  fsync, EC fan latency); attack the chunk-store fsync analogue of
  W6, or skip EC for inline-tier sizes
- **intent fan dominates** → openraft replication RTT is the floor;
  network tuning, h2 flow-control, or per-shard transport batching
- **They're balanced (within ~1 ms)** → both need work; W6 deserves
  re-test after one of them moves

## Honest framing

The 2026-06-01 roadmap rewrite claimed W6 would give 5-8× lift. **That
was wrong.** The Pass 1 + Pass 3 data was correctly identifying the
WAIT but not the CRITICAL PATH. Off-CPU profiling shows what's waiting,
not what's pacing wall time. We needed an on-CPU + per-fan timing
breakdown to distinguish, and we now have it as a known next step.

## Cost
~30 min wall (apply + bench A + restart + apply v2 + bench B + bench C +
destroy) ≈ **~$4**. Total day-of: ~$17.

## Code disposition

PR #152 stays merged. The coalescer is correct (7 unit tests pass) and
the env-var gate keeps it off by default. Two reasons to keep it
landed:

1. **Useful for the rare workload where Raft-log fsync IS the
   critical path** (high-write single-shard, small object, no EC fan,
   no replication). That's a niche, but it does exist.
2. **Removing it is more risk than leaving it gated off.** The
   correctness contract is preserved by `fsync_barrier()`'s
   tri-state — no behavior change when env vars unset.

But **§W6 in `docs/performance/roadmap.md` needs the predicted-vs-
measured table prepended** and the "8 → 3 ms" predictions deleted.
That's the next commit on this branch / a separate doc PR.
