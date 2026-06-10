# 2026-06-10 GCP run #2 — P1/P2/P3 validation (PR #224)

6-node `default` profile, profile binary off main @ `41b23b5`
(PR #224: ack-path cache insert, per-entry apply batching, bounded SM
delta history). Same dedup-proof bench and gates as the morning run
(`2026-06-10-gcp-group-commit-sweep.md`). Binary identity verified
(`e36e638d…` = staged tarball); recompute pinned via perf-arm.env;
18 shards, leaders distributed; group-commit arm.

## Verdict vs the bar

**Target was PUT ≥ 48k op/s (44k = fail), GET within 20%. Measured
peak PUT = 22.3k. The bar was NOT met.** GET constraint met with
margin (+2.3% vs reference). The run moved the ceiling 1.76× and
eliminated the decay + tail pathologies, and it names the next two
binders with direct measurements.

## Numbers

GET baseline (get-heavy, conn1/conc16 ×3 clients, spread endpoints):
**296,808 op/s aggregate** (98.0k/98.5k/100.3k, p99 ~240 µs) vs the
2026-06-04 reference 290,772 → **+2.3%, no read degradation.**

PUT (put-heavy 4 KiB, conn1, 3 clients, spread endpoints):

| cell | this run (#224) | morning run (pre-#224) |
|---|---:|---:|
| cold throwaway conc64 | **22,275** | 14,479 |
| warm conc64 | **22,272** · p99 132 ms | 12,684 · p99 357 ms |
| warm conc128 | 16,282 | 12,274 |
| warm conc256 | 18,041 | 10,340 |

- **Within-run decay is GONE** (cold ≡ warm to 3 decimal places) —
  the P3 O(N²) snapshot mechanism was the decay.
- p99 at conc64 improved 2.7× (357 → 132 ms).
- Saturation now occurs at conc64; deeper concurrency degrades —
  a single serialization point remains.

## Phase decomposition (storage-1, 1.03M ingress ops)

| phase | mean | before |
|---|---:|---:|
| `inline_cache_insert` (P1) | **7.3 µs** | 5,300 µs (`inline_store_put`) — ×726 |
| `encrypt` | 8.1 µs | — |
| `chunk_fan_inner` | 0.07 µs | — (inline: no fan) |
| `sm.append_delta_inner` (P2+P3) | **0.43 µs** | 4,220 µs — ×9,800 |
| **`pif.total` ≡ `intent_fan_inner`** | **12.0 ms** | — THE wall (`parallel_fan_wall` ≡ it) |
| **`composition_record`** | **13.5 ms** | — second binder |

Everything P1/P2/P3 targeted collapsed by 3-4 orders of magnitude.
The ack wall is now entirely the **intent-fan leg (12.0 ms)** with
**composition_record (13.5 ms)** beside it — both ~1,000× heavier
than every other phase. No finer `pif.*` sub-spans were exported in
this build (only `pif.total`), so in-fan attribution (ingress→leader
forward hop vs coalescer cap (16) vs per-peer pool vs remote intent
put) is the first task of the next pass. A live A/B probe of
`KISEKI_INTENT_FAN_BATCH_MAX=128` was prepared but not run
(operator called teardown).

## Anomaly flagged (honest ledger)

First GET run post-boot: 15,234 errors (0.52% ≈ 1-2 of 256 warmup
objects unreadable for the whole 30 s window — ChunkLost via the
inline fallback chain). A fresh run immediately after: 978,995 ops,
**zero errors**, and zero errors in all subsequent cells. Cold-start
first-touch race (committer/provisioning), not steady-state; worth a
targeted look when touching the committer next. This is the P1
review's named risk class surfacing only at cluster cold start.

## What this validates from #224

- P1: ack path no longer touches the small-object journal (7.3 µs).
- P2: apply-side journal pressure gone (0.43 µs per apply step).
- P3: bounded history — decay eliminated; snapshots bounded
  (no #220-class growth during the run); supervisor-gathered
  watermark pruning ran live without halting any hydrator.

## Next binders (measured, not guessed)

1. **Intent-fan leg, 12.0 ms** at 22k — candidates to decompose with
   restored `pif.*` sub-spans: the ingress→shard-leader forward hop
   (5/6 of writes), coalescer batch cap 16 / flush pipelining,
   per-peer connection pool, remote intent-put service time.
2. **`composition_record`, 13.5 ms** — the composition fjall store
   under ingress load despite the 100 ms buffered mode (#200 class:
   journal mutex + per-op overhead).

## Artifacts

`pprof-out/gcp-2026-06-10-p123/` — per-cell outputs + per-node
metrics snapshots + the 3 GET-baseline JSONs. Cluster destroyed
(24 resources); zero instances left.
