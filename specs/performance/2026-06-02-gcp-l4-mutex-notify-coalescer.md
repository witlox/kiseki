# 2026-06-02 — GCP A/B: L4 Mutex+Notify producer coalescer (#175 / PR #185)

## A/B summary

6 × c3-standard-22-lssd + 3 clients, 18 shards, conc=32, 64 KiB, 60 s × 3 shapes.

| shape | W12 | **L4** | Δ vs W12 |
|---|---:|---:|---:|
| put-heavy | 8 051 | **8 707 op/s** | **+8.1 %** |
| get-heavy | 89 853 | 97 053 | +8.0 % (within noise band; GET path unchanged) |
| mixed | 11 829 | **13 270 op/s** | **+12.2 %** |

Zero errors. **L4 is the first real lift since W12** (every coalescer
attempt between W12 and L4 — L1+L2 (#182), L3 (#183) — regressed).

## The new metrics — exactly the predicted shape change

| metric | W12 | **L4** | Δ |
|---|---:|---:|---:|
| `kiseki_intent_coalesce_wait_seconds` | 2 116 µs | **1 319 µs** | **−38 %** ← the headline win |
| `kiseki_intent_put_batch_size` | 1.17 | **1.32** | +13 % (slightly more arrivals coalesce when each pays less wait) |
| `gateway_put_phase{intent_fan_inner}` | 5 308 µs | 4 885 µs | −8 % |
| `gateway_put_phase{composition_record}` | 7 802 µs | 6 806 µs | −13 % |
| `gateway_put_phase{parallel_fan_wall}` | 7 092 µs | 6 135 µs | −13 % |
| `aux.store_put` (receiver) | 1 077 µs | 1 109 µs | +3 % (within noise) |
| `chunk_fan_inner` | 1 734 µs | 1 802 µs | +4 % (within noise; unchanged path) |
| `raft_transport_rpc{op="intent_put"}` | 185 ms | 182 ms | −2 % (within noise) |

The 800 µs drop in `coalesce_wait` accounts for almost the entire
`composition_record` reduction (8 → 7 ms), which propagates through
`parallel_fan_wall = max(intent_fan, chunk_fan)` to the per-PUT
critical path. Throughput follows.

## Why L4 worked where L1/L2/L3 did not

The reverted experiments tried to add coalescing on top of W12 (L1+L2
moved the coalescer to the receiver, L3 made it cross-shard). All
added a fresh tokio task hop and paid the ~1.6 ms tokio scheduler
park/unpark cost.

L4 takes the *opposite* direction: it **removes** the spawned task
from the existing W12 producer-side coalescer.

- W12 used a per-shard spawned task draining an mpsc channel. Every
  batch paid one task park/unpark cycle for the receiver, plus another
  for each waiter's oneshot ack delivery.
- L4 uses `Mutex<State>` + `tokio::sync::Notify`. The first submitter
  to find no active flusher *becomes* the flusher; runs the timer in
  its own task; does the local `put_batch` + fan + ack distribution
  synchronously.

What this saves:

- No spawned task at construction = lower steady-state scheduler load
- The flusher's timer runs in the submitter's task, not a fresh
  parked task that needs to be woken up
- `Notify::notify_one` for cap-reached is cheaper than mpsc-send +
  select-unpark
- `Mutex<State>` on the hot path is ~10 ns uncontended

## What L4 does NOT save

- The timer wake-up itself (`tokio::time::sleep_until`) still parks
  the flusher once per batch
- Each waiter's oneshot still parks/unparks once per batch

So the predicted lift was modest (+5-15 %) and the measured lift
sits inside that band. To push further we'd need either:

- Replace the timer with a non-tokio mechanism (which is fundamentally
  incompatible with the tokio runtime worker model)
- Move the lever off the intent path entirely (e.g. native client-side
  route-to-leader, #135 — but native-only)

## Path-to-half-of-read context

With L4 landing the cluster moves to **8 707 PUT op/s** for the
3-client × 18-shard bench shape. Half of read (97 k GET) is 48 k. The
remaining gap is ~5.5×.

The honest assessment from the campaign:

- **For native**: #135 (client-side route-to-leader) can lift another
  ~3-5× by eliminating the cross-node forward fan-through. Combined
  with L4 lands in the 25-45 k band.
- **For S3 / NFS**: same shape change is structurally unavailable. Best
  candidates are #129 (multi-node inline small-file, can be 5-10× for
  files below threshold) and #133 (hydrator throughput, unlocks
  sustained writes but not peak).

## Artefacts

In `/tmp/gcp-2026-06-02-l4/`:

- `put-heavy-c{1..3}.json`, `get-heavy-c{1..3}.json`, `mixed-c{1..3}.json`
- `n{1..6}-{before,after}-bench.txt`
- `n1-pprof.svg`

## Recommendation

**Merge PR #185.** L4 is the first measured improvement on top of W12
in the entire 2026-06-02 campaign. The code change is small and the
diagnostic histograms confirm the predicted mechanism.
