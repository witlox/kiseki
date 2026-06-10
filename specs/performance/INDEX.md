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
| [2026-06-10 — P1/P2/P3 validation (PR #224)](2026-06-10-gcp-p123-validation.md) | main `41b23b5` | 6 × c3-standard-22-lssd + 3 × c3-standard-22 (europe-west1-b) | **PUT ceiling 22.3k op/s (1.76× same-day pre-#224; cold ≡ warm — decay GONE; p99 357→132 ms), GET 296.8k (+2.3%, constraint met) — but the ≥48k bar NOT met.** Ack wall is now entirely the intent fan: `pif.total` 12.0 ms + `composition_record` 13.5 ms, everything P1/P2/P3 targeted collapsed 3-4 orders (`inline_cache_insert` 7.3 µs, `sm.append_delta_inner` 0.43 µs). Next: pif sub-span decomposition + composition store. |
| [2026-06-10 — GCP group-commit saturation sweep](2026-06-10-gcp-group-commit-sweep.md) | main `d899596` (post-#217 + #219) | 6 × c3-standard-22-lssd + 3 × c3-standard-22 (europe-west1-b) | **First honest saturation matrix (dedup-proof bench): PUT ceiling 12.7 k op/s warm / 14.5 k cold @ conc64/conn1 — 2.1× the pre-#217 baseline; a real ceiling (throughput falls, p99 grows with depth), not under-driving.** conn=4/8 pools LOSE ~40 % vs one multiplexed conn. Next binder measured by name: `inline_store_put` mean 5.3 ms with fsync OFF (shared fjall journal mutex). Strict-arm A/B leg blocked by discovered **#220** (SM snapshot ~540 MB > Raft frame cap → cluster can't reconverge after restart under inline-heavy data). F11 threshold-recompute confirmed live + pinned via perf-arm.env. |
| [2026-06-04 — GCP slice-4 at realistic mount shape](2026-06-04-gcp-slice4-realistic.md) | main `808d0ac` | 6 × c3-standard-22-lssd + 3 × c3-standard-22 (europe-west1-b) | **Slice-4 multiplex validated on GCP: conn=1 now sustains 61 % of conn=16 PUT (was 31 %)** — 6,054 op/s @ conn1/conc16 vs 9,873 @ conn16. Recomputed against the physics-derived ceiling for the synchronous-commit architecture: PUT was AT ceiling (4.8–19.2 k band) at that shape; the idealised "35× short" framing was misleading. (Numbers predate the dedup-proof bench of #219 — treat as shape-comparison, not absolute.) |
| [2026-06-03 — GCP post-#195/#197/#198/#201 stack](2026-06-03-gcp-perf-stack.md) | main `9776fd0` | 6 × c3-standard-22-lssd + 3 × c3-standard-22 (europe-west1-b) | **+13 % PUT / +17 % GET vs same-shape 2026-05-29 baseline.** Native 4 KiB / conc=16 / 3 clients: PUT 10.3 k op/s · GET 433 k · Mixed 11.4 k. CPU bottleneck moved off serde (#194/#195 serde_bytes + #197 postcard Envelope + #199 LRU on FjallRaftLogStore) onto fjall I/O / lsm_tree write path — the desired shape. Next levers visible: `ViewStore::get_view` (62.8 %), `tcp_transport::decode_request_body` (75.8 % — wire-receive site, distinct from #199's log-store decode), and #200 composition coalescer. |
| [2026-05-31 — local slab-EC validation](2026-05-31-local-slab-ec.md) | `perf/2026-05-31-tiered-storage-slab-ec` @ `9eb2dff` | dev workstation 16c | **ADR-048 slab-EC correctness-neutral on the hot path.** 60 cells (5 protocols × 3 shapes × 2 sizes × 2 pool types). Median Δ% baseline ↔ slabec = **-0.01 %**; 0/30 paired cells with \|Δ\| > 10 %. Compactor migrates Hot → Cold in the background without measurable throughput / p99 impact. Ready for GCP. |
| [2026-05-29 — GCP post-#137 matrix](2026-05-29-gcp-137-matrix.md) | `perf/2026-05-28-roadmap` (#137) | 6 × c3-standard-22-lssd + 3 × c3-standard-22 (europe-west1-b) | **#137 chunk-store write parallelism VALIDATED: native put-heavy 6.3× (253→1,595 op/s), mixed 7.0× (305→2,151), 0 err; `write_chunk` flat under concurrency (was 56 ms plateau).** get-heavy 30.2k op/s · 1.97 GB/s, S3 PUT 1,378 / cross-node GET 1,845 (real 200s). Separate (non-#137): NFS read wedges (D-state), FUSE O_DIRECT (#37), native conc=48 single-client limit. |
| [2026-05-28 — GCP full matrix (PR #116)](2026-05-28-gcp-matrix.md) | `feat/115-capacity-tiering` (PR #116) | 6 × c3-standard-22-lssd (1.5 TB NVMe ea) + 3 × c3-standard-22 (europe-west1-b) | **#124 FUSE connect-timeout fix VERIFIED LIVE — mount attaches (was a hang).** native get-heavy **22.7k op/s** (2× prior, 0 err), S3 PUT+cross-node GET byte-verified. **But FUSE+NFSv4+pNFS read-by-name returns 0 bytes (#127, POSIX name→composition resolution broken on multi-node — data persists, name index doesn't), NFSv3 mount fails (#128).** native/S3 (by id/key) bypass the name index and work. dedup 6.03×. |
| [2026-05-27 — GCP capacity (PR #116)](2026-05-27-gcp-capacity.md) | `feat/115-capacity-tiering` (PR #116) | 6 × c3-standard-22-lssd (1.5 TB NVMe ea) + 3 × c3-standard-22 (europe-west1-b) | **GH #115 fixed & verified live: 1.5 TB/node (not 4 GiB).** Capacity+dedup observable (1.54×); multi-shard S3 0-err. Native parallel 3-client: get **10.6 k op/s / 662 MiB/s**, put 253 op/s (commit-bound). NFSv4 write 6.9 MB/s. FUSE mount hang + write commit-bound + forward-to-leader WARN-spam → findings. |
| [2026-05-16 local matrix](2026-05-16-local-matrix.md) | `162c55e` + uncommitted READLINK / LOCK / CREATE_SESSION wire fixes | dev workstation 16c | NFS PUT ceiling lifted ~9× (5 k → 42–49 k) — `name_for` O(N²) hot path is gone. pNFS GET recovered 4.3× (17 k → 77 k). Native TCP 55 k PUT / 147 k GET — **A-NG11 PUT gate at 99 %, GET clears 1.84×**. gRPC binding ~2–3× slower. Zero functional breaks across all 21 combos. |
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
