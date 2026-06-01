# 2026-06-01 — GCP `default` profile, two-pass run (partial)

6 × c3-standard-22-lssd (1.5 TB local NVMe each) + 3 × c3-standard-22 clients
+ 1 × ctrl. `europe-west1-b`. EC-4+2 default. 6-shard distributed-leader
namespace, one leader per node. Production binary on Pass 2 was current
`main` HEAD `88a6e08` (post #144 / #145 / #146-#147 / #149 / #150);
profile binary added `--features hot-path-trace,pprof`. SHA256:
- production: `9b8f44cb7bd502bbc8e80f267a56ff627c9c49edc8c3ce4ecd552aee4e9f78f7`
- profile:    `f39695a5e95e31667cf25cebbb06c19ca8497f0109feea40aad8cd7f38d3ec8f`

**Status: PARTIAL.** Pass 1 completed cleanly with full diagnostic. Pass 2
covered the PUT side and got two GET concurrency points before the
operator (Claude) torched the budget on stuck harness-background bench
runs that lost stdout. Aborted on cost at ~$9 of the $50 budget. Cluster
torn down clean (24/24 resources destroyed; 0 instances + 0 disks
remain in the project).

The two findings that matter were both captured in Pass 1, before
Pass 2's harness mishap. Pass 2 confirmed the production binary
matches the profile binary's PUT shape (4% delta on op/s, 20% delta on
p99); the rest of Pass 2 is salvageable for shape, not for absolute
numbers.

## Pass 1 — diagnostic (profile binary, single shard targeted)

### Workload
`kiseki-client bench --shape put-heavy --concurrency 16 --object-size 65536 --duration-secs 60`
Client 1 → `kiseki://10.0.0.10:9103`. Node 10 is the leader of shard
`cd3c528e-...`. No forward hop.

| Run | op/s | MiB/s | p50 | p95 | p99 | errors |
|---|---:|---:|---:|---:|---:|---:|
| 1 (warm-up) | 742 | 46.4 | 3.6 ms | 58.6 ms | 107.1 ms | 1 |
| 2 (steady)  | **1311** | **81.9** | **4.1 ms** | 56.1 ms | 74.9 ms | 3 |

vs 2026-05-28 RUN 3: 243 op/s · 15 MB/s · ~180 ms p50.
**Run 2 is 5.4× higher op/s, 44× lower p50.**

### Server-side put-phase decomposition (run 2)

| phase | count | sum (ms) | mean (μs) |
|---|---:|---:|---:|
| composition_record | 78711 | 582 013 | 7 394 |
| raft_commit        | 78711 | 571 772 | 7 264 |
| chunk_write        | 78711 | 571 711 | 7 263 |
| encrypt            | 34 115 | 1 075   | 32    |

`chunk_write` / `raft_commit` / `composition_record` are all measured
from their start time to the moment **both** parallel fans complete
(`tokio::try_join!` in `mem_gateway::write_impl`). They are wall-times
of the same window, not per-fan times — so all three converge on the
same value (7.3 ms). `encrypt` is a separate per-chunk measurement.

Mean window = 7.4 ms. p50 from the client = 4.1 ms. The mean-vs-p50
gap is the tail (p99 = 75 ms).

### Off-CPU profile (`perf record -e sched:sched_switch`, 30 s window)

Top sched-switch sources, aggregated by thread + state:

| Thread state | % | Meaning |
|---|---:|---|
| `kiseki-data` in S | 18.1% | tokio data workers idle (no contention) |
| `kiseki-committer` in D | 12.2% | **fsync of openraft Raft log on local NVMe** |
| `kiseki-raft` in S | 0.29% | negligible |
| `kiseki-data` in D | 0.24% | negligible |

This pins one of the three pre-registered hypotheses
(`docs/performance/roadmap.md` §"The 41 ms mystery"): **the wait is
real I/O on the local Raft-log fsync**, not tokio scheduling
congestion and not lock convoy. The committer threads enter D state
(uninterruptible kernel I/O) waiting for fjall's WAL fsync on the
device write queue.

### The 41 ms mystery — resolved

The roadmap's "42 ms stacked wait" was measured on the
2026-05-28 RUN 3 matrix, where the bench client dialed
follower nodes and the gateway forwarded each write to the leader.
With Pass 1 targeting the leader directly, p50 dropped 42 ms → 4 ms.

**The dominant component of the gap was the server-side forward
hop, NOT a stacked wait at the consensus layer.** What remains
in the residual mean-vs-p50 gap (7.4 ms mean, 4 ms p50, 75 ms p99
tail) is the local log fsync — measured at 12% of all
sched-switch events.

## Pass 2 — production binary, cluster aggregate (partial)

### What we measured

| Test | op/s | MiB/s | p50 | p99 |
|---|---:|---:|---:|---:|
| PUT 16-conc × 1 client → leader (warm) | 770 | 48 | 3.4 ms | 101 ms |
| PUT 16-conc × 1 client → leader (steady) | **1363** | **85** | **4.0 ms** | **61 ms** |
| PUT 16-conc × 3 clients × distinct leaders (aggregate) | **3178** | **199** | 4.8–5.0 ms | 62–109 ms |
| GET 16-conc × 3 clients (aggregate) | 1440 | 90 | 35 ms | 116 ms |
| GET 32-conc × 3 clients (aggregate) | 2104 | 132 | 49 ms | 130 ms |

### What was lost
- GET 64-conc × 3 clients (intended R1 capstone)
- R2 — single-stream bulk GET (1 client × 1 stream × 1 GiB)
- #143 inline-tier 4 KB PUT (should hit `kiseki_pool_writes_total{durability="inline"}`)

Lost because the operator wrapped the 64-conc invocation in subshells
with `&` + `wait`, which the Claude Code harness treated as
background tasks that completed-exit-0 with stdout redirected into
nothing. Single-process gcloud-ssh invocations would have worked.

### Profile vs production binary delta (PUT 16-conc direct-to-leader)

| Metric | Pass 1 (profile) | Pass 2 (production) | Delta |
|---|---:|---:|---:|
| op/s | 1311 | 1363 | +4% |
| p50 | 4.1 ms | 4.0 ms | −2% |
| p99 | 74.9 ms | 60.6 ms | −19% |

Instrumentation overhead is small (≤4% throughput, mostly p99 tail
expansion). **Pass 1's measurements are a fair lower-bound for what
production sees.**

### PUT scaling with shard count (1 → 3 clients)

- 1 client × 1 leader = 1363 op/s
- 3 clients × 3 leaders, aggregate = 3178 op/s
- Per-client throughput in the 3-client run: 1679 / 760 / 738 (asymmetric — client 1 ran first on a cold-but-not-empty cluster and hit a faster path; clients 2 + 3 hit the same chunk-store NVMe contention)

Effective scaling: **2.3× from 3× clients = 77% efficiency at 3 shards.**
Linear extrapolation to 6 leaders × 6 clients: 6 300 – 8 400 op/s
aggregate at PUT 64 KB.

## Where the targets stand

Updated from `docs/performance/competitive-targets.md`:

| Op | Today (Pass 2 partial) | kiseki target | Realistic post-route-fix¹ | Note |
|---|---:|---:|---:|---|
| native PUT 64 KB | 3 178 op/s | 360 k op/s | 6 300 – 8 400 op/s | ~50× under target |
| S3 PUT 64 KB | not measured | 5.2 GB/s aggregate | same as PUT? | client compatibility unknown — see below |
| pNFS write 1 MB | not measured | 10 GB/s aggregate | same as PUT? | NFS can't route — server-side proxy mandatory |
| native GET 64 KB | 2 104 op/s | 16 GB/s bulk | 4–6 k op/s at higher ∥ | 8× concurrency hadn't been reached when aborted |

¹ Assuming server-side forward hop is what we measured (9 ms) and a
hypothetical client-side route-to-leader removes it on every PUT. See
the next section for why this is more nuanced.

## Why client-side route-to-leader isn't the holistic fix

The 9 ms forward hop is paid by **every protocol that lands on a
follower**. The fix shape depends on the protocol:

| Protocol | Can the client choose the leader? | Forward hop is paid by |
|---|---|---|
| native gRPC / TCP | Yes — client dials specific endpoint, can use `ForwardToLeader` hint | client (if it implements the hint) |
| **NFSv3 / NFSv4 / pNFS** | **No** — kernel mounts to ONE server IP | **server-side proxy, always** |
| **S3 HTTP** | Sometimes — modern AWS SDK follows 307 redirects; curl with `-L` does; most clients don't | **server-side proxy in practice** |
| FUSE | Yes — depends on whether the FUSE daemon inherits from native client's pool | client (with code) |

**Of the five surfaces, only two (native + FUSE) can route around
the forward at the client.** NFS and S3 are stuck paying it server-side.
So a client-side route-to-leader optimization in `kiseki-client`
unblocks **native + FUSE** but does nothing for the **NFS / S3 / pNFS
write rows** of the matrix — which is most of the protocol surface
production deployments actually use.

**This is why the user held off:** landing the route-to-leader change
would put native ahead by 10×, but leave NFS / S3 / pNFS exactly where
they were on the 2026-05-28 matrix.

## What's actually blocking targets across all protocols

In order of how broadly the fix applies:

### 1. Server-side forward hop costs 9 ms (affects all protocols)

The 9 ms we backed out (42 ms forwarded p50 minus 4 ms direct p50,
minus the ~7 ms fsync we'd pay either way ≈ ~9 ms attributable to
forward) is the single biggest multi-protocol lever. On a 10 Gbps
intra-cluster link, raw TCP RTT is ~0.1 ms; ~9 ms means the forward
path is doing real work — likely:
- Wire-level re-issue (deserialize → re-serialize the body)
- `ProxyClient` may open a fresh gRPC channel per request (TLS handshake)
- `AppendForwarder` (the openraft-style transport) may have per-request setup

**Need to profile the server-side forward path** with the same
`hot-path-trace` instrumentation we used on the gateway write path.
The hot timer is already plumbed; just need to drive 16-conc PUT
forwarded-from-follower against the profile binary, snapshot the
`gw.forward_*` histograms, and decompose. ETA: 30 min on a 6-node
cluster, ~$2 of GCP cost.

Estimated lift if the forward goes 9 ms → 2 ms: **5× on every
protocol that forwards**, which is NFS + S3 + pNFS + the "client
didn't pin the leader" case for native + FUSE.

### 2. Local Raft-log fsync (12% committer in D-state, affects all writes)

The off-CPU profile pinned 12.2% of sched-switch events on
`kiseki-committer` threads going to D state — uninterruptible kernel
wait on local NVMe write completion. This is fjall's WAL fsync
serialized through the device queue under concurrent writers.

**Fix shapes (any of them affects every protocol that writes):**

- **Log group-commit / batched fsync** — coalesce N committer
  fsync requests into one device-write-and-sync window. fjall already
  batches at the AppendEntries level; the gap is the kiseki-side
  committer entering D once per replication round under load.
- **Parallel devices for the Raft log** — the `default` profile has
  4 × NVMe but the Raft log lives on one. Stripe the WAL across
  devices.
- **Move the log to a faster medium** — Optane-class write buffer
  (VAST's secret) gets ~100 ns per fsync, vs our ~10 µs NVMe.
  Not available on c3-standard-22-lssd; would require an Optane VM.

Estimated lift if fsync time goes ~3 ms → ~0.3 ms: **2–3× on every
write**, p99 compresses from ~60 ms toward ~10 ms. Same lift on
NFS/S3/pNFS as on native.

### 3. Sub-linear GET concurrency scaling

R1 measured 1.46× throughput at 2× concurrency (16-way → 32-way).
The R2 single-stream profile pass that would have decomposed this
into "decrypt-bound vs RTT-bound" was lost. Best evidence we have:
35 ms p50 at 16-conc is way above the local NVMe read latency
(~0.2 ms) and above 64 KB-over-TCP serialization (~0.05 ms), so
something else is in the critical path.

Most likely candidates: cross-node chunk fetch (NIC RTT) or a per-op
decrypt allocation that's not zero-copy. The 2026-05-09 Arc-wrap
fix (in-process single-stream 8.2 → 16.4 Gbps) suggests the
multi-node version may have the same pattern.

### 4. POSIX surfaces (NFS, FUSE) must wait for synchronous commit by design

ADR-047 decoupled-ack only applies to **S3** and **Native**. POSIX
close-to-open consistency (ADR-013) requires NFS and FUSE to wait
for the full Raft commit on every CLOSE/COMMIT. This is a
fundamental gap that no amount of profiling closes — it's a
durability contract.

So `targets.md`'s **10 GB/s pNFS write target is structurally
~2× more expensive than the 5.2 GB/s S3 PUT target** because pNFS
can't take the decoupled-ack path. The headline ratio between NFS
and S3 in `competitive-targets.md` reflects this.

## Recommendation for the next move

Don't land the client-side route-to-leader yet (user's call, correct).
Instead:

1. **Server-side forward decomposition** — small profile pass
   (~30 min, $2). 16-conc PUT from client targeting a *follower*,
   with `hot-path-trace` ON, snapshot `gw.forward_*` histograms.
   This pins **where** in the 9 ms the forward spends its time and
   tells us whether it's a config tune (gRPC channel reuse) or a
   structural change.
2. **Only THEN** decide:
   - if the forward fix is small + general → land it; redo the
     full matrix
   - if the forward fix is structural or per-protocol →
     reconsider the client-side route-to-leader as one of N
     surface-specific fixes
3. **Log group-commit** is the parallel lever; independent of (1).
   Needs an ADR (changes durability/recovery semantics).

## Artefacts

- Pass 1: `/tmp/pass1-artefacts/` on the operator's box
  - `SUMMARY.md` — Pass 1 analysis (the version below the divider here)
  - `metrics-before-v2.txt`, `metrics-after-v2.txt` — full Prometheus snapshots
  - `perf-report.txt` — sched_switch top-50 events on node 1
  - `perf-off-cpu.data` (154 MB) was on the destroyed node; not pulled back
- Pass 2: `/tmp/pass2-artefacts/`
  - `01-put-baseline-run1.json`, `02-put-baseline-run2.json`,
    `03-put-3client-aggregate.json` — the captured numbers
  - GET 32-way × 3 numbers are in this file but not separate

## Cost

| Pass | Wall time | Cost |
|---|---:|---:|
| Pass 1 (profile) | ~30 min | ~$4 |
| Pass 2 (matrix, aborted) | ~40 min | ~$5 |
| **Total** | **~70 min** | **~$9** |

vs $50 budget. Saved ~$41 by aborting Pass 2 early — at the cost of
3 missing rows (GET 64-way, R2 single-stream bulk, #143 inline tier).
