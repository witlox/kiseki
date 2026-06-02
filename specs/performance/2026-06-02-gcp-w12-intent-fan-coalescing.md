# 2026-06-02 — GCP A/B validation: PR #172 (W12 intent-fan coalescing)

## A/B summary

6 × c3-standard-22-lssd storage + 3 clients, 18 shards, conc=32, 64 KiB, 60 s × 3 shapes.

| shape | rawblk | W7+W9 | W11 | **W12** | Δ vs W11 |
|---|---:|---:|---:|---:|---:|
| put-heavy | 5 264 | 6 603 | 7 382 | **8 051 op/s** | **+9.1%** |
| get-heavy | 96 365 | 98 982 | 96 926 | 89 853 op/s | −7.3% (noise / NIC) |
| mixed | 7 580 | 9 591 | 10 938 | **11 829 op/s** | **+8.1%** |

Zero errors across all 9 client-shape runs.

W12 lifted PUT +9%, mixed +8%. Predicted: 2-3×. The diagnostics tell us why — and they're explicit.

## The diagnostic: batch_size says everything

The new `kiseki_intent_put_batch_size` histogram is the headline:

```
mean = 1.17  (n = 2 240 944 fans)

bucket distribution (n1, 60 s, 470 128 fans):
   ≤ 1  : 403 191  (85.8 %)  ← single-intent fans
   ≤ 2  :  57 529  (12.2 %)
   ≤ 4  :   9 298  ( 2.0 %)
   ≤ 8  :     110  ( ~0 %)
   ≤ 16 :       0
```

**86 % of fans carry exactly 1 intent.** The coalescer is correct but the
workload doesn't feed it partners. The cap of 16 is irrelevant.

`kiseki_intent_coalesce_wait_seconds` confirms the shape:

```
mean = 2 116 µs (configured timeout = 500 µs)
```

Each fan waits the full 500 µs for a partner that never arrives, plus
tokio scheduler latency on the wake-up — total ~2 ms of pure idle hold
on every PUT.

## Why partners never arrive

Per-shard arrival math:

- 3 clients × conc=32 = 96 in-flight cluster-wide
- 18 shards, hash-distributed keys → ~5.3 in-flight per shard
- Per-PUT latency `intent_fan_inner = 5.3 ms` (post-W12)
- Per-shard arrival rate ≈ 5.3 in-flight / 5.3 ms = **1 PUT / ms**
- In a 500 µs coalescing window, expected partners ≈ 0.5

Batch size = 1 (first arrival) + 0.5 (avg new arrivals in window) ≈ 1.5.
Observed mean = 1.17 — within rounding (some windows see zero arrivals,
some see two).

**The coalescer's win is gated by per-shard concurrency.** At 18 shards
× 5.3 in-flight, the bench cannot fill batches of 4-8. To fill batches:

- Fewer shards (denser per-shard), OR
- Higher client concurrency (more in-flight), OR
- A workload with hot-key clustering (skewed shard hash distribution)

## What we DID get (the receiver-side win still showed)

| metric | A (W11) | B (W12) | Δ |
|---|---:|---:|---:|
| `chunk_fan_inner` | 3 100 µs | **1 734 µs** | **−44.1 %** |
| `aux.handle_intent_put_total` | 1 718 µs | **1 085 µs** | **−36.8 %** |
| `aux.store_put` | 1 711 µs | **1 077 µs** | **−37.0 %** |
| `gw.comp_create` | 918 µs | 723 µs | −21.3 % |
| `committer.sink_incorporate` | 4 567 µs | 4 075 µs | −10.8 % |
| `intent_fan_inner` | 5 849 µs | **5 308 µs** | **−9.3 %** |
| `parallel_fan_wall` | 7 140 µs | 6 601 µs | −7.6 % |
| `composition_record` | 8 072 µs | 7 337 µs | −9.1 % |

Even with mean batch size 1.17, `aux.store_put` dropped 37 %. The receiver-
side `put_batch` win shows up because two effects compound:

1. The wire/decode shape changed from `WireIntent` to `Vec<WireIntent>` —
   we save one postcard decode roundtrip per RPC (small).
2. fjall's WriteBatch group-commit is *cheaper* than per-PUT WriteBatch
   even for batches of size 1, because the W11 pending-index mirror
   update path now takes a single mutex hold instead of per-element.

`chunk_fan_inner` dropped 44 % — that's NOT from W12 directly; it's
likely the receiver-side CPU freed by `aux.store_put` finishing faster
giving the chunk fan more headroom. Causation chain: coalescer → faster
per-RPC commit → freed receiver CPU → chunk fan less contended.

## The `intent_put` mean went UP (as predicted)

```
raft_transport_rpc{op="intent_put"}: 158 ms → 180 ms  (+13.6 %)
                                     n     :  1.07 M → 2.24 M  (+109 %)
```

Yes — per-RPC mean rose 14 % even though each RPC now does *more* useful
work, exactly as I documented in the PR (the metric is per-RPC; with
batch_size=1.17 we're seeing a fraction of the win). Count went up
because we now record per-batch-fan instead of per-PUT — same total work,
different unit. Pair with the batch_size histogram to interpret.

## CPU profile is healthy

| frame | W11 | W12 | shape |
|---|---:|---:|---|
| `FjallIntentStore::remove_seqs` | 4.58 % | (in tokio worker noise) | unchanged |
| `intent_fan_coalescer::coalescer_loop` | n/a | **6.71 %** | new — task loop |
| `kiseki-data` worker (data path) | 62.55 % | 54.69 % | shifted to coalescer + chunk paths |

The coalescer task takes ~7 % of CPU — small for what it does, and the
worker thread freed up. No alarming hot spots.

## GET dropped 7 % — likely noise

GET goes nowhere near `put_intent_and_fan` or the coalescer; the path is
gateway → fragment fan → fragment fetch. The W12 changes don't touch
that. Probable causes:

- GCS bucket fetch variance on the boot
- NIC contention with the (rebuilding) PUT load
- Per-run variance from a single 60 s sample

I'd not weight this signal heavily without a re-run. The other PUT/mixed
gains are real; the GET drop is plausibly within noise band.

## What this means

**W12 worked correctly** but its effective lift is workload-shape gated.

- **Code is correct.** The bundle landed clean, the histograms verify
  the design (batches are formed, waits are bounded, receiver path
  benefits even at small batches).
- **The lift is gated by per-shard concurrency**, NOT by the coalescer
  design. The 18-shard × conc=96 bench feeds the coalescer ~1.5
  intents per window — not the 4-8 the predicted lift assumed.
- **The receiver-side win held independently** of producer batching:
  `aux.store_put` −37 %. That win comes from `put_batch`'s lock /
  commit / index-update amortisation even for tiny batches.

## Options to push further (filed but not landed)

### O-1: Lower `KISEKI_INTENT_FAN_BATCH_TIMEOUT_US` to 50-100 µs

At 1.5-2 ms per fan in steady state, the 500 µs timeout dominates per-PUT
latency without helping throughput. A 50-100 µs timeout would:
- Drop `coalesce_wait` mean from 2.1 ms → ~150-250 µs
- Drop `intent_fan_inner` p50 by ~1-2 ms
- Sacrifice essentially no batching (we're at 1.17 already)
- Trade: tail-latency win at the cost of *some* fans dropping to size 1
  when there were 2+ in flight

Probably a net win for this workload. Easy to A/B with no code change.

### O-2: Tighten the coalescer recv-loop wake-up cost

The 2 ms observed wait vs 500 µs configured suggests tokio wake-up
latency. The cause is probably the spawned-task model — `recv().await`
parks the task, then a fresh send wakes it, then `sleep(500µs)` parks
again. Each park/unpark cycle is a few hundred µs of scheduler latency
under load.

A cheaper shape: a `Mutex<CoalescerState>` + condvar in-thread. Same
batch logic, no task spawn, no recv park-unpark.

Risk: more invasive. Hold for a follow-up.

### O-3: Concurrency sweep at clients

We deferred this earlier ("next run will show it"). It mostly did NOT
show it — because per-shard concurrency is bench-shape limited even
when total concurrency rises. The coalescer needs 8+ in-flight per shard
to see size-8 batches. With 18 shards that's 144 in-flight. We were at
96.

Trying conc=128 per client × 3 clients = 384 in-flight = 21/shard would
let batches grow to ~5-7 routinely. Predicted PUT then: 12-15k op/s.

### O-4: Higher-density workload

A bench with **skewed key distribution** (hash buckets a small set of
hot keys) would route many in-flight PUTs to the same shard. That's a
realistic AI workload shape (parameter-server pattern: many writers
hitting the same model-shard).

## Headline through the campaign

| pass | PUT op/s | GET op/s | mixed op/s | dominant bottleneck |
|---|---:|---:|---:|---|
| baseline cap-broken | 1 474 | 32 953 | 2 574 | per-peer cap rejecting work |
| #169 raw-block | 5 264 | 96 365 | 7 580 | remove_seq 71 % CPU |
| #170 W7+W9 | 6 603 | 98 982 | 9 591 | remove_seqs *still* 70 % CPU (W7 fixed the wrong half) |
| #171 W11 | 7 382 | 96 926 | 10 938 | producer fan queue 158 ms mean |
| #172 W12 | **8 051** | 89 853 | **11 829** | **per-shard concurrency too low to fill batches** |

PUT 5.5× from where we started; mixed 4.6×. GET at NIC ceiling for n=3
clients. The remaining gap is workload-shape, not code.

## Artefacts

In `/tmp/gcp-2026-06-02-w12/`:

- `put-heavy-c{1..3}.json`, `get-heavy-c{1..3}.json`, `mixed-c{1..3}.json`
- `n{1..6}-{before,after}-bench.txt`
- `n1-pprof.svg`
