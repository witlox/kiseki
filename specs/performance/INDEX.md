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
| [2026-05-07](2026-05-07-local-matrix.md) | `51c48aa` | dev workstation 16c | FUSE leapfrogs everything (52 k PUT / 115 k GET); NFS PUT regressed to 5 k. |
| [2026-05-03](../../docs/performance/README.md#local-single-node-matrix) | (pre-fjall sweep) | dev workstation 16c | Post-fix May matrix; NFSv4 GET 27 k, S3 GET 25 k, FUSE GET 10 k. (Lives in docs/performance/README.md as the "May 2026 perf-fix" baseline.) |

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
