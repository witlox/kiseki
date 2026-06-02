# 2026-06-01 — GCP instrumented single-client RCA

## Setup

- Cluster: 6 × `c3-standard-22-lssd` storage nodes (4 local NVMe SSDs each) + 3 client VMs (only client-1 driving).
- Profile: `default` (europe-west1-b).
- Binary: instrumented (`hot-path-trace` + `pprof`), built 2026-06-01,
  fabric over TCP-framed (#158), shared-handler refactor + TLS API
  (#159), all rev-2/3/4 fjall stores, ADR-042 §2.2 native TCP-framed
  default.
- Storage env (verified via `systemctl show`):
  - `KISEKI_RAFT_FLUSH_INTERVAL_MS=100`
  - `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100`
  - `KISEKI_CHUNK_FLUSH_INTERVAL_MS=100`
  - `KISEKI_RAW_DEVICES=/dev/disk/by-id/google-local-nvme-ssd-{0..3}` (4 raw NVMes per node)
  - `KISEKI_RAFT_THREADS=64`
- Namespace: 18 shards across 6 nodes (3 shards/node).
- Bench: single client at `--concurrency 32 --object-size 65536
  --duration-secs 30` against the leader's native TCP-framed binding
  (`kiseki://10.0.0.10:9103`).

## Headline numbers

| shape       | ops/s    | p50      | p99       | errors        | MiB/s   |
|-------------|---------:|---------:|----------:|--------------:|--------:|
| put-heavy   | **1 474** | 4 064 µs | 106 524 µs | 33 / 44 340 | 92.1    |
| get-heavy   | **32 953** | 543 µs |   1 974 µs | 0 / 988 617 | 2 059.6 |
| mixed (70/30) | **2 574** | 2 491 µs |  63 435 µs | 58 / 77 345 | 160.9   |

GET runs 22× faster than PUT. The write path is the entire problem.

## Per-phase histogram aggregates (mean µs, summed across all 6 nodes, sorted)

### PUT-heavy

| mean µs | count   | metric                                              | label                |
|--------:|--------:|-----------------------------------------------------|----------------------|
| **128 752** | 119 952 | `raft_transport_rpc`                                 | `op=append_entries`  |
| **109 254** | 68 243 | `raft_transport_rpc`                                 | `op=unknown`         |
| 12 352  | 44 340  | `gateway_put_phase`                                  | `phase=composition_record` |
| 12 189  | 44 373  | `hotpath_step`                                       | `gw.put_intent_and_fan_call` |
| 12 186  | 44 340  | `gateway_put_phase`                                  | `phase=raft_commit`  |
| 12 186  | 44 340  | `gateway_put_phase`                                  | `phase=parallel_fan_wall` |
| 12 186  | 44 340  | `gateway_put_phase`                                  | `phase=chunk_write`  |
| 11 303  | 44 340  | `gateway_put_phase`                                  | `phase=chunk_fan_inner` |
| **6 639** | 36 964 | `chunk_persistent_write_phase`                       | `phase=extent_io`    |
| **6 618** | 221 865 | `fabric_put_recv_phase`                              | `phase=write_chunk`  |
| 1 712   | 44 340  | `gateway_put_phase`                                  | `phase=intent_fan_inner` |
| 855     | 31 829  | `composition_hydrator_apply`                         | —                    |
| 657     | 68 243  | `hotpath_step`                                       | `aux.handle_intent_put_total` |
| 651     | 68 243  | `hotpath_step`                                       | `aux.store_put`      |
| 153     | 44 373  | `hotpath_step`                                       | `gw.comp_create`     |
| 60      | 44 373  | `hotpath_step`                                       | `gw.derive_chunk_id` |
| 23      | 44 373  | `gateway_put_phase`                                  | `phase=encrypt`      |
| 9       | 221 865 | `fabric_put_recv_phase`                              | `phase=decode`       |
| 7       | 36 964  | `chunk_persistent_write_phase`                       | `phase=save_meta`    |
| 0.6     | 36 964  | `chunk_persistent_write_phase`                       | `phase=dedup_check`  |

### GET-heavy (for reference)

| mean µs   | count  | metric                            | label                |
|----------:|-------:|-----------------------------------|----------------------|
| 36 156    | 4 066  | `gateway_get_phase`               | `phase=chunk_fetch`  |
| 0.7       | 988 617 | `gateway_get_phase`               | `phase=composition_lookup` |
| 24        | 4 066  | `gateway_get_phase`               | `phase=decrypt`      |

Composition cache hits 988 617 GETs with only 4 066 chunk fetches (one
chunk read per ~243 GETs). The cache works. Decrypt is clean.

## The two non-sensible numbers (in order of severity)

### 1. `raft_transport_rpc{op=append_entries}` = **129 ms mean**

Same-zone intra-VM RTT on GCP `c3-standard-22` is ~150 µs. The
expected histogram mean for an in-zone Raft AppendEntries should be
under 1 ms. **129 ms is 800× the floor.**

**Root cause (confirmed in journals)**:

```
Jun 01 19:27:15 kiseki-storage-4 kiseki-server[15980]:
  WARN kiseki_raft::tcp_transport:
    rejecting Raft RPC connection — per-peer cap exceeded
    peer=10.0.0.10 active=17
```

`crates/kiseki-raft/src/tcp_transport.rs:84` —
`RAFT_TRANSPORT_PER_PEER_MAX_DEFAULT = 16`.

Tunable via `KISEKI_RAFT_PER_PEER_MAX` (inbound cap) and
`KISEKI_RAFT_CONN_POOL_PER_PEER` (outbound pool size). **Neither
is set in `infra/gcp/scripts/setup-raw-storage.sh`** so we run at
the default.

At 18 shards × `min_acks=2` followers per AppendEntries fan, the
leader needs up to 36 inflight RPCs to a single follower under
saturating write load. The 16-slot cap on the inbound side rejects
new connections, the outbound side stalls on slot availability,
AppendEntries latency blows up.

This also produced 33 PUT errors via quorum shortfall:

```
WARN kiseki_log::raft_shard_store: put_intent_and_fan: quorum
  shortfall — refusing to ack shard_id=9e9784ee-c6fd-4e7f-8deb-bd4f4cd0872e
  acks=1 min_acks=2
```

**Fix** (zero-code, just env-var on the GCP storage nodes):

```
Environment=KISEKI_RAFT_PER_PEER_MAX=128
Environment=KISEKI_RAFT_CONN_POOL_PER_PEER=128
```

Add to `infra/gcp/scripts/setup-raw-storage.sh` and re-run. Expected
result: `raft_transport_rpc{op=append_entries}` drops from 129 ms to
sub-millisecond; `intent_fan_inner` and `chunk_fan_inner` collapse
toward the local-loopback floor; PUT ops/s lifts ~10×.

The same 16-cap is wrong **as a default** for any cluster with
shards × fan ≥ 16. Should also be raised in code: a more sensible
default is `RAFT_TRANSPORT_PER_PEER_MAX_DEFAULT = 256` (TCP
connections are cheap; the cap exists only to bound process-wide
file descriptor growth).

### 2. `chunk_persistent_write{phase=extent_io}` = **6.6 ms** on a 64 KiB write to local NVMe

`KISEKI_RAW_DEVICES` correctly points at the four `google-local-nvme-ssd-N`
block devices. A 64 KiB direct block write to an Intel/Google local
NVMe SSD should take 30–100 µs.

**Most likely**: this is a **Tokio runtime-starvation artifact
secondary to (1)**. When AppendEntries are jammed for 129 ms, the
tokio executor task queue backs up; futures that *cross an `await`
boundary measure wall time, not CPU time*. `extent_io` is
instrumented with `Instant::now()` before the async write call and
observed after — so the 6.6 ms is "elapsed wall-clock from
schedule-the-write to await returns", not "actual disk syscall
duration".

The way to validate this is to fix (1) and re-measure. If `extent_io`
drops to <100 µs on the re-run, the GCP "non-sensible disk" was
runtime distortion. If it remains in the millisecond range, we have
a real disk-path bug.

The `kiseki_fabric_put_recv{write_chunk} = 6.6 ms` matches
`extent_io` almost exactly (`write_chunk` = `decode + extent_io +
save_meta`, and `decode = 9 µs`, `save_meta = 7 µs`). So the entire
receiver-side write_chunk overshoot is the extent_io measurement.

### 3. `composition_record` umbrella histogram = 12 ms

Already known per #164 — this aggregates `gw.comp_create` (153 µs) +
`log.emit_delta` + `parallel_fan_wall` (12 186 µs). The 12 186 µs of
`parallel_fan_wall` is the
`try_join!(chunk_fan, intent_fan)` await, which max's between the
two. With `chunk_fan_inner = 11 303 µs` and `intent_fan_inner = 1 712 µs`
the join takes ~max ≈ chunk_fan, so the umbrella tracks the chunk
fan. Once (1) is fixed, all three of `composition_record`,
`parallel_fan_wall`, and `chunk_fan_inner` should collapse together.

## Bench errors

33 PUT errors / 44 340 = 0.07 % error rate. **All 33 are quorum-shortfall
WARNs caused by the per-peer cap rejection** above. Not a separate
correctness issue.

## Per-shard load distribution

Receiver-side `fabric_put_recv{write_chunk}` count = 221 865 across
6 nodes. With 44 340 PUTs from the bench and a 5-way fanout (1
leader + 2 followers for the data chunk + 2 followers for the EC
parity), expected total = 44 340 × 5 = 221 700. Within 0.1 % of
observed. Good.

Per-node fan distribution looked even — no hot-shard skew.

## Artefacts

In `/tmp/gcp-instr-2026-06-01/`:
- `put-heavy.json`, `get-heavy.json`, `mixed.json` — bench summaries
- `n{1..6}-{before-put,after-put,after-get,after-mixed}.txt` — /metrics snapshots
- `n{1..6}-journal.txt` — last 800 journal lines per storage node
- `n1-pprof.svg` — pprof on-CPU flamegraph from node 1 (graceful shutdown after mixed bench)
- `perphase-analysis.txt` — full sorted per-phase tables

## Next actions

1. **Land**: add `KISEKI_RAFT_PER_PEER_MAX=128` +
   `KISEKI_RAFT_CONN_POOL_PER_PEER=128` to
   `infra/gcp/scripts/setup-raw-storage.sh`. Raise the in-code
   default in `crates/kiseki-raft/src/tcp_transport.rs:84` from 16
   to 256 in a separate PR — anyone running with shards×fan ≥ 16
   today hits this silently.
2. **Re-run**: same instrumented binary, same workload, expect
   PUT ops/s to lift ~5–10× and per-phase histograms to drop into
   the same shape as the local 3-node loopback.
3. **If extent_io stays high after step 1**: that's a real disk-path
   bug separate from the Raft transport saturation. Investigate via
   `iostat`/`blktrace` on the local NVMe during the PUT bench.
