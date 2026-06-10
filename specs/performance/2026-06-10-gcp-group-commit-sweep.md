# 2026-06-10 GCP run — first honest saturation sweep, group-commit arm

6-node `default` profile (6 × c3-standard-22-lssd + 3 clients,
europe-west1-b), **profile binary** (`hot-path-trace,pprof`) off main
@ `d899596` (post-PR #217 group-commit defaults + post-PR #219 bench
fixes). First run with the dedup-proof bench (run-nonce seeding),
warm-up discipline, endpoint spread, error halts, arm control, and
the `--connections` sweep axis — i.e. the first sweep whose numbers
are trustworthy per `specs/findings/2026-06-10-bench-correctness-review.md`.

## Setup (all gates passed)

- Binary identity verified: deployed `kiseki-server` sha matches the
  local profile tarball (`9127836b…` / tarball `00cac599…`).
- Arm B (group commit, the new default): boot logs show
  `small object store: … group_commit=true flush_interval_ms=100` and
  `intent store: group commit … interval_ms=100` on every node.
- `KISEKI_INLINE_THRESHOLD_RECOMPUTE_S=0` pinned via the new
  `/etc/kiseki/perf-arm.env` — and necessarily so: on first boot the
  recompute fired at exactly T+60 s and bumped the bootstrap shard
  4096 → 65536 (F11 confirmed live; PR #219's `=0` gate verified
  working after the pin).
- `00-health`: 7/7 nodes, 750 GiB/node chunk pool (#115 clear),
  leader agreement, arm recorded into results.
- `setup-shards`: 18 bench shards, leaders distributed across all 6
  nodes.

## PUT saturation sweep (4 KiB, put-heavy, 3 clients × 30 s, warm)

Aggregate op/s (3 clients, each → distinct storage endpoint):

| | conn=1 | conn=4 | conn=8 |
|---|---:|---:|---:|
| **conc 64** | **12 684** · p99 357 ms | 7 260 | 7 335 |
| **conc 128** | 12 274 · p99 632 ms | 8 208 | 7 244 |
| **conc 256** | 10 340 · p99 941 ms | 8 386 | 8 094 |

Cold throwaway (first run after boot, conc64/conn1): 14 479 op/s.
Zero errors in every cell (the new `--max-error-rate` gate never
fired).

### Findings

1. **The ceiling moved: ~6 k → ~12.7 k op/s warm (2.1×), 14.5 k
   cold.** The 2026-06-04 pre-#217 measurement was 6 054 op/s at
   conc16/conn1 (Little's-law-bound) and 9 873 at conn16. This sweep
   *saturates* — throughput falls and p99 grows with depth — so
   12.7 k is a genuine architecture ceiling for this arm, not an
   under-driven floor.
2. **One multiplexed connection beats pools**: conn=4/8 cost ~40 %
   throughput vs conn=1 at every depth. The slice-4 request_id demux
   is the efficient shape; extra sockets just spread the same
   in-flight over more per-connection overhead.
3. **The next binder is named and measured**: the new
   `inline_store_put` phase histogram (PR #219) recorded
   1 181 293 puts · mean **5.3 ms** on storage-1 under saturation —
   with fsync OFF. That is fjall journal-mutex queueing on the ONE
   shared SmallObjectStore Database (gateway ingress + every local
   shard applier). Lever shape: shard the small-object store into
   N Databases, or drop the gateway-side ack-path put entirely
   (the intent already carries the bytes; SM apply materializes).
4. Per-client fairness was good (within ~5 % at conn=1).

## Strict arm: NOT measured — blocked by a discovered bug (#220)

Swapping the arm (`KISEKI_SMALL_OBJECT_FLUSH_INTERVAL_MS=0` +
`KISEKI_INTENT_FLUSH_INTERVAL_MS=0` via perf-arm.env, rolling
restart) wedged the cluster at 2/7: after ~3.5 M inline objects, the
SM snapshot (which embeds the full delta history + inline
ciphertexts as one serde_json blob) is **~540 MB**, exceeding the
Raft transport frame cap — snapshot install rejected in a loop,
re-convergence impossible. Filed as **#220** with journal evidence;
a simultaneous-restart recovery attempt also failed. The strict-arm
comparison needs a fresh cluster after #220 is fixed (or arms run
strict-first on virgin state).

The boot-log lines did confirm the strict arm engages correctly:
`intent store: strict per-write fsync (KISEKI_INTENT_FLUSH_INTERVAL_MS=0)`.

## Artifacts

- `pprof-out/gcp-2026-06-10/kiseki-perf-default-20260610-120910/` —
  9 cells × 3 per-client JSONs (cell-labeled, with run_nonce /
  client_id / connections provenance), 00-health (arm recorded),
  90-metrics per-node snapshots.
- `pprof-out/gcp-2026-06-10/strict-arm-boot-storage-{1,2}.log` —
  #220 evidence.
- Cluster destroyed after collection (24 resources).

## Follow-ups

1. **#220** snapshot transfer (blocker for any restart-under-data,
   not just A/Bs).
2. Small-object journal contention (the 5.3 ms `inline_store_put`):
   shard the store or remove the ack-path put — candidate next
   lever toward 30 k+.
3. Strict-arm A/B leg on a fresh cluster (cheap: strict-first on
   virgin state, then flip to group-commit — avoids #220 entirely).
4. p99 tail (357 ms at conc64) — wave-shaped queueing worth a
   histogram decomposition on the next instrumented pass.
