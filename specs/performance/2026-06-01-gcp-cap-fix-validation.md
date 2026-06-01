# 2026-06-01 — GCP A/B validation: PR #165 (per-peer cap 16 → 256)

## What the fix does

- `crates/kiseki-raft/src/tcp_transport.rs:84` — raise
  `RAFT_TRANSPORT_PER_PEER_MAX_DEFAULT` 16 → 256.
- `infra/gcp/scripts/setup-raw-storage.sh` — set
  `KISEKI_RAFT_PER_PEER_MAX=128` and
  `KISEKI_RAFT_CONN_POOL_PER_PEER=128` explicitly.

## A/B summary, same cluster shape, same workload

Both runs: 6 × `c3-standard-22-lssd` storage + 1 client (kiseki-client-1).
18 shards in the bench namespace (3/node). `--concurrency 32
--object-size 65536 --duration-secs 30` per shape. Instrumented
profile binary in both passes.

| shape | broken-cap (16) | cap-fix (128) | ops/s Δ | errors Δ |
|---|---:|---:|---:|---:|
| put-heavy | 1 474 op/s · 4.1 ms p50 · 107 ms p99 · **33 err** | 1 383 op/s · 4.3 ms p50 · 111 ms p99 · **0 err** | −6 % | −33 |
| get-heavy | 32 953 · 543 µs p50 · 1.97 ms p99 · 0 err | 33 355 · 551 µs p50 · 3.12 ms p99 · 0 err | +1 % | 0 |
| mixed (70/30) | 2 574 · 2.5 ms p50 · 63 ms p99 · **58 err** | 2 427 · 2.6 ms p50 · 77 ms p99 · **0 err** | −6 % | −58 |

**The fix did what it said:** zero per-peer cap rejections in any
node journal post-fix (verified via `journalctl -u kiseki-server
| grep -c "per-peer cap exceeded"` = `0,0,0,0,0,0`). The 33 + 58 = 91
PUT quorum-shortfall errors went to zero.

**The fix did not lift throughput.** Aggregate PUT ops/s actually
dropped ~6 % (within noise / inter-run variance for a one-shot
benchmark, but definitely not the 5–10× lift I projected). My earlier
hypothesis "the cap is gating throughput" was wrong: the cap was
gating *errors* (rejections + quorum shortfall), not work.

## Per-phase histogram A/B — what actually moved

Aggregated over all 6 nodes, mean µs, sorted by broken-cap value:

| metric                                                       | broken cap | cap fix    | Δ        |
|--------------------------------------------------------------|-----------:|-----------:|---------:|
| `raft_transport_rpc{op=append_entries}`                      | 128 752 µs | 73 822 µs  | **−43 %** |
| `raft_transport_rpc{op=unknown}`                             | 109 254 µs | 509 045 µs | **+366 %** |
| `gateway_put_phase{composition_record}`                      | 12 352 µs  | 13 035 µs  | +6 %     |
| `gateway_put_phase{raft_commit}` (alias of intent+fan)        | 12 186 µs  | 12 845 µs  | +5 %     |
| `gateway_put_phase{parallel_fan_wall}`                       | 12 186 µs  | 12 845 µs  | +5 %     |
| `gateway_put_phase{chunk_write}` (alias)                     | 12 186 µs  | 12 845 µs  | +5 %     |
| `gateway_put_phase{chunk_fan_inner}`                         | 11 303 µs  | 11 997 µs  | +6 %     |
| `chunk_persistent_write{extent_io}`                          | 6 639 µs   | 6 315 µs   | −5 %     |
| `fabric_put_recv{write_chunk}`                               | 6 618 µs   | 6 371 µs   | −4 %     |
| `gateway_put_phase{intent_fan_inner}`                        | 1 712 µs   | 1 872 µs   | +9 %     |
| `composition_hydrator_apply`                                 | 856 µs     | 853 µs     | 0 %      |
| `hotpath_step{aux.handle_intent_put_total}` (receiver)       | 658 µs     | 643 µs     | −2 %     |
| `hotpath_step{gw.comp_create}`                               | 153 µs     | 178 µs     | +16 %    |
| `gateway_put_phase{encrypt}`                                 | 23 µs      | 22 µs      | −4 %     |

## Three real conclusions

### A. The `extent_io = 6.3 ms` is real, not a runtime-starvation artifact

Yesterday's `specs/performance/2026-06-01-gcp-instrumented-single-client.md`
hypothesised that `extent_io = 6.6 ms` was secondary to the AppendEntries
129 ms blow-up — "the executor is jammed, the await dilates."

That was wrong. The cap fix dropped AppendEntries 43 % (129 → 74 ms) but
`extent_io` moved ~5 % (6 639 → 6 315 µs). If the disk path were
artifact, it would have followed the AppendEntries drop. It didn't.

So: **a 64 KiB write to the local NVMe SSD on `c3-standard-22-lssd`
is actually taking ~6.3 ms in our chunk-persistent path.** A clean
NVMe should do 30–100 µs at single-queue, several hundred µs under
contention. 6.3 ms means we're either fsyncing per write, queueing
writes through an internal serialization point in the persistent
chunk store, or doing extra work in the extent allocator.

Open question, separate investigation: where in
`crates/kiseki-chunk/src/persistent/extent_io.rs` does the wall
time go? The `chunk_persistent_write` histogram only has phase
`dedup_check` (0.6 µs) + `extent_io` (6 315 µs) + `save_meta`
(7 µs) — three phases that already sum to `write_chunk`. The 6.3 ms
is entirely inside the `extent_io` span. **#166 follow-up: instrument
extent_io with sub-phases (alloc, device_write, fsync_wait,
return).**

### B. `op=unknown` Raft RPCs got dramatically worse

The histogram label `op=unknown` is what `kiseki-raft` records when
the RPC tag doesn't match a known op. Pre-fix, this stood at 109 ms ×
68 243 samples; post-fix, 509 ms × 69 218 samples. The count is
basically the same, but each one now takes 5× longer.

Hypothesis: pre-fix, the cap rejected these RPCs quickly, so the
"duration" the histogram saw was the rejection RTT (~100 ms is
plausible for a tight reject loop). Post-fix, the RPCs now actually
queue and execute, taking 509 ms each. **The 509 ms is hiding
something — either a 4× per-RPC serialization point, or InstallSnapshot
RPCs firing during the high-write storm and being slow.**

**#167 follow-up: identify what `op=unknown` actually is in
`crates/kiseki-raft/src/tcp_transport.rs` (likely a missing match arm
that conflates several Raft RPC types into one label) and split it
into named ops.** Also: 509 ms is probably the real cost of
InstallSnapshot under load; if true, the leader's snapshot install
behaviour during a sustained write spike is now a perf concern in
its own right.

### C. The 12 ms `gateway_put_phase{*}` family is the actual cap

Every put_phase histogram that wraps the post-encrypt write region
sits at ~12 ms regardless of the AppendEntries fix:

- `composition_record` 13 ms = create + log emit + parallel_fan_wall
- `parallel_fan_wall` 12.8 ms = `try_join!(chunk_fan, intent_fan)`
- `chunk_fan_inner` 12.0 ms — the chunk fan
- `intent_fan_inner` 1.9 ms — the Raft intent fan
- `extent_io` 6.3 ms (receiver side)

`parallel_fan_wall` is `max(chunk_fan, intent_fan)` = max(12.0, 1.9)
= 12.0. That matches.

`chunk_fan_inner` = 12.0 ms is leader-side wait on `try_join_all(N
peer PutFragment RPCs)`. With receiver-side `write_chunk = 6.4 ms`
and intra-zone RTT ~150 µs, the natural floor is ~6.5 ms. The gap of
**5.5 ms is somewhere between leader-side serialize-encode and the
TCP-framed fabric peer round-trip**.

The TCP-framed fabric peer pool (`crates/kiseki-chunk-cluster/src/peer/tcp_framed/client.rs`)
is the analogue of the Raft transport's PeerConnPool. If it has the
same kind of low default slot count under leader concurrency, fragments
queue on the leader side waiting for an outbound slot. **#168 follow-up:
check the fabric TcpFramedFabricPeer pool size + add per-phase
histograms for the leader-side fabric send (serialize, queue, RTT,
ack) so we can see exactly where the 5.5 ms lives.**

## Where this leaves PUT performance

Before:
- 1 474 op/s, 33 errors / 30 s
- p99 107 ms
- Identified bottleneck: Raft transport per-peer cap

After:
- 1 383 op/s, 0 errors / 30 s
- p99 111 ms
- Identified bottleneck: **`extent_io = 6.3 ms` (the device write inside
  chunk-persistent) + a `chunk_fan_inner` leader-side fabric tax of
  5.5 ms above the receiver's floor**

Headline 64 KiB PUT throughput is **roughly 1.4 k op/s for a 6-node
n=18-shard GCP perf cluster under 32-conc from one client** with the
current bottleneck shape. GET stays at 33 k op/s. The 22× GET/PUT
asymmetry is intact.

## Why I'm landing PR #165 anyway

Correctness wins matter:
- 91 errors / 30 s → 0 errors. The cap was actually producing
  quorum-shortfall failures during normal-load PUTs. Any production
  deployment with shards × min_acks ≥ 16 was silently dropping
  writes.
- The default 16 was wrong as a default. 256 is fine — TCP
  connections are cheap. Future deployments don't have to remember
  to set the env var.

Throughput will improve when we land #166 (extent_io split) + #167
(op=unknown RCA) + #168 (fabric peer pool audit), which are the
three concrete follow-ups this run identified.

## Artefacts

In `/tmp/gcp-cap-fix-2026-06-01/`:
- `put-heavy.json`, `get-heavy.json`, `mixed.json`
- `n{1..6}-{before-put,after-put,after-get,after-mixed}.txt`
- `n{1..6}-journal.txt`
- `n1-pprof.svg` — pprof on-CPU flamegraph from storage-1
- `perphase-analysis.txt` — full side-by-side comparison
