# 2026-06-01 — Local 3-node loopback: RCA for the GCP per-phase overshoot

The 2026-06-01 GCP fabric-TCP A/B (#163) returned aggregate ops/s
that were flat vs the gRPC baseline, with per-phase histograms whose
absolute values felt very high:

| phase              | GCP n=6   | Hardware floor estimate | Overshoot |
|--------------------|-----------|-------------------------|-----------|
| `composition_record` | 8 335 µs | <500 µs (writer thread) | ~17× |
| `parallel_fan_wall`  | 8 000 µs | RTT + receiver work     | ~20× |
| `intent_fan_inner`   | 5 590 µs | RTT + Raft append       | ~7× |
| `chunk_fan_inner`    | 3 320 µs | RTT + receiver write    | ~6× |
| `put_recv.write_chunk` (receiver) | 6 500 µs | Local NVMe write + meta commit | ~75× |

The receiver-side `put_recv.write_chunk` is the most diagnostic
single number — it isolates the cost on the follower side, excluding
RTT and Raft. A 6.5 ms cost for a 64 KiB chunk write to local NVMe
demands an explanation.

This memo runs the three diagnostics the user asked for on a local
3-node loopback (ramdisk, no real network) to rule the lock graph in
or out as the cause.

## Three tests (local 3-node loopback, ramdisk-backed)

### Test 1 — receiver-side `write_chunk` decomposition (conc sweep)

Per-follower `kiseki_fabric_put_recv_phase_duration_seconds`, mean
across both followers, against `kiseki://127.0.0.1:19101` (TCP-framed
native binding, TCP-framed fabric default, 100 ms flush coalescing
defaults from `spawn-3node-v2.sh`):

| conc | client ops/s | client p50 | client p99 | recv `decode` | recv `write_chunk` |
|------|--------------|------------|------------|---------------|--------------------|
| 1    | 2 906        | 309 µs     | 951 µs     | 6.5 µs        | **37 µs**          |
| 4    | 8 483        | 416 µs     | 1.05 ms    | 8.6 µs        | **56 µs**          |
| 16   | 10 961       | 1 325 µs   | 7.24 ms    | 18 µs         | **86 µs**          |

Receiver-side write_chunk grew from 37 µs → 86 µs as concurrency
went 1× → 16×. That is a **2.3× growth for 16× load** — modest
elastic growth, not the linear queue blow-up we'd see if a lock or
device-side queue were saturating on the follower path.

Client p50 latency grew **4.3×** (309 µs → 1 325 µs) over the same
range, and ops/s plateaued at conc=16 around 10.9 k. The bottleneck
is **on the leader path**, not on the follower receiver.

### Test 2 — openraft per-shard serialization (code scan)

Source: `crates/kiseki-log/src/raft/state_machine.rs:916` and the
ADR-047 hot-span instrumentation already in the SM.

The Raft state machine apply loop is the per-shard serialization
point — `futures::lock::Mutex<ShardSmInner>`. Findings:

- One lock per shard (per-shard, not process-wide).
- Held for the **entire commit batch** (openraft streams all
  ready-to-apply entries into one apply call).
- Per-entry cost: 600 ns – 1.5 µs hot (from existing instrumentation).
- 64 entries batch-applied under one lock hold = 40–96 µs of
  serialized CPU per shard.
- Replication to followers is per-follower channels in openraft;
  **not** serialized through this lock.
- fjall log fsync runs on a background task at
  `KISEKI_RAFT_FLUSH_INTERVAL_MS=100`; **not** on the
  `client_write()` critical path.
- PeerConnPool serializes per slot (16 slots/peer default); for a
  3-node cluster at conc=64 to one shard, RPCs spread 4 per slot.

If 64 writes converge on one shard at the leader, they batch through
the apply lock in ~40–96 µs CPU. On localhost that matches the
observed scaling. **The per-shard apply lock is not the GCP
bottleneck either** — 96 µs ≪ 5.6 ms `intent_fan_inner`.

### Test 3 — concurrency=4 vs concurrency=16 (same data as Test 1)

The hypothesis was: if `write_chunk` drops to <500 µs at conc=4 and
stays ≥5 ms at conc=16, queueing is confirmed; if it stays flat,
queueing is *not* on the receiver.

Result: write_chunk goes 56 µs → 86 µs (1.5× growth) for a 4× load
step. **Queueing is not on the receiver-side write_chunk path**.

## What this means for the GCP overshoot

The local 3-node loopback rules out:

- **Receiver-side write_chunk** as the dominant cost. Locally
  it's 86 µs at conc=16; GCP shows 6 500 µs at similar conc — a 75×
  gap that the local lock graph and fjall path do **not** produce.
- **The per-shard apply lock** as the dominant cost of
  `intent_fan_inner`. Locally that path is sub-100 µs even under
  saturation; GCP shows 5 590 µs.
- **The chunk-cluster `ChunkEnvelopeRegistry` process-wide
  mutex** (`server.rs:115`) as a queueing point. It is the single
  contention candidate that's process-wide rather than per-shard, but
  the scaling data (37 → 56 → 86 µs as conc grows 1 → 4 → 16) shows
  ≤ 2.3× growth on a 16× load step. A saturated process-wide lock
  would grow linearly.

What it does **not** rule out — and what the GCP overshoot likely
is:

1. **Real-network RTT on the chunk and intent fans.** GCP intra-zone
   RTT is ~50–100 µs each way; multiple sequential round-trips on
   the EC fan (encrypt → fragment → fan to followers → ack → SM
   apply → ack-to-client) compound. The receiver-side measurement
   excludes RTT by design.
2. **Receiver-side fjall meta commit cost on real NVMe.** Locally
   the meta-store fjall lives on tmpfs (ramdisk, 14 GiB tmpfs in
   this run); on GCP it lives on the local SSD. A 6.5 ms
   write_chunk on real local SSD with the 100 ms flush-interval
   *defaults disabled in production* would match a per-write fsync.
   **Action**: verify GCP startup sets the three
   `KISEKI_*_FLUSH_INTERVAL_MS` knobs; if not, that single env
   change is the GCP fix.
3. **Composition store + log emit on real disk.** The
   `composition_record` aggregate (8 ms) is the post-encrypt write
   region per #164's split: `Compositions::create` +
   `log.emit_delta` + `parallel_fan_wall`. On ramdisk we measured
   1 677 µs for the whole region; on GCP it's 8 335 µs. The 6.7 ms
   gap is largely real-network RTT for `parallel_fan_wall`, plus
   real-disk cost for `log.emit_delta`. **#164** splits these so we
   stop conflating them.

## Honest conclusion

The local-instance tests **invalidate** the "GCP per-phase numbers
are caused by the chunk-cluster lock graph or by openraft per-shard
serialization" hypothesis. Those are sub-100 µs paths.

The likely GCP root cause is unsexy:

- **Sync-per-write on receiver fjall meta commit** (no flush-interval
  set on the GCP boot) — accounts for most of the 6 500 µs
  receiver `write_chunk` overshoot.
- **Real-network RTT compounded across the EC fan** — accounts for
  most of the `parallel_fan_wall` and `intent_fan_inner` overshoot.
- **Honest histogram naming** (#164 split of `composition_record`
  into `comp_record.create_call` / `log_emit` / `fan_wait`) so we
  stop reading "8 ms composition record" as "the composition store
  is slow."

## Next steps (in this priority order)

1. **GCP A/B at the flush-interval knob.** Add
   `KISEKI_RAFT_FLUSH_INTERVAL_MS=100`,
   `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100`,
   `KISEKI_CHUNK_FLUSH_INTERVAL_MS=100` to the GCP startup script's
   storage-node env, re-run the put-heavy bench, expect a 5–10× lift
   in receiver-side `write_chunk`.
2. **Ship #164** (split `composition_record`) so the next GCP run
   reports the three sub-phases honestly.
3. **Keep #163's instrumented binary** for a re-run after step 1 to
   close the loop on per-phase RCA.

## Data files

`/tmp/3node-conc-sweep/`:
- `c{1,4,16}.json` — client bench results (TCP-framed fabric, default coalescing)
- `n{2,3}-{before,after}-c{1,4,16}.txt` — follower /metrics snapshots

The original conc=64 run filled `/tmp` (the 4 GiB chunk-store cap
× 3 nodes × high throughput at ≥10 k op/s) and produced no JSON;
it was not re-attempted because the conc=1/4/16 scaling already
falsified the hypothesis under test.
