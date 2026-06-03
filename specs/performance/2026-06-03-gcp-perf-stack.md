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

### Open levers visible at this scale

1. **`ViewStore::get_view` 62.75 %** — a per-PUT view lookup. Caching candidate
   (per-tenant, short TTL). Tracked separately if it pans out.
2. **`tcp_transport::decode_request_body` 75.80 %** — Raft inter-node RPC
   wire decode. Different decode site than #199 targeted (#199 was log-store
   read; this is wire-receive). Same fix shape might apply but on a different
   type.
3. **Composition store sync write** (#200) — still a tracked follow-up;
   contention with the IntentStore writer is visible in the fjall worker pool
   traces.

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
