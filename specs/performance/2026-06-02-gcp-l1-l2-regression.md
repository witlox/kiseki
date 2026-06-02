# 2026-06-02 — GCP A/B: Levers 1 + 2 REGRESSED — design wrong

## A/B summary

6 × c3-standard-22-lssd + 3 clients, 18 shards, conc=32, 64 KiB, 60 s × 3 shapes.

| shape | rawblk | W7+W9 | W11 | W12 | **L1+L2** | Δ vs W12 |
|---|---:|---:|---:|---:|---:|---:|
| put-heavy | 5 264 | 6 603 | 7 382 | 8 051 | **7 722 op/s** | **−4.1 %** |
| get-heavy | 96 365 | 98 982 | 96 926 | 89 853 | 97 295 op/s | +8.3 % (recovered W12's noise) |
| mixed | 7 580 | 9 591 | 10 938 | 11 829 | **11 352 op/s** | **−4.0 %** |

Zero errors. **PUT/mixed regressed; GET recovered W12's noise dip.**

## The new metrics expose the design flaw

Headline numbers from the W12 follow-ups landed in #182:

```
kiseki_intent_put_batch_size              mean = 1.09   n = 2.30 M   (producer, was 1.17 in W12)
kiseki_intent_coalesce_wait_seconds       mean = 2188 µs n = 944 k   (producer, was 2116 µs in W12)
kiseki_intent_recv_batch_size             mean = 1.17   n = 1.35 M   (NEW — receiver)
kiseki_intent_recv_coalesce_wait_seconds  mean = 748 µs  n = 1.44 M   (NEW — receiver)
```

The **receiver** coalescer's batch_size of **1.17** is the same shape as
the producer-side coalescer's. 86.8 % of receiver flushes carry exactly
one RPC.

### Bucket distribution (n1 only, 60 s)

```
intent_recv_batch_size:
  ≤ 1 :  158 764  (86.8 %)
  ≤ 2 :   22 165  (12.1 %)
  ≤ 4 :    2 003  ( 1.1 %)
  ≤ 8 :       10  ( ~0 %)
  ≤ 16:        0
```

## Why the receiver-side coalescer didn't help

My PR #182 was based on the assumption "the receiver is the cluster-wide
aggregation point — many producers fan to one receiver." That's wrong
for per-shard coalescing.

**Each shard has exactly ONE leader = ONE producer.** A shard's
receiver-side coalescer accepts `intent_put` RPCs only from that one
leader. Per-shard concurrency at the receiver is bounded by the
producer's pipeline depth (5-ish in-flight per shard), which is the
SAME arrival rate as the producer side. We added a coalescer with the
same fundamental limitation.

What the receiver-side coalescer would need to actually batch:

| design | batching capacity |
|---|---|
| per-shard receiver (what landed) | bounded by per-shard producer pipeline → same as producer |
| **per-node receiver** (cross-shard) | aggregates 3 shards/node × 1 producer/shard × 5 in-flight = 15 |
| protocol change (multi-producer fan-in) | only happens during membership transitions |

The per-shard receiver coalescer adds:

- mpsc + oneshot per-RPC overhead (~600 µs visible in `aux.store_put`)
- An extra tokio wake-up cycle on every receiver-side intent_put

Total per-RPC overhead: ~660 µs. With no batching benefit, that's pure
regression.

## What the per-phase histograms confirm

| metric | W12 | L1+L2 | Δ |
|---|---:|---:|---:|
| `aux.store_put` | 1 077 µs | **1 740 µs** | **+61.5 %** ← receiver overhead, no batching |
| `aux.handle_intent_put_total` | 1 085 µs | 1 747 µs | +61.0 % |
| `intent_fan_inner` | 5 308 µs | 5 805 µs | +9.4 % |
| `pif.total` | 5 305 µs | 5 802 µs | +9.4 % |
| `composition_record` | 7 337 µs | 7 802 µs | +6.3 % |
| `chunk_fan_inner` | 1 734 µs | 1 723 µs | −0.6 % (unchanged) |
| `raft_transport_rpc{intent_put}` | 180 ms | 168 ms | −6.7 % |

Note `chunk_fan_inner` is essentially identical — confirms the
regression is on the intent path, not the chunk path.

## Lever 2 also approximately a no-op

The producer-side `coalesce_wait` mean stayed at **2 188 µs** despite
the timeout config dropping 500 → 100 µs. The 1.6 ms tokio
scheduler-wake-up floor I called out in the design notes is the actual
cap; the configured timeout matters only for the rare case of a partner
arrival ≥ 100 µs after the first one (where the producer would
otherwise wait for that partner). At 1.17 mean batch size that's a
small fraction of fans.

## Recommendation

**Revert PR #182.** The receiver-side coalescer is a clear regression
under per-shard hosting; Lever 2's timeout change was approximately
free but neither helped nor hurt.

Possible follow-ups (each as a separate PR):

1. **Per-node cross-shard receiver coalescer** — the design that
   actually matches the concurrency shape. Substantial work: needs a
   per-node coalescer that flushes a `put_batch` PER SHARD inside a
   single fjall transaction (atomic across shards). Risk: changes the
   per-shard atomicity contract.
2. **#175 (Mutex+condvar producer coalescer)** — directly attacks the
   1.6 ms scheduler wake-up floor that bounds Lever 2's gain. The
   producer-side benefit might still be small (per-shard arrival rate
   is the same), but the wake-up overhead would vanish.
3. **Look elsewhere.** chunk_fan_inner is still 1.7 ms — at this point
   intent_fan and chunk_fan are roughly balanced. The next lever might
   not be on the intent path at all.

## Artefacts

In `/tmp/gcp-2026-06-02-l1-l2/`:

- `put-heavy-c{1..3}.json`, `get-heavy-c{1..3}.json`, `mixed-c{1..3}.json`
- `n{1..6}-{before,after}-bench.txt`
- `n1-pprof.svg`
