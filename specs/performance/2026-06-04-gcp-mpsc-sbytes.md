# GCP confirmation — mpsc-writer + serde_bytes(decode_request_body)

Cluster: 6 × c3-standard-22-lssd, 3 × c3-standard-22 (clients),
europe-west1-b. PR #210 head = 97d8b30.

Shape: 6-shard bench namespace (`kiseki-bench`, distributed leadership
— one leader per node verified via `kiseki-admin shards`), 3 clients
× conn=1 × conc=16 × 60s × 4 KiB PUT, three parallel processes
hitting three different nodes.

## Headline (per-client; total = ×3)

| Run | c1 op/s | c2 op/s | c3 op/s | total op/s | p50 | p99 |
|---|---:|---:|---:|---:|---:|---:|
| warm-3 | 2 490.6 | 2 518.0 | 2 536.0 | **7 544.6** | 3.3 ms | 74–113 ms |
| warm-4 | 2 516.3 | 2 500.1 | 2 506.7 | **7 523.1** | 3.2 ms | 110–148 ms |

## A/B vs prior baselines (same cluster shape, same client mix)

| Branch | total op/s warm-2 | p50 | Δ vs slice-4 | Δ vs prev |
|---|---:|---:|---:|---:|
| slice-4 (no mpsc, no serde_bytes) | 6 054 | 4.3 ms | — | — |
| mpsc-writer alone | 6 294 | 4.0 ms | +4.0 % | +4.0 % |
| **mpsc-writer + serde_bytes (this PR)** | **7 524** | **3.2 ms** | **+24.3 %** | **+19.5 %** |

The serde_bytes change alone is responsible for the +19.5 % delta
on top of mpsc-writer. The retracted lever ranking expected 5–15 %
PUT lift in isolation; the GCP measurement exceeds that band.

## Cross-check: microbench

Local criterion A/B on `decode_request_body` at production payload
sizes (4 KiB / 64 KiB / 1 MiB):

| Payload | vec_u8 (old) | byte_buf (new) | Speedup |
|---|---:|---:|---:|
| 4 KiB | 4.06 µs | 90.5 ns | **44.9×** |
| 64 KiB | 64.6 µs | 991 ns | **65.2×** |
| 1 MiB | 1.04 ms | 21.0 µs | **49.2×** |

The 75.8 %-of-dispatch-CPU flamegraph claim was real; the codec
became ~98 % cheaper at the payload sizes the receiver sees under
intent_put fan + AppendEntries replication.

## Cluster posture

- 0 errors across all four 60s runs (warm-1 / 2 / 3 / 4)
- 6 leaders distributed across 6 nodes (verified)
- chunk_fan_inner phase saturating sub-100 µs bucket — chunk fabric
  not the bottleneck at this load
- chunk_write phase: 84 / 471 408 entries under 1 ms — chunk storage
  IO healthy
