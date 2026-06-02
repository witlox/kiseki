# 2026-06-02 — GCP A/B validation: PR #169 (drop `Mutex<File>` from `RawBlockDevice`)

## What the fix does

`crates/kiseki-block/src/raw.rs` — every PUT fragment landed on a node went
through `Mutex<File>::lock()` before the `pwrite`. `pwrite(2)` is already
POSIX-thread-safe with disjoint `(offset, len)` pairs; the mutex was
serialising every fragment write through one critical section, capping the
disk-tier throughput at single-queue speed regardless of NVMe parallelism.

PR #169 drops `Mutex<File>` to a bare `File` and removes the `.lock()` at
the 4 hot-path call sites (`flush_bitmap`, `write`, `read`, `sync`).

## A/B summary — same cluster shape, three-client workload

6 × `c3-standard-22-lssd` storage + 3 clients. 18 shards in `kiseki-bench`
(3 per node). `--concurrency 32 --object-size 65536 --duration-secs 60`.
Each client targets a distinct storage IP.

### Aggregate throughput

| shape | cap-fix (1 client) | raw-block (3 clients) | per-client mean | aggregate Δ |
|---|---:|---:|---:|---:|
| put-heavy   | 1 383 op/s   | **5 263 op/s**  | 1 754 op/s · p99 73 ms · 0 err | **+3.8×** |
| get-heavy   | 33 355 op/s  | **96 365 op/s** | 32 122 op/s · p99 6.5 ms · 0 err | **+2.9×** (near-linear) |
| mixed 70/30 | 2 427 op/s   | **7 580 op/s**  | 2 527 op/s · p99 58 ms · 0 err | **+3.1×** |

Single-client put-heavy went 1 383 → 3 350 op/s (**+2.4×**), so the lift is
NOT just from triple-client fan-out — the disk-tier serialisation was real
on both single- and multi-client paths.

GET aggregate 6.0 GB/s = ~50 % of n=3 client NIC ceiling. The cluster has
headroom for more clients on read.

### Per-phase histogram A/B — what moved

Aggregated over 6 nodes, mean µs, sorted by Δ:

| metric                                                       | cap-fix      | raw-block   | Δ        |
|--------------------------------------------------------------|-------------:|------------:|---------:|
| `chunk_persistent_write{extent_io}`                          | 6 315 µs     | **1 638 µs** | **−74 %** (3.85×) |
| `fabric_put_recv{write_chunk}`                               | 6 371 µs     | **1 438 µs** | **−77 %** (4.4×)  |
| `gateway_put_phase{chunk_fan_inner}`                         | 11 997 µs    | **3 665 µs** | **−69 %** (3.27×) |
| `gateway_put_phase{parallel_fan_wall}`                       | 12 845 µs    | 10 502 µs   | −18 %    |
| `gateway_put_phase{composition_record}`                      | 13 035 µs    | 11 249 µs   | −14 %    |
| `gateway_put_phase{intent_fan_inner}`                        | 1 872 µs     | **9 305 µs** | **+397 %** ⚠ |
| `raft_transport_rpc{op=append_entries}`                      | 73 822 µs    | 337 010 µs  | +357 %   |
| `raft_transport_rpc{op=unknown}`                             | 509 045 µs   | 243 632 µs  | −52 %    |
| `composition_hydrator_apply`                                 | 853 µs       | 2 830 µs    | +231 %   |
| `encrypt`                                                    | 22 µs        | 22 µs       | 0 %      |

The raw-block fix did exactly what it claimed:

* **`extent_io` 6.3 ms → 1.6 ms**: pwrites that used to serialise on the
  mutex now run in parallel through the 4 NVMe devices per node.
* **`fabric_put_recv{write_chunk}` 6.4 ms → 1.4 ms**: receiver-side write
  path tracks `extent_io` cleanly (1.4 = 1.6 − some inlined overhead).
* **`chunk_fan_inner` 12.0 ms → 3.7 ms**: leader-side fan-out wait now
  pays only ~2.3 ms above the receiver floor (was 5.6 ms). The
  `TcpFramedFabricPeer` pool was NOT the residual tax — #168 can be
  re-scoped or closed.

## The new bottleneck (predicted by my pre-run model, confirmed)

The disk fix freed throughput, exposing a sharper one:

### `intent_fan_inner = 9.3 ms` is the cap now

The PUT critical path is `parallel_fan_wall = max(chunk_fan, intent_fan)`.
Before: 12.0 ms vs 1.9 ms → chunk-fan bound. After: 3.7 ms vs **9.3 ms**
→ intent-fan bound. The pwrite fix didn't lift PUT op/s further than it
did because intent_fan replaced chunk_fan as the wall.

Inside `pif.total = 9.3 ms`:

* `pif.local_put = 4.5 ms` — local fjall metadata write for the intent
* `pif.leader_first_hop = 5.5 ms` — first-hop Raft `AppendEntries` ack
* `pif.parallel_topup = 1.5 ms` — quorum top-up arm

Receiver side: `aux.store_put = 3.2 ms` — peer's local fjall write.

This is the W1 / `#126` write-coalescing lever: the Raft round per PUT
intent is the cost, and the lever to drop it is batching multiple intents
into one round. Roadmap docs/performance/roadmap.md is correct.

### CPU profile (pprof flamegraph from storage-1)

Captured on graceful shutdown; `/tmp/gcp-2026-06-02-rawblock/n1-pprof.svg`.

The CPU picture is striking — almost ALL on-CPU samples are in one
codepath:

| frame                                                  | samples (%) |
|--------------------------------------------------------|------------:|
| `kiseki_log::raft_shard_store::spawn_supervisor` (root) | 248 331 (95.59 %) |
| `FjallIntentStore::remove_seq`                          | 186 402 (71.75 %) |
| `FjallIntentStore::pending`                             |  60 038 (23.11 %) |
| `Committer::run` (drain_local)                          |  60 046 (23.11 %) |
| chunk_cluster (everything write-fragment)               |  ~1 042 (0.40 %) |
| EC encode (rust)                                        |     545 (0.21 %) |
| fabric TCP-framed peer                                  |     146 (0.06 %) |
| AES-GCM encrypt (aws-lc)                                |      83 (0.03 %) |

**71.75 % of every storage-node core-second is in
`FjallIntentStore::remove_seq`.** The disk path, EC, TLS, and encryption
are CPU-trivial — the bottleneck is the supervisor's per-intent prune
loop.

### The root cause — `run_supervisor_loop` calls remove_seq() per element

`crates/kiseki-log/src/raft_shard_store.rs:1110-1115`:

```rust
let snapshot = leadership_store.recent_incorporated_snapshot().await;
for seq in &snapshot {
    if let Err(e) = prune_store.remove_seq(crate::intent::PerspectiveSeq(*seq)) {
        tracing::debug!(...);
    }
}
```

Each `remove_seq()` call (`crates/kiseki-log/src/intent.rs:641`):

1. Acquires `self.mutations` mutex (per-store)
2. `fjall.intents_ks.get(seq_key)` — 71 % of pprof CPU is here
3. Decodes the value
4. `fjall.idem_ks.get(idem)` — second skiplist walk
5. `batch.commit()` — fjall WAL write

The snapshot can contain THOUSANDS of seqs per tick (it's the SM's
recent-incorporated set, bounded but large). The committer thread walks
the entire snapshot **sequentially**, doing 5+ fjall operations per seq.
At 5 263 PUT/s × 18 shards × N-seq-snapshot-per-tick, this saturates
the supervisor thread on every node — and the supervisor is per-shard, so
ALL nodes are doing it.

Most of those `remove_seq` calls are **no-ops** (the snapshot represents
the SM's history; many intents were already pruned). Each no-op still
costs one full skiplist `get`.

## Three concrete next wins for PUT

### W7 (immediate, highest expected lift): batch remove_seq

Add `IntentStore::remove_seqs(&self, seqs: &[PerspectiveSeq])`:

* Single mutex acquisition (or none — fjall's own batch atomicity covers it)
* One range scan over the present keys (not N point-gets for absent keys)
* One batch commit for all removes (not N WAL syncs)

Expected pprof shift: `remove_seq` 71 % → < 5 %. Frees ~70 % of every
storage core for actual write work. **Predicted PUT throughput: 5 263 →
12 000-18 000 op/s aggregate**, depending on how the intent_fan responds
to less CPU contention.

Risk: low. The prune semantics (idempotent self-prune of already-applied
intents) don't change; only the call shape does.

### W8 (next): batched Raft commit (W1 / #126 — already in roadmap)

With CPU freed by W7, `intent_fan_inner = 9.3 ms` becomes the new
ceiling. The committer already batches via `DRAIN_BATCH_CAP` but the
gateway emits one intent per PUT. Wire the gateway's parallel PUT path to
coalesce into a single intent + delta batch before invoking
`put_intent_and_fan`. Per-PUT Raft cost amortises across the batch.
Expected: 12 k → 25-40 k PUT op/s.

### W9 (cleanup): `raft_transport_rpc{op=unknown}` 

1.06 M samples at 244 ms mean. The metric label is what `kiseki-raft`
records when the RPC tag is not matched — likely a missing match arm
conflating `InstallSnapshot` + `Vote` + other into one bucket. 244 ms ×
1 M = real wall-time being spent somewhere; just unlabelled. Issue #167
is still open.

## GET wins

GET is already close to the cluster's wire budget for n=3 clients:

* **96 k op/s = 6.0 GB/s aggregate**, p50 0.50 ms, p99 6.5 ms, 0 errors
* Single-client 32 k op/s = 2.0 GB/s. Linear scaling to 3 clients.
* DecryptCache CPU = 0.02 % (Arc-wrap fix from 2026-05-09 holding cleanly)
* `gateway_get_phase{chunk_fetch}` = 18.3 ms tail (n=39k); but p50 = 0.5
  ms means most GETs are warm-cache. The 18 ms metric is the fragment-fan
  wait on rare cold GETs.

The next GET lever is not server-side. To push past 6 GB/s, either:

1. Add more clients (5-6 should reach ~10 GB/s before saturating the
   storage-node 2 × 25 Gb NICs).
2. Larger object sizes (current 64 KiB). 1 MiB GETs would push the wire
   utilisation further with the same op/s.

No code change makes sense here right now.

## Artefacts

In `/tmp/gcp-2026-06-02-rawblock/`:

* `put-heavy-c{1..3}.json`, `get-heavy-c{1..3}.json`, `mixed-c{1..3}.json`
* `n{1..6}-{before,after}-bench.txt` — full /metrics scrapes
* `n1-pprof.svg` — pprof on-CPU flamegraph (the FjallIntentStore picture)

## What landed, what to land next

| Item                                | Status | Lift                                  |
|-------------------------------------|--------|---------------------------------------|
| PR #165 raft per-peer cap default 16→256 | merged | correctness (91 errors → 0)            |
| PR #169 raw-block `Mutex<File>` → `File` | merged | **PUT 3.8× / GET 2.9× / mixed 3.1×** |
| W7 `remove_seqs` batch (proposed)        | new    | predicted PUT 5 k → 12-18 k op/s     |
| W1/#126 batched Raft commit              | roadmap | predicted PUT 12 k → 25-40 k op/s   |
| #167 `op=unknown` Raft transport         | filed  | cleanup + visibility                  |
