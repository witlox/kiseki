# 2026-05-03 — Local single-node post-fix baseline

**HEAD:** Pre-fjall-sweep May matrix baseline (the post-perf-fix-sweep state); referenced by later snapshots as "2026-05-03 baseline".
**Hardware:** dev workstation (Linux, x86_64, 16 cores).
**Driver:** `kiseki-profile` matrix, single-node `kiseki-server` (ephemeral ports). 5 protocols × 3 workload shapes. Object size 64 KiB, c=16 (matches NFS connection-pool default cap), 30 s per scenario, warmup=256 for get-heavy / mixed.
**What changed since previous snapshot:** 5 perf fixes landed in late April / early May — see "May fix sweep" below.

## May fix sweep

| Commit | Change | Local matrix impact |
|---|---|---|
| `b0f048d` | server: single-node MDS advertises local DS uaddr | pNFS GET 0 op/s · 3528 errors → 62 op/s · 0 errors |
| `56ec297` | client/nfs: `tokio::sync::Mutex` on session — std mutex starved tokio runtime under concurrency | NFSv4 c=16 read p99: 30 s → 667 ms |
| `e058ded` | client+gateway: TCP_NODELAY on NFS RpcTransport + pNFS DS listener | NFSv4 c=1 GET: 24 op/s · 41 ms → 9285 op/s · 199 µs |
| `eebc7f0` | profile harness: tokio mutex on FuseDriver + pNFS session pool *(harness-only)* | n/a — measurement fix |
| `59cab58` | client/nfs: connection pool — N parallel sessions per Nfs3/Nfs4Client | NFSv4 c=16 GET: 9 k → 27 k op/s |

Each commit references the metric it was driven by; the matrix below is the post-fix snapshot.

## Throughput (c=16, 64 KiB)

| Protocol | put-heavy | get-heavy | mixed (70 P / 30 G) |
|---|---:|---:|---:|
| **S3 (HTTP)** | 7124 op/s · 445 MiB/s | **25 843 op/s · 1.6 GiB/s** | 8470 op/s · 529 MiB/s |
| **NFSv3** | 2042 op/s · 128 MiB/s | 26 615 op/s · 1.6 GiB/s | 778 op/s · 49 MiB/s |
| **NFSv4.1** | 8327 op/s · 520 MiB/s | **27 291 op/s · 1.7 GiB/s** | 808 op/s · 50 MiB/s |
| **pNFS Flex Files** | 8327 op/s · 520 MiB/s | 16 549 op/s · 1.0 GiB/s | 2254 op/s · 141 MiB/s |
| **FUSE** | 2790 op/s · 174 MiB/s | 10 789 op/s · 674 MiB/s | 3375 op/s · 211 MiB/s |

## Tail latency (p99 µs, c=16)

| Protocol | put-heavy | get-heavy | mixed |
|---|---:|---:|---:|
| S3 | 3 297 | 6 205 | 3 102 |
| NFSv3 | 11 277 | 4 038 | 49 157 |
| NFSv4.1 | 10 528 | 4 234 | 46 076 |
| pNFS | 10 540 | 21 116 | 23 493 |
| FUSE | 159 613* | 134 | 126 747* |

\*FUSE put p99 tail (160 ms) is the next investigation target. p50 is 0.35 ms; the bimodal distribution suggests batched composition flush or redb checkpoint contention. Not blocking — the median is fast.

## Total trajectory across the May fix sweep

| | starting matrix | after the 5 fixes | gain |
|---|---:|---:|---:|
| NFSv3 GET (c=16) | 12 op/s · p99 31 s | 26 615 op/s · p99 4 ms | **2 220×** throughput / 7 700× p99 |
| NFSv4.1 GET (c=16) | 24 op/s · p99 30 s | 27 291 op/s · p99 4 ms | **1 137×** / 7 100× |
| pNFS GET (c=16) | **0 op/s · 100 % errors** | 16 549 op/s · p99 21 ms | broken → working |
| pNFS PUT (c=16) | 583 op/s · p99 553 ms | 8 327 op/s · p99 11 ms | 14× / 50× |
| S3 GET (c=16) | 4 580 op/s | 25 843 op/s | 5.6× |

Numbers are server-side ceiling on a single host. Multi-node ceilings (and EC) tracked in the GCP snapshots.

## Captured profiles

- `/tmp/kiseki-prof/cpu-{protocol}-{shape}.svg` — pprof flamegraphs
- `/tmp/kiseki-prof/heap-{protocol}-{shape}.json` — dhat heap

Hot stacks in the post-fix S3 PUT path (server side):

- 22 % SHA256 in `kiseki_crypto::chunk_id::derive_chunk_id`
- 17 % redb `name_insert` in `CompositionStore::bind_name`
- 13 % AEAD seal envelope
- 13 % Raft `append_delta`

These were the candidates for the next round of optimization — addressed in the 2026-05-07 fjall sweep and 2026-05-09 libfuse-swap snapshots.

## Cross-references

- Next snapshot: [2026-05-07 local matrix](2026-05-07-local-matrix.md) (post-fjall sweep, FUSE leapfrogs everything).
- GCP companion: [2026-05-03 GCP transport](2026-05-03-gcp-transport.md) (multi-node, surfaced fabric quorum-loss bug).
