# Write-fanout validation — Step B baseline-vs-fanout delta

**Type**: Implementer → next-perf-run consumer (validation pending)
**Date**: 2026-05-15
**Author**: implementer (Step B of the write-routing posture plan)
**Status**: AWAITING GCP RE-RUN — code lands first, validation lands when the user re-runs the suite on the 2026-05-15 compact profile (cost-gated).
**Cross-refs**:
- Plan: `specs/implementation/write-routing-posture.md` Step B.
- Baseline snapshot: `specs/performance/2026-05-15-gcp-compact.md`.
- Targets: `docs/performance/targets.md` — compact-profile aggregate rows.
- Spec basis for the spread: ADR-033 §1 (initial shard topology) + §2 (leader placement).

## Problem

The 2026-05-15 GCP compact run reported `gateway_requests=1863 / 0 / 0`
across the three storage nodes. The cluster *can* spread writes across
shard leaders (ADR-033 §1 creates 9 shards per namespace; §2 spreads
their leaders best-effort round-robin), but `perf-suite.sh` phase 9 was
written before that spread existed and still aims every client's PUTs
at `$LEADER_S3`. The single ingest gateway forwards every request to
its own per-shard Raft clients — and even though Raft commits then
fan out to the three voters, the *ingest* node carries the gateway
CPU and the metric counts the gateway dispatch, not the Raft replica
work. Result: `gateway_requests` ≈ 0 on the two non-leader nodes.

This finding doc captures (a) what changed in the bench, (b) what the
acceptance criterion is, (c) what numbers we expect the next run to
land, and (d) the residual risks that the next run will surface.

## Change shape (Step B)

Two commits in this worktree:

- `test(perf): pick_storage_for_client / pick_namespace_for_client helpers (Step B-1)`
- `perf(bench): fan-out phase 9 across ingest gateways + add phase 9b (Step B-2)`

### Phase 9 (modified)
Same single namespace `perf-agg`. Each client's PUT loop now targets a
different storage node's S3 gateway, chosen by
`pick_storage_for_client(idx) = STORAGE_IPS_ARRAY[idx % N]`. The
gateway still routes by hashed-key to the owning shard leader, so write
quorum-commit happens at the shard leader regardless of ingest gateway
— this isolates the "ingest gateway is hot" vs. "shard leader is hot"
axes. Expected: gateway_requests visible on **all** ingest nodes; the
**per-shard-leader** spread (or lack of it) remains the variable for
phase 9b to expose.

### Phase 9b (new)
Same total payload (3 × 100 × 1 MB), but each client *also* writes to
its own namespace (`perf-agg-ns0`, `perf-agg-ns1`, `perf-agg-ns2`) on
its own ingest gateway. Per ADR-033 §1, each namespace creates 9
shards with leaders spread round-robin across the three nodes, and
the namespaces are independent — so the union of "9 leaders × 3
namespaces" spans 27 shard-leader slots, distributed best-effort
across 3 nodes ≈ 9 leaders/node. With key-hash-uniform PUTs that
should fully saturate the leader-spread axis. This is the *upper
bound* of what bench-only changes can do; further lift requires
Steps A (server proxy) and C (client leader hints).

### Phase 10 (annotated)
Now logs an explicit `Fan-out OK | SKEWED | FAIL` verdict line
against the plan's max/min ≤ 4:1 threshold. Verdict is informational
only — the suite is a measurement tool, not a CI gate; an operator
reads the line to decide whether the run is comparable to prior phase-9
runs.

## Baseline (2026-05-15, pre-Step-B)

From `specs/performance/2026-05-15-gcp-compact.md` and the captured
metrics snapshot in `infra/gcp/benchmarks/results/`:

| Metric | Value |
|---|---:|
| S3 parallel write (phase 9, 2 clients × 100 × 1 MB) | **528 MB/s aggregate** |
| `gateway_requests_total` across 3 nodes | **1863 / 0 / 0** |
| max/min ratio | **∞** (two zeros) |
| Per-node ceiling (NIC × 90% multi-stream) | 5.75 GB/s |
| Per-node target (single client, 64 MB PUT) | 4 GB/s |

## Expected after Step B (next run)

Per `docs/performance/targets.md` row "S3 PUT aggregate 2 clients" on
compact: `7 GB/s ÷ R-3 = 2.3 GB/s`. With three clients (the existing
phase-9 client count) and proper fan-out, we expect to land in the
**1.3-1.9 GB/s aggregate** band — 2.5×–3.6× the 528 MB/s baseline.

| Expected metric (phase 9) | Target band |
|---|---:|
| Aggregate MB/s | **1300–1900 MB/s** (matches the targets-doc compact row, ÷ R-3) |
| gateway_requests on each node | non-zero |
| max/min ratio | **≤ 4.0** (plan threshold) |

| Expected metric (phase 9b — per-ns + per-gw) | Target band |
|---|---:|
| Aggregate MB/s | **≥ phase 9** (or within noise) |
| max/min ratio | **closer to 1.0** than phase 9 (more shard-leader spread) |

Phase 9b should be the **tighter ratio**, not necessarily the higher
throughput — the per-node aggregate is still capped by the same NIC,
storage, and crypto budget. The win is in *measurement honesty*: phase
9 tests "gateway-side fan-out" and phase 9b tests "gateway + shard-
leader fan-out", and the delta between them is a number we currently
have no way to measure.

If the post-Step-B aggregate stays below 1 GB/s, **Step B did not
remove the bottleneck** and the gap is elsewhere — likely
`LeaderUnavailable` retries (Step C addresses) or per-call gRPC tax
(separate slice). The metrics snapshot tells the story:

- gateway_requests roughly equal on all nodes + low throughput =
  fan-out works, but each gateway is CPU-bound or back-pressured.
- gateway_requests still 1:0:0-ish + low throughput = a bug in the
  helpers, the env, or in S3 bucket creation (per-namespace PUT
  returned an error and subsequent object PUTs all 404'd).
- gateway_requests roughly equal + high throughput = success, Step B
  delivered the expected lift.

## Acceptance

Per the plan:

> Post-run `metrics-snapshot.txt` shows `gateway_requests > 0` on all
> three nodes, ratio no worse than 4:1 between max and min.

Validation in this worktree:

1. **Unit test of `pick_storage_for_client(idx)` against a 3-element
   array** — `infra/gcp/benchmarks/tests/test_pick_helpers.sh`, 17/17
   passing locally. Also covers 6-element (default profile, post GH
   #38) and 1-element (degenerate single-node dev) shapes.
2. **Static inspection of the phase 9 / 9b diff** — see commits in
   this branch. The loop iterates `idx in 0 1 2`, each `idx` resolves
   to `STORAGE_IPS_ARRAY[idx % N]` via the unit-tested helper. Phase
   9b additionally hits `perf-agg-ns(idx % N)`. No further bash unit
   test is meaningful — the bench *is* its own integration test on
   GCP.
3. **GCP re-run** — user-driven (cost-gated). The next `run-perf.sh
   compact` will write a `metrics-snapshot.txt` whose three lines
   either satisfy the acceptance criterion or surface one of the
   diagnostic shapes above.

## Why no BDD scenario

`specs/features/` scenarios wire to Rust step impls in
`kiseki-acceptance`. The bench harness is bash and runs against
either a live GCP cluster or docker-compose e2e — it has no surface
for a Gherkin scenario to drive end-to-end without spending money on
the cluster or duplicating the perf-suite logic in Python. The
scenario-level assertion that the plan calls for ("metrics-snapshot
post-run shows non-zero on all three nodes") is the
`Fan-out OK | SKEWED | FAIL` verdict line added to phase 10.

The existing BDD coverage for the underlying mechanism is in
`specs/features/cluster-formation.feature:106-131`:

- *Namespace creation produces 3x node_count shards by default* —
  asserts `initial_shards = 9` on a 3-node cluster.
- *Leader placement* — "each shard's leader is placed on a distinct
  node where possible".

If those scenarios start failing, phase 9b's spread will degrade
gracefully (fewer distinct leaders → tighter max/min ratio but lower
aggregate) — they are **the upstream invariants Step B trusts**, not
something Step B has to re-prove at the bench level.

## Residual gaps

(Things the next perf run is likely to surface that this finding
doesn't pre-empt.)

1. **R-3 vs R-? in `perf-agg-ns0..2`** — Step B implicitly assumes the
   bench-managed namespaces are created with the same replication
   policy as `perf-agg`. The S3 PUT `/<bucket>` path on the gateway
   uses the cluster-default policy; if that default is not R-3, the
   targets-doc row's `÷ R-3` denominator is wrong for phase 9b and the
   expected band shifts. Verify on the next run by reading the
   per-namespace policy from `/cluster/info`.

2. **Per-namespace shard creation atomicity (ADV-033-1)** — three
   `PUT /<bucket>` calls land at roughly the same wall-clock moment.
   Each creates `initial_shards = 9` Raft groups. On a 3-node cluster
   that's 27 concurrent Raft-group formations against the control
   plane. The `Creating` state guard in ADR-033 §1 protects against
   partial-key-range coverage *within a namespace*, but the
   control-plane Raft is the serialization point. If the bench
   sometimes flakes on bucket creation under this concurrency, that's
   a real Step-B-surfaced finding and should be filed against ADR-033.

3. **gateway_requests is a coarse signal** — counts every request
   landing at the S3 gateway, including buckets create-attempts and
   reads. The 4:1 ratio could mask "phase 9 spread well, phase 9b
   piled everything on one ingest". The next iteration should split
   the snapshot into per-bucket counts (label dimensions exist on
   `kiseki_gateway_requests_total` per ADR-021 observability).

4. **No teardown between runs** — `perf-agg-ns0..2` accumulate. On
   the next run they already exist, which is *fine* (S3 PUT bucket is
   idempotent), but compounded across many runs the chunk-store fill
   measured in GH #36 will arrive sooner. Add an explicit cleanup
   phase to a future iteration.

## Self-adversary pass (what an adversary would raise as gate-1)

Findings I would flag if I were reviewing this work as an adversary
before declaring done:

1. **F-A1 (Medium): test wraparound only covers idx=3..5; no
   idx=N*1000 stress** — the modulo helper is trivial so the depth
   is bounded, but if someone refactors to a stateful counter (say,
   to track per-client byte rates) the lack of large-idx coverage
   misses an off-by-one. Action: not blocking; revisit if the helper
   gains state.

2. **F-A2 (Medium): phase 9b assumes `PUT /<bucket>` succeeds; on
   failure all 100 PUTs silently 404** — the current code does
   `curl -sf -X PUT ... || true` and continues. If bucket creation
   fails, the bench reports "0 bytes/s, gateway_requests on this
   node mostly 404s" — not "namespace creation failed: <reason>".
   Action: log the `PUT /<bucket>` HTTP status explicitly in the
   next iteration; the operator should see "Created ns ... (200)"
   or the explicit error.

3. **F-A3 (Low): phase 9 still uses the legacy `perf-agg` bucket
   name; if a prior run created `perf-agg` on the *old* leader and
   the new run hits a *different* leader, S3 may surface as
   NoSuchBucket on the second host** — buckets are global across
   the cluster (the namespace registry is in the control-plane
   Raft), so this is fine in practice, but worth verifying on the
   first re-run.

4. **F-A4 (Low): the Fan-out verdict's python3 invocation runs even
   when the metrics curl fails silently — if `gateway_requests=0`
   on all three nodes (e.g. metric-name typo upstream), the verdict
   reads "Fan-out FAIL: at least one storage node has
   gateway_requests=0" which is technically correct but
   misleading.** Action: a future iteration could distinguish "0
   requests" from "metric not exposed" by checking for the line at
   all in `/metrics` before summing.

5. **F-A5 (Low): the 4:1 ratio threshold is somewhat arbitrary** —
   the plan picked it as a workable ceiling, not as a derived
   invariant. After the first GCP re-run we'll have a real number
   for what "spread well" looks like and can tighten this. Action:
   capture the actual ratio in the post-run snapshot and update the
   verdict threshold if 4:1 is too lenient.

None of the above is gate-1 blocking; they're hygiene items for the
post-run iteration.
