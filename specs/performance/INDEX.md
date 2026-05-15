# Performance snapshots

Time-series record of `kiseki-profile` matrix runs. Each entry is a
dated, immutable snapshot — written once, never edited (errata go in
follow-up entries). The canonical "what's the current number" view
lives in [`docs/performance/README.md`](../../docs/performance/README.md);
this directory is the long-term record so we can plot trajectory and
catch regressions.

## Convention

- Filename: `YYYY-MM-DD-<short-label>.md` (e.g. `2026-05-07-local-matrix.md`).
- Frontmatter:
  - HEAD commit (so we can re-run against the same code).
  - Hardware (machine class, CPU count, kernel).
  - Driver config (concurrency, object size, duration, warmup).
  - What changed since the previous snapshot (commit range or summary).
- Body: throughput table, p99 table, delta vs previous, findings.

## Snapshots

| Date | HEAD | Hardware | One-line takeaway |
|---|---|---|---|
| [2026-05-15 evening — compact PARTIAL](2026-05-15-gcp-compact-evening-partial.md) | main `defd8c3` (post-sweep) | 3 × c3-standard-44-lssd + 2 × c3-standard-44 (europe-west1-b) | Phase 4 wedged on composition hydrator backlog. iperf3 46.3 Gbps ✓, S3 PUT 1 KB p50 2.5 ms, S3 16∥ 726 MB/s, FUSE create() = EIO, NFS not measured. F-1 (hydrator) + F-2 (FUSE RO default) to file. |
| [2026-05-15 morning — compact](2026-05-15-gcp-compact.md) | `v2026.43.759` (`f6f6e5b`) | 3 × c3-standard-44-lssd + 2 × c3-standard-44 (europe-west1-b) | First post-libfuse-swap GCP run. NFSv4.2 1.71 GB/s aggregate, S3 PUT 673-1094 MB/s, S3 GET 1170 MB/s. Surfaced GH #36 (chunk-fill), #37 (FUSE O_DIRECT), #38 (EC-4+2 cap). |
| [2026-05-09 libfuse-swap](2026-05-09-libfuse-swap.md) | `527c2e6` (single) / `da45687` (multi) | dev workstation 16c | FUSE GET +25% on the multi-thread libfuse loop; NFSv4.1 read 0.5 MB/s → 923 MB/s after disabling pNFS layout advertisement on the 3-node compose. |
| [2026-05-07 post-pool](2026-05-07-post-pnfs-pool.md) | `5fc9523` | dev workstation 16c | pNFS GET unstuck via DS pool (17 k → 80 k); CI green for the first time post rust 1.95. |
| [2026-05-07 post-fix](2026-05-07-local-matrix.md) | `51c48aa` | dev workstation 16c | FUSE leapfrogs everything (52 k PUT / 115 k GET); NFS PUT regressed to 5 k. |
| [2026-05-05 ADR-042 native](2026-05-05-adr042-native-local.md) | Phase 7 of adr-042 plan | dev workstation 16c | First end-to-end native-binding measurement. A-NG11 gate at 15% — Phase 9 perf slice pending. |
| [2026-05-03 GCP transport](2026-05-03-gcp-transport.md) | (pre-fjall sweep) | 3 × c3-standard-88-lssd + 3 × c3-standard-44 (europe-west1-b) | First multi-node GCP run. Partial — surfaced fabric write quorum-loss bug (fixed in `f362060`). |
| [2026-05-03 local baseline](2026-05-03-local-baseline.md) | (pre-fjall sweep) | dev workstation 16c | Post-fix May matrix; NFSv4 GET 27 k, S3 GET 25 k, FUSE GET 10 k. The "May 2026 baseline" later snapshots delta against. |

## When to add a snapshot

Snapshot when *one or more* of:

- A perf-relevant change has landed (transport, lock, encoding, cache).
- A hardware target has changed (new GCP profile, new machine class).
- A previous snapshot is more than a month old and the code has moved.

Don't snapshot every commit — the value is the time-series, not the
point measurement. Aim for ≤2 per week even during heavy perf work;
batch the changes into one re-run.

## Cross-references

- Optimization backlog: [`docs/performance/optimization-backlog.md`](../../docs/performance/optimization-backlog.md)
- Profile matrix tooling: `crates/kiseki-profile/run-all.sh`
- A-NG11 gate definition: ADR-042 §14
