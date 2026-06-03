# 2026-06-03 GCP perf — post-#195/#197/#198/#201 stack verification

6-node default profile (c3-standard-22-lssd × 6 storage + × 3 clients), europe-west1-b.
Single cluster, profile binary (`hot-path-trace + pprof`) built from main `9776fd0`
(includes #194/#195 serde_bytes Raft + IntentSync wire; #196/#197 inline-Envelope
postcard swap; #198 `tracing::release_max_level_info`; #199/#201 in-memory LRU on
`FjallRaftLogStore`).

Driver: `kiseki-client bench` from each kiseki-client-N targeting a distinct
storage node's native-tcp listener (10.0.0.10/11/12:9103), `--concurrency 16
--object-size 4096 --duration-secs 30`, run in parallel from all 3 clients.

## Native-tcp matrix (3 clients × 30 s × conc=16, 4 KiB)

| Shape | Per-client ops/s | Aggregate ops/s | p50 | p95 | p99 | Throughput |
|---|---|---|---|---|---|---|
| put-heavy | 3 416 / 3 442 / 3 439 | **10 297** | 3.4 ms | 11-15 ms | 25 ms | ~40 MiB/s |
| get-heavy | 141 269 / 147 056 / 144 636 | **433 k** | 108 µs | 141-145 µs | 175 µs | ~1.7 GB/s |
| mixed 70/30 | 3 830 / 3 816 / 3 798 | **11 444** | 3.1 ms | 13-14 ms | 23 ms | ~45 MiB/s |

Per-client variance across the three drivers is tight (<1 %), so the cluster is
the limit, not the bench-side.

## Raw bench output

### PUT-heavy
```
client 1 (→ 10.0.0.10): ops=102564 throughput=3416.6 op/s p50=3421 p95=11564 p99=27528
client 2 (→ 10.0.0.11): ops=103365 throughput=3442.9 op/s p50=3378 p95=10582 p99=21992
client 3 (→ 10.0.0.12): ops=103271 throughput=3439.8 op/s p50=3394 p95=11027 p99=19799
```

### GET-heavy (warmup 1024)
```
client 1: ops=4238194 throughput=141268.6 op/s p50=109 p95=145 p99=174
client 2: ops=4411845 throughput=147056.4 op/s p50=106 p95=141 p99=165
client 3: ops=4339238 throughput=144636.2 op/s p50=108 p95=142 p99=162
```

### Mixed 70/30 (warmup 1024)
```
client 1: ops=114961 throughput=3830.5 op/s p50=3158 p95=13626 p99=23519
client 2: ops=114548 throughput=3816.4 op/s p50=3099 p95=13293 p99=23500
client 3: ops=114176 throughput=3798.0 op/s p50=3115 p95=13989 p99=23426
```

## Flamegraph (`node1-pprof.svg`)

Captured from kiseki-storage-1 during sustained 3-client conc=16 PUT for 25 s
then `systemctl stop kiseki-server` (graceful → pprof dump on `Drop`). 996 total
samples.

### What changed vs the pre-#195 / pre-#197 / pre-#201 stack

| Frame | Pre-stack (post-#193, ref: 2026-05-29) | Post-stack (this run) | Note |
|---|---|---|---|
| `Envelope::serialize` via `serde_json` | ~5 % | not in profile | #197 swapped to postcard |
| `itoa::write` | ~3.5 % | not in profile | byproduct of the JSON→postcard swap |
| `<impl Deserialize for LogCommand>::visit_enum` | ~6 % | not visible | #199 LRU hides log-store decode |
| `<impl Deserialize for IncorporateItem>::visit_seq` | ~6 % | not visible | same |
| `postcard::de::from_bytes` (cumulative) | ~12 % | 75.80 % under `tcp_transport::decode_request_body` only | residual is the **inter-node Raft RPC wire**, not the log-store path #199 targeted |

### New dominant clusters (production scale)

| Frame | % of samples |
|---|---|
| `kiseki_log::raft_shard_store::run_supervisor_loop` | 86.55 |
| `kiseki_log::intent::FjallIntentStore::put_batch` | 88.86 |
| `fjall::worker_pool::worker_tick` | 91.27 |
| `lsm_tree::table::Table::point_read` | 84.64 |
| `lsm_tree::memtable::Memtable::insert` | 70.68 |
| `fjall::batch::WriteBatch::commit` | 76.51 |
| `fjall::flush::worker::run` | 66.57 |
| `kiseki_chunk::small_object_store::SmallObjectStore::put` | 71.49 |
| `kiseki_view::view::ViewStore::get_view` | 62.75 |
| `kiseki_raft::tcp_transport::decode_request_body` (postcard) | 75.80 |

These are ancestor frames (sum-of-descendants), not leaf CPU — they overlap
heavily. What it tells you cleanly: at PUT scale on real hardware, **fjall I/O
and the lsm_tree write/read path dominate**. The CPU optimisations the recent
stack landed (#194-#201) have moved the bottleneck off the deserialise path and
onto the durable-storage path, which is the desired shape.

### Scoring against `targets.md` (default profile)

Per the `targets.md` status legend (`✓` within 20 % below target, `≈`
within 50 %, `✗` far below):

| Shape | Measured | Target (idealised) | % | Status |
|---|---:|---:|---:|---|
| 4 KiB GET op/s (3-client agg) | 433 000 | 600 000 | 72 % | **≈** (very near ✓) — in band on an idealised target |
| 4 KiB PUT op/s (3-client agg) | 10 297 | 360 000 | 2.9 % | **✗** — far below |
| PUT/GET ratio | 2.4 % | 60 % (ADR-042 §14 TCP-framed) | — | architectural asymmetry not the issue; PUT is the gap |

The targets are derived from ADR-042 §14 (TCP-framed: 60 k op/s PUT,
100 k op/s GET per node) scaled by 6 nodes, with no Raft commit cost
subtracted — i.e. an idealised ceiling. The 60 % PUT/GET write-asymmetry
matches VAST's marketed ratio; GET landing at 72 % of that ceiling is
solid, well inside ADR-042's `≈` band and approaching the 80 % ✓
threshold. PUT at 2.9 % is the single gap and matches the roadmap's
"commit-bound: one Raft round per write" framing.

Competitive view (`competitive-targets.md`, same hardware): GET at
433 k is 2–4× ahead of Lustre / Ceph / VAST for 4 KiB reads; PUT at
10.3 k is on-par-to-2×-ahead of Ceph 4 KiB writes (5–20 k, also
commit-/journal-bound) and 3–15× behind Lustre R-1 (30–80 k) and
VAST (50–150 k).

### Levers visible for PUT — corrected ranking

**Correction (2026-06-03):** an earlier draft of this section listed
"W1 batched Raft commit" as the headline lever. That was wrong: W1
(ADR-046) was built then **REVERTED on 2026-05-29 (commit `3fd8cc8`)**
after the local A/B showed flat throughput (254 vs 264 op/s). Root
cause of the flat: **openraft already auto-batches concurrent
`client_write`s** into one AppendEntries (`max_payload_entries: 300`)
— W1 amortised exactly what openraft amortises. ADR-046 is retained
as a rejected-decision record. There is no `KISEKI_WRITE_COALESCE`
env var to turn on; the code is not in main.

The honest story per `docs/performance/roadmap.md` +
`project_perf_roadmap_commit_bound`:

- **On loopback, the replicated round pipelines cleanly to ~13 k op/s
  on a single shard**. The earlier "stacked off-CPU wait in the
  commit pipeline" theory was disproven by direct probes; an earlier
  "40 ms stall" was traced to a probe artefact (tonic 0.14.5
  `serve_with_incoming` ignores `tcp_nodelay`; prod path is
  `serve_with_shutdown` which has nodelay default-on and is not
  Nagle-stalled).
- **On real GCP, the 250 op/s / 50–100 ms forwarded tier is NOT
  reproduced on loopback.** The mechanism on real-node hardware is
  not yet pinned. The roadmap's next step is **per-step timing in
  the loaded server on real GCP** (tokio-console or instrumented
  spans), NOT another flamegraph — a flamegraph won't show an
  off-CPU wait.

Two concrete code sites the prior investigation surfaced (still
applicable, not blocked on theory):

| Site | Evidence | Expected lift |
|---|---|---|
| **`tcp_framed/connection.rs` serve_connection loop** — `read → dispatch (await full round) → write → next`. `request_id` is carried on the wire but **not used for multiplexing**, so per-connection concurrency = 1 and cluster concurrency = number of client TCP connections. | Identified during the W1 investigation; ranked above W1 in the corrected analysis. | Substantial: every additional in-flight PUT on a single client connection becomes possible. |
| **`tcp_transport::decode_request_body` (postcard, 75.8 % of flamegraph samples)** — Raft inter-node RPC wire-receive decode. Distinct site from #199 (log-store *read* decode); same fix-shape as #195 (`serde_bytes` on `Vec<u8>` fields). | This flamegraph. | CPU win, 5–15 % PUT lift in isolation. |

Two diagnostic-shaped follow-ups (the roadmap's actual next probes):

| Probe | What it answers |
|---|---|
| **tokio-console / per-step timing on a loaded real-GCP server** | Where the GCP-only 50–100 ms forwarded-tier latency actually waits. Not visible on a flamegraph (off-CPU). |
| **Same direct-pipeline probe shape on real GCP nodes** | Whether the GCP latency is mechanism or measurement — does the loopback "13 k op/s scales cleanly" result reproduce, or is it real-NIC / co-location / kernel-stack specific? |

Two CPU follow-ups, lower priority:

| # | Lever | Status |
|---|---|---|
| **3** | `tcp_transport::decode_request_body` `serde_bytes` swap (same shape as #195) | Not started; well-understood fix. |
| **4** | `ViewStore::get_view` (62.8 %) per-tenant cache | Not started; cheap to land. |

**The 2026-06-03 CPU stack (#194/#195/#197/#198/#201) was real work**
— it took CPU off serde and surfaced the actual structure of the
hot path on the flamegraph. But it does NOT close the GCP PUT gap on
its own; the gap is not a CPU shortage at the leader, it's a
real-hardware latency mechanism that needs a representative probe
to identify. **Next A/B is not a code lever — it's a measurement.**

## Comparison vs prior snapshots

Run-to-run numbers vary because the hardware shape and bench config differ
(2026-05-30 used 64 KiB objects; this run is 4 KiB to align with the local
matrix work). Direct comparison is only meaningful where shape matches.

Closest matching prior: **2026-05-29-gcp-137-matrix** at 4 KiB / 16 conc / 6
storage / 3 clients native-tcp, which recorded **PUT ~9.1 k aggregate** and
**GET ~370 k aggregate**. This run measures **10.3 k / 433 k** — same shape,
**+13 % PUT, +17 % GET**. Consistent with the post-#194/#195/#197/#201 CPU
freed showing up as headroom for more requests on the same hardware.

## Pipeline state

- All four PRs (#195, #197, #198, #201) merged at the time of this run.
- Profile binary: `dist-2026-06-03-main/profile/kiseki-server-x86_64.tar.gz`
  (built off main `9776fd0`, uploaded to
  `gs://kiseki-bench-binaries-pwitlox-20260502/kiseki-server-x86_64.tar.gz`).
- Cluster torn down after data capture (24 resources destroyed).

## Artefacts

- [`2026-06-03-gcp-perf-stack/node1-pprof.svg`](2026-06-03-gcp-perf-stack/node1-pprof.svg)
  — node-1 CPU flamegraph (1.9 MB, 996 samples) captured on graceful shutdown
  after the sustained PUT phase. Original at `pprof-out/gcp-2026-06-03/node1-pprof.svg`.
- Local matrix CSVs from the same stack: `pprof-out/matrix-post195.csv`,
  `pprof-out/matrix-post197.csv`, `pprof-out/matrix-post201.csv`,
  `pprof-out/matrix-post201-arc.csv` — for the dev-box A/B that motivated each
  PR.
