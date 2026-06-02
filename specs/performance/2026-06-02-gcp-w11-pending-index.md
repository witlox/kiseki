# 2026-06-02 — GCP A/B validation: PR #171 (W11 — in-memory pending-index mirror)

## What the fix does

`crates/kiseki-log/src/intent.rs` — adds a `PendingIndex { by_seq:
BTreeMap, by_idem: HashMap }` mirror to `FjallIntentStore`. fjall stays
the durable source of truth; the mirror is rebuilt on `open()` and
maintained on every mutation under the existing `mutations` mutex.

- `pending`, `next_pending_seq`, `pending_len` are pure memory reads
- `put` dedups against the in-memory idem index
- `remove_seq`, `remove_seqs`, `prune` consult the mirror first;
  absent seqs are no-ops without touching fjall

## A/B summary, same shape, same workload

6 × `c3-standard-22-lssd` storage + 3 clients. 18 shards. conc=32 ×
64 KiB × 60 s × 3 shapes × 3 clients in parallel.

| shape | raw-block | W7+W9 | **W11** | Δ vs W7+W9 | cumulative |
|---|---:|---:|---:|---:|---:|
| put-heavy | 5 264 | 6 603 | **7 382 op/s** | **+11.8 %** | **+40.2 %** |
| get-heavy | 96 365 | 98 982 | 96 926 op/s | −2.1 % (NIC-bound, noise) | +0.6 % |
| mixed | 7 580 | 9 591 | **10 938 op/s** | **+14.0 %** | **+44.3 %** |

Errors zero across all 9 client-shape runs.

## What W11 did to the CPU profile

The 2026-06-02 W7+W9 pprof showed `FjallIntentStore::remove_seqs` still
holding **70.5 %** of every storage-node core (down a hair from 71.75 %
pre-W7). W7's batch saved the WAL commit per snapshot; the per-element
`intents_ks.get()` inside the loop was still burning all the CPU on
already-pruned no-op snapshot entries.

W11 collapsed it:

| frame | A (W7+W9) | B (W11) | Δ |
|---|---:|---:|---:|
| `FjallIntentStore::remove_seqs` | **70.50 %** | **4.58 %** | **−15.4× CPU** |
| `FjallIntentStore::pending` | 19.98 % | (gone — pure memory read) | — |
| `Committer::run` (drain_local) | 23.11 % | (gone) | — |

The pprof now shows the top frames are tokio runtime trampolines under
the `kiseki-data` worker thread (62.55 % of CPU) — the storage nodes
are spending CPU on **actual data-path work** (TCP I/O, encoding,
encryption, fjall puts) instead of supervisor housekeeping. That is the
healthy shape we wanted.

## What W11 did to the per-phase histograms

Aggregated over 6 nodes, mean µs:

| metric | W7+W9 | **W11** | Δ |
|---|---:|---:|---:|
| `committer.read_pending` | 11 837 µs | **8 µs** | **−99.9 %** (1500×) ✅ headline |
| `aux.handle_intent_put_total` | 2 653 µs | 1 718 µs | −35.3 % |
| `aux.store_put` | 2 647 µs | 1 711 µs | −35.4 % |
| `pif.leader_first_hop` | 4 493 µs | 3 485 µs | −22.4 % |
| `raft_transport_rpc{append_entries}` | 133 358 µs | 94 399 µs | −29.2 % |
| `raft_transport_rpc{intent_put}` | 185 370 µs | 158 492 µs | −14.5 % |
| `pif.total` | 6 865 µs | 5 846 µs | −14.8 % |
| `intent_fan_inner` | 6 867 µs | 5 849 µs | −14.8 % |
| `gateway_put_phase{composition_record}` | 8 804 µs | 8 072 µs | −8.3 % |
| `chunk_fan_inner` | 3 216 µs | 3 100 µs | −3.6 % |
| `committer.sink_incorporate` | 4 014 µs | 4 567 µs | **+13.8 %** ⚠ |
| `gw.comp_create` | 580 µs | 918 µs | **+58.4 %** ⚠ |

The 1500× speedup on `committer.read_pending` is the headline — exactly
the predicted shape change. Every receiver-side and producer-side phase
that depends on the committer being responsive moved with it.

## Why throughput went +12 %, not the predicted 15-25 k

Two phases got *worse* under the new shape:

### `committer.sink_incorporate` +14 % — bigger batches are slower

This is the IncorporationSink::incorporate call — the actual Raft round
that commits a batch of intents to the log. With W11's fast
`read_pending`, the committer drains MORE intents per tick (the
accumulated pending set is larger between ticks), so each
`sink_incorporate` invocation is processing a larger batch and takes
longer. **This is desirable: per-intent commit cost falls, even if
per-batch wall time rises.**

Math: 7.4k PUT/s ÷ 18 shards = 411 PUT/s/shard. At
`intent_fan_inner = 5.85 ms` and 5.3 concurrent per shard,
latency-bound capacity is ~900 PUT/s/shard. We're at 46 %. The cap is
**not** per-PUT latency — it's the leader-side wait for quorum acks on
`intent_put`, which is 158 ms mean per RPC.

### `gw.comp_create` +58 % — composition layer under more pressure

Now that the rest of the path is faster, the composition layer is being
hit harder per second. 918 µs is still small (< 1 ms), but it's the
fastest-growing phase under W11. Worth watching.

## The real cap now: `intent_put` RPC mean 158 ms

The producer fans `intent_put` to peer voters BEFORE the gateway acks
(ADR-047 phase 5c). 1.34 M `intent_put` RPCs / 60 s = 22 k aux puts/s.
Mean 158 ms × 22 k/s = **3 500 in-flight RPCs at all times**, which is
heavy queuing. Each receiver runs intent_put through `aux.store_put =
1.7 ms` plus all the queuing.

This is the per-PUT quorum fan that the no-loss floor I-L2/I-CS1 makes
required. To push throughput past where W11 left it requires either:

1. **Coalesce intent_put fans across PUTs to the same peer set.** Multiple
   in-flight PUTs to the same shard could ride one fan RPC. This isn't
   W8 (Raft commit batching, ruled out because Raft already batches);
   it's batching the PRE-Raft quorum fan at the producer.
2. **Larger gateway concurrency.** conc=32 per client × 3 clients = 96
   in flight = 5.3/shard. Pushing to conc=64 or 128 per client would
   give each shard more parallel PUTs and amortise the fan cost.
3. **Per-shard fabric peer pool audit.** The `TcpFramedFabricPeer` pool
   may be capping outbound fan concurrency on busy peer pairs. Need to
   confirm with per-pool-state histograms.

## Headline through this campaign

| pass | PUT op/s | GET op/s | mixed op/s | bottleneck |
|---|---:|---:|---:|---|
| baseline (cap-broken) | 1 474 | 32 953 | 2 574 | per-peer cap (errors) |
| #165 cap fix | 1 383 | 33 355 | 2 427 | extent_io 6.3 ms |
| #169 raw-block | 5 264 | 96 365 | 7 580 | remove_seq 71 % CPU |
| #170 W7+W9 | 6 603 | 98 982 | 9 591 | remove_seqs STILL 70 % CPU |
| #171 W11 | **7 382** | 96 926 | **10 938** | intent_put fan 158 ms RPC mean |

**PUT 5× from where we started.** GET hit ~6 GB/s in the raw-block pass
and stays NIC-bound. Mixed +4.25×. All zero errors.

## Where this leaves the roadmap

The CPU profile is now healthy (`remove_seqs` 4.6 % instead of 70 %).
Wall time has shifted to actual data-path work: Raft replication, fabric
fan, durable LSM writes. Pushing further is no longer about freeing CPU
— it's about reducing the per-PUT fan cost.

Three follow-ups in priority order:

1. **Producer-side intent_put fan coalescing** — multiple in-flight PUTs
   to the same shard ride one quorum-fan RPC. The HARD part is keeping
   the I-L2/I-CS1 no-loss floor; the EASY part is that intents within
   one fan are already independent (different perspective seqs).
2. **Concurrency sweep** — does `conc=64 / 128 per client` push past
   12 k aggregate PUT, or does it just inflate `intent_put` mean further?
3. **`TcpFramedFabricPeer` per-peer pool histograms** — confirm whether
   we're queueing on the outbound fabric send (would explain why
   `intent_put` mean stayed near 158 ms despite W11 lifting throughput).

## Artefacts

In `/tmp/gcp-2026-06-02-w11/`:

- `put-heavy-c{1..3}.json`, `get-heavy-c{1..3}.json`, `mixed-c{1..3}.json`
- `n{1..6}-{before,after}-bench.txt`
- `n1-pprof.svg` — pprof on-CPU flamegraph (the healthy-shape one)
