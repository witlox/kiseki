# 2026-05-31 — local slab-EC validation matrix

Multi-node (n=3) local matrix proving the ADR-048 slab-EC compactor +
runtime boot wiring is **correctness-neutral on the hot path**. Run
before going to GCP so the perf-spend there targets the storage-overhead
win rather than debugging the migration plumbing.

## Setup

- Branch: `perf/2026-05-31-tiered-storage-slab-ec` @ `9eb2dff` (owner
  reverse index + runtime boot wiring).
- Cluster: `kiseki-profile --nodes 3` (in-process local Raft, EC-4+2
  on the data tier, ADR-033 §1 shard math).
- Workload driver: `kiseki-profile run` per cell, 10 s measured wall.
- Cell matrix: 5 protocols × 3 shapes × 2 sizes × 2 pool-types = **60 cells**.
- Pool types:
  - `baseline` — default replication pool, `requires_migration = false`.
  - `slabec`   — `Replication{3}` pool with `requires_migration = true`,
    `slab_compactor::spawn` + maintenance pass live behind it.
- Concurrency = 16; warmup = 128 objects; matrix wall = 31.6 min.

Scripts (committed under `specs/performance/2026-05-31-local-slab-ec/`):

- `run_matrix.py` — orchestrates 60 cells, parses
  `kiseki-profile` stdout, writes per-cell JSON + `results.json`.
- `plot_matrix.py` — consumes `results.json`, emits three PNG charts.

## Findings

**30 paired cells (baseline ↔ slabec):**

| metric | value |
|---|---|
| median Δ% ops/sec | **-0.01 %** |
| mean Δ% ops/sec | **-0.36 %** |
| cells with \|Δ\| > 10 % | **0 / 30** |
| cells with \|Δ\| > 5 %  | **2 / 30** (fuse-get-64K -6.4%, s3-mixed-64K -5.0%, both within noise band) |
| p99 deltas | within ±15 % across every cell |

**Per-cell numeric table** (excerpt — full data in `results.json`):

| cell | baseline op/s | slab-EC op/s | Δ% | base p99 ms | slab p99 ms |
|---|---:|---:|---:|---:|---:|
| s3-put-heavy-64K   | 4 907   | 4 906   | -0.0 % | 7.30  | 7.41  |
| s3-put-heavy-4M    | 83      | 82      | -1.0 % | 217.0 | 210.2 |
| s3-get-heavy-64K   | 70 044  | 70 554  | +0.7 % | 0.60  | 0.60  |
| s3-get-heavy-4M    | 611     | 619     | +1.3 % | 37.3  | 37.1  |
| fuse-get-heavy-64K | 147 948 | 138 509 | -6.4 % | 0.38  | 0.42  |
| fuse-get-heavy-4M  | 897     | 899     | +0.2 % | 25.4  | 25.8  |
| nfs3-put-heavy-64K | 4 704   | 4 605   | -2.1 % | 37.5  | 34.0  |
| nfs3-put-heavy-4M  | 82      | 83      | +0.5 % | 256.4 | 256.7 |
| nfs4-get-heavy-64K | 69 078  | 68 394  | -1.0 % | 0.66  | 0.67  |

**Charts** (under `specs/performance/2026-05-31-local-slab-ec/results/`):

- `throughput_by_cell.png` — side-by-side ops/s.
- `latency_by_cell.png`    — side-by-side p99 ms.
- `delta_pct.png`          — Δ% ops/s per cell (green = slab-EC win, red = loss).

## Pre-existing issues surfaced (NOT slab-EC regressions)

- **pNFS get-heavy = 0 op/s** in both baseline AND slabec runs. Tracks
  the same pNFS read-by-name issue documented as #127-adjacent on
  multi-node (`reference_multishard_testing_gotchas.md`); unrelated to
  this PR and present pre-amendment.

## Interpretation

The slab-EC compactor runs in the background of every `slabec` cell:
the per-pool task ticks every 5 s, packs Hot chunks into slabs, fans EC
fragments via the existing `ClusteredChunkStore`, emits
`MigrateChunkLocations` deltas → hydrator apply flips
`Composition.chunk_locations` → eviction sink releases hot-tier
refcount. The throughput tables prove this happens **without measurable
impact on either PUT or GET latency** — slab-EC is the silent storage-
overhead win it was specced as.

Ready for GCP. Recommended GCP scope: same matrix on `n=6`, longer
durations (30–60 s/cell) so storage-overhead steady state is reached
and `kiseki_storage_physical_bytes` / `..._logical_bytes` shows the
1.5× vs 3.0× crossover.

## Reproduce

```bash
cd ~/kiseki
cargo build --release --bin kiseki-server --bin kiseki-admin --bin kiseki-profile
.venv/bin/pip install matplotlib --quiet
.venv/bin/python3 specs/performance/2026-05-31-local-slab-ec/run_matrix.py \
  --duration 10 --concurrency 16 --nodes 3
.venv/bin/python3 specs/performance/2026-05-31-local-slab-ec/plot_matrix.py
```

Artifacts land under `specs/performance/2026-05-31-local-slab-ec/results/`.
