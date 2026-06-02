# 2026-06-02 — GCP A/B: L3 per-node cross-shard coalescer REGRESSED HARDER

## A/B summary

6 × c3-standard-22-lssd + 3 clients, 18 shards, conc=32, 64 KiB, 60 s × 3 shapes.

| shape | W12 | L1+L2 (#182) | **L3 (#183)** | Δ vs L1+L2 | Δ vs W12 |
|---|---:|---:|---:|---:|---:|
| put-heavy | 8 051 | 7 722 | **4 930 op/s** | **−36.2 %** | **−38.8 %** |
| get-heavy | 89 853 | 97 295 | 87 176 op/s | −10.4 % | −3.0 % |
| mixed | 7 580 | 11 352 | **7 129 op/s** | **−37.2 %** | −5.9 % |

Zero errors. **L3 is a clean, larger regression than L1.**

## The headline diagnostics

```
kiseki_intent_recv_batch_size              mean = 1.25   (target was 3-5; L1 was 1.17)
kiseki_intent_recv_coalesce_wait_seconds   mean = 3334 µs (configured 500 µs!)
kiseki_intent_coalesce_wait_seconds        mean = 5030 µs (configured 500 µs, was 2188 in L1+L2)

bucket distribution (n1):
  ≤ 1 : 64.7 k  (70.7 %)
  ≤ 2 : 16.7 k  (18.3 %)
  ≤ 4 :  9.0 k  ( 9.8 %)
  ≤ 8 :  1.1 k  ( 1.2 %)
  ≥ 16:    0
```

## Per-phase damage

| metric | L1+L2 | **L3** | Δ |
|---|---:|---:|---:|
| `aux.store_put` | 1 740 µs | **4 066 µs** | **+133.7 %** ← coalescer overhead |
| `aux.handle_intent_put_total` | 1 747 µs | 4 073 µs | +133.1 % |
| `intent_fan_inner` | 5 805 µs | 9 479 µs | +63.3 % |
| `composition_record` | 7 802 µs | 10 646 µs | +36.5 % |
| `raft_transport_rpc{intent_put}` | 168 ms | 277 ms | +64.7 % |
| `chunk_fan_inner` | 1 723 µs | 1 160 µs | **−32.7 %** (artifact — less work because PUT throughput collapsed) |
| `committer.sink_incorporate` | 3 896 µs | 2 568 µs | −34.1 % (same artifact) |

## Root cause

**Tokio scheduler latency dominates regardless of coalescer design.**

L1+L2 (per-shard, 100µs timeout): `coalesce_wait` mean = 2.1 ms.
L3 (per-node, 500µs timeout): `coalesce_wait` mean = 3.3 ms.

The wake-up floor (mpsc send → task park → mpsc recv → task unpark) is
~1.6-2 ms under load. The configured timeout adds to that. Bigger
timeout → bigger wait. The batch_size barely moves because at this
load shape, arrivals are bursty and most windows see 0-1 partners
regardless of timeout.

Net: the coalescer adds 3.3 ms of wait per RPC and saves maybe 250 µs
of fjall WAL sync amortisation. Pure regression.

## The wider lesson

Coalescing on the receiver side via a spawned tokio task is the wrong
shape under this workload. Any timeout-based design pays the wake-up
penalty. Two designs could plausibly avoid it:

1. **Inline group-commit inside `FjallIntentStore::put_batch`** —
   when concurrent callers arrive, the first one starts a fjall batch
   and others append to it under a `Mutex`. No spawn, no wake-up
   penalty. Each caller's commit blocks until the batch lands. The
   amortisation is bounded by how many callers are concurrently
   inside `put_batch` (typically 1-2 per shard at this load), but the
   overhead is bounded by the mutex acquisition (~10 ns
   uncontended).
2. **W8-style Raft batching** — the user previously ruled this out
   because fjall already batches. But the producer-side fan and the
   receiver-side commit each pay independent WAL syncs; a higher-
   layer batch crossing both would amortise both. This is a
   significant protocol change.

Neither is a quick win. The honest answer is: **at this workload
shape (96 in-flight, 18 shards, 60 s windows), W12's 8 051 op/s is
the floor of what's achievable without changing either the protocol
or the workload concurrency profile.**

## Recommendation

**Do NOT merge PR #183.** Both L1 (per-shard) and L3 (per-node)
coalescers regressed. The architecture (background task + mpsc) is
the wrong shape for this latency budget.

The PR is on a branch and not merged — we lose nothing by leaving it
unmerged. Close PR #183 with a pointer to this write-up. Keep the
W12 producer-side coalescer (which DOES help, even at batch=1.17,
because of the receiver-side `put_batch` amortisation).

## What I'd actually do next

Three candidates ranked by predicted lift × risk:

1. **Inline group-commit in `FjallIntentStore::put_batch`** (the lever
   #2 from yesterday's response, #175). Avoids tokio scheduler latency
   by serialising through a mutex instead of an mpsc channel. Bounded
   complexity, predictable gains (~10-20 % from fjall WAL sync
   amortisation on truly concurrent callers).
2. **Look outside the intent path.** `chunk_fan_inner` is still 1.7 ms.
   At W12, intent_fan and chunk_fan are roughly balanced; the next
   lever might be on the chunk path (EC encoding, fragment fan
   scheduling, or the cluster_chunk store's lock pattern). Needs a
   chunk-path-targeted profile.
3. **Change the bench shape.** The 18-shard / 96-in-flight workload is
   the limiting factor at this point. A higher-concurrency or
   skewed-key workload would let the existing coalescers do more
   work without code changes — #176 (open).

## Artefacts

In `/tmp/gcp-2026-06-02-l3/`:

- `put-heavy-c{1..3}.json`, `get-heavy-c{1..3}.json`, `mixed-c{1..3}.json`
- `n{1..6}-{before,after}-bench.txt`
- `n1-pprof.svg`
