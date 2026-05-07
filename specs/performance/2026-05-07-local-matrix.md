# 2026-05-07 — local single-node matrix

| Field | Value |
|---|---|
| Date | 2026-05-07 |
| HEAD | `51c48aa` (perf(profile): default --binding to tcp) |
| Hardware | dev workstation, AMD Ryzen 7 6800H, 16 vCPU, Linux 7.0.3-arch1-2 |
| Cluster | single-node `kiseki-server` (ephemeral ports, plaintext) |
| Object size | 65 536 B |
| Concurrency | 16 |
| Duration | 30 s per shape |
| Warmup | 256 objects (get-heavy / mixed) |
| Tooling | `bash crates/kiseki-profile/run-all.sh` |
| Output dir | `/tmp/kiseki-prof/` |

## Why this snapshot

Two days and ~20 perf commits since the 2026-05-03 baseline:

- ADR-022 fjall sweep (rev-2/3/4) — three hot paths off redb/JSON.
- ADR-042 native gateway + TCP-framed binding (`315568e`, `73713c2`).
- FUSE wired through native TCP-framed binding (`29a6a35`),
  3-phase RwLock + composition_id fast-path (`6035bab`),
  KisekiFuse runtime detour bypass (`c10cc65`).
- NFS server async-native conversion (`bd56236`), client fh cache
  drop + LOOKUP fallback (`10297f4`), CREATE pending-fh fast-path
  (`c813e9a`).
- Mem-gateway: zero-copy GET on full-object fast path (`60b1032`).
- Composition: drop store-wide Mutex, shard per-name + per-id (`94af66d`).
- Raft: eventual durability for FjallLogStore via flush interval (`d5c56ad`).

The 2026-05-03 published matrix needed a refresh.

## Throughput (CPU phase, pprof-instrumented server)

| Protocol | put-heavy | get-heavy | mixed (70 P / 30 G) |
|---|---:|---:|---:|
| **S3 (HTTP)** | 42 160 op/s · 2 635 MiB/s | 75 078 op/s · 4 692 MiB/s | 47 584 op/s · 2 974 MiB/s |
| **NFSv3** | 5 006 op/s · 313 MiB/s | **107 830 op/s · 6 739 MiB/s** | 6 618 op/s · 414 MiB/s |
| **NFSv4.1** | 5 008 op/s · 313 MiB/s | 58 861 op/s · 3 679 MiB/s | 6 634 op/s · 415 MiB/s |
| **pNFS Flex Files** | 4 970 op/s · 311 MiB/s | 17 921 op/s · 1 120 MiB/s | 6 453 op/s · 403 MiB/s |
| **FUSE** | **52 888 op/s · 3 305 MiB/s** | **115 368 op/s · 7 210 MiB/s** | **61 230 op/s · 3 826 MiB/s** |

## Tail latency p99 (µs)

| Protocol | put-heavy | get-heavy | mixed |
|---|---:|---:|---:|
| S3 | 901 | 525 | 831 |
| NFSv3 | 12 652 | 410 | 11 889 |
| NFSv4.1 | 12 582 | 630 | 11 913 |
| pNFS | 12 476 | 1 177 | 13 308 |
| FUSE | 705 | 412 | 668 |

## p50 / p95 / p99 (µs) — full table

| Protocol | shape | p50 | p95 | p99 |
|---|---|---:|---:|---:|
| S3 | put-heavy | 321 | 625 | 901 |
| S3 | get-heavy | 194 | 365 | 525 |
| S3 | mixed | 288 | 571 | 831 |
| NFSv3 | put-heavy | 2 237 | 9 713 | 12 652 |
| NFSv3 | get-heavy | 127 | 293 | 410 |
| NFSv3 | mixed | 1 391 | 8 545 | 11 889 |
| NFSv4.1 | put-heavy | 2 198 | 9 628 | 12 582 |
| NFSv4.1 | get-heavy | 245 | 477 | 630 |
| NFSv4.1 | mixed | 1 369 | 8 494 | 11 913 |
| pNFS | put-heavy | 2 295 | 9 583 | 12 476 |
| pNFS | get-heavy | 870 | 1 017 | 1 177 |
| pNFS | mixed | 1 164 | 8 831 | 13 308 |
| FUSE | put-heavy | 242 | 499 | 705 |
| FUSE | get-heavy | 117 | 273 | 412 |
| FUSE | mixed | 218 | 459 | 668 |

## Delta vs 2026-05-03 baseline

| Protocol | PUT 2026-05-07 | PUT 2026-05-03 | Δ | GET 2026-05-07 | GET 2026-05-03 | Δ |
|---|---:|---:|---:|---:|---:|---:|
| S3 | 42 160 | 7 124 | **5.9×** | 75 078 | 25 843 | 2.9× |
| NFSv3 | 5 006 | 2 042 | 2.5× | 107 830 | 26 615 | **4.1×** |
| NFSv4.1 | 5 008 | 8 327 | **0.6× ↓** | 58 861 | 27 291 | 2.2× |
| pNFS | 4 970 | 8 327 | **0.6× ↓** | 17 921 | 16 549 | 1.1× |
| FUSE | 52 888 | 2 790 | **19×** | 115 368 | 10 789 | **10.7×** |

## A-NG11 gate (≥80 k GET, ≥56 k PUT per node)

| Protocol | PUT (gate ≥56 k) | GET (gate ≥80 k) |
|---|---|---|
| S3 | 42 160 — **75 % of gate** | 75 078 — **94 % of gate** |
| NFSv3 | 5 006 — 9 % of gate | 107 830 — **clears** |
| NFSv4.1 | 5 008 — 9 % of gate | 58 861 — 74 % of gate |
| pNFS | 4 970 — 9 % of gate | 17 921 — 22 % of gate |
| FUSE | 52 888 — 94 % of gate | 115 368 — **clears** |

FUSE GET / NFSv3 GET clear the gate. PUT still gated everywhere
on this single host. Native binding wasn't measured here — the
harness script doesn't include `--protocol native`.

## Findings

1. **FUSE is the fastest path on every shape** in this matrix.
   The TCP-framed wiring + 3-phase RwLock + KisekiFuse-detour
   bypass compounded into a 10–19× lift. For the first time the
   POSIX path beats S3 HTTP on every workload.
2. **NFSv3 GET is the throughput ceiling** at 107 830 op/s.
   NFSv4 GET (58 861) sits at 55 % of v3 — the v4 session
   machinery costs ~2× per op even after the async-native
   rewrite. ADR-032 trade-off cost is real and measurable.
3. **NFS-family PUT measured at ~5 k op/s, but it is run-time
   degradation, not a structural regression.** Investigation
   afterward (see addendum below) found the matrix's 30 s
   duration captures the *degraded* end of an O(N²) curve in
   `DirectoryIndex::name_for` (linear scan over all files in
   the namespace, called once per NFS COMMIT inside
   `flush_writes`). Standalone c=16 measurements:
   - 10 s: NFSv3 9 970 op/s · NFSv4 10 178 op/s · pNFS 10 286 op/s
   - 30 s: NFSv3 4 984 op/s
   - 60 s: NFSv3 3 394 op/s

   The May 3 baseline measured 8 327 op/s for NFSv4 PUT under
   the same 30 s degraded regime. The 8 327 → 5 008 delta
   shows the degradation rate worsened slightly between snapshots
   (likely fjall journal growth compounding with the O(N²)
   scan), but the steady-state ceiling at startup is uniform
   ~10 k op/s across v3 / v4 / pNFS — they all converge on
   the shared `name_for` bottleneck.
4. **pNFS GET stagnated** (16 549 → 17 921, +8 %). Every other
   GET path picked up the gateway-side wins; pNFS DS read path
   didn't. Likely a frame copy or sync mutex on the DS server
   that the metadata-stream NFS GETs shed.
5. **Mixed-shape latency p99 ≈ 12 ms** for NFS variants tracks
   the PUT regression — mixed is 70 % PUT so it inherits the
   write-path queueing.

## Captured profiles

- `/tmp/kiseki-prof/cpu-{protocol}-{shape}.svg` — pprof
  flamegraphs (15 SVGs, 200 KB – 1 MB each).
- `/tmp/kiseki-prof/heap-{protocol}-{shape}.json` — dhat heap
  records (15 JSONs, ~3.5 MB each; load via dh_view.html).

Note: the heap-phase op/s in the script log are dhat-instrumented
(typically 5–10× lower) and not throughput-representative — use
them only to attribute allocation cost.

## Open follow-ups (filed against `docs/performance/README.md` Open issues)

- DirectoryIndex `name_for` O(N²) fix (root cause of NFS PUT
  matrix degradation — see addendum below).
- pNFS GET DS-server-side investigation.
- Add `--protocol native` to `run-all.sh`.

---

## Addendum — root cause for NFS PUT matrix numbers (added same day)

After the snapshot landed, the NFS PUT "regression" was
investigated. Methodology:

1. Re-ran NFSv3 PUT c=16 standalone (10 s, no warmup) → **9 970
   op/s** (vs matrix's 5 006). Initial hypothesis "pprof
   instrumentation tax" disproven: c=16 with `KISEKI_PPROF_OUT`
   set still gave 9 876 op/s.
2. Concurrency sweep at 10 s duration:

   | c | throughput | per-op |
   |---:|---:|---:|
   | 1 | 5 425 op/s | 184 µs |
   | 2 | 7 096 op/s | 282 µs |
   | 4 | 9 388 op/s | 426 µs |
   | 8 | 10 122 op/s | 790 µs |
   | 16 | 9 970 op/s | 1.6 ms |
   | 32 | 10 201 op/s | 3.1 ms |

   Saturates at ~10 k op/s past c=4 — single-threaded server-side
   gate.
3. Duration sweep at c=16:

   | duration | throughput | per-op | files at end |
   |---:|---:|---:|---:|
   | 10 s | 9 970 op/s | 1.6 ms | ~100 k |
   | 30 s | 4 984 op/s | 3.2 ms | ~150 k |
   | 60 s | 3 394 op/s | 4.7 ms | ~200 k |

   Throughput halves as duration triples → cumulative cost
   scales O(N²) on file count.
4. Code search located `DirectoryIndex::name_for` (`crates/
   kiseki-gateway/src/nfs_dir.rs:66-73`) — a `dir.values()
   .find(|e| &e.file_handle == fh)` linear scan over every
   entry in the namespace, all 16 NFS connections serialized on
   the single `Mutex<HashMap>`. Called inside `flush_writes`
   (`crates/kiseki-gateway/src/nfs_ops.rs:411`) on every NFS
   COMMIT to re-map the dir entry to the new composition_id.
5. `name_for` was added 2026-05-01 in `40cac2b` (per-fh
   buffered writes + flush on COMMIT). Already present in the
   May 3 baseline — the 8 327 op/s May 3 NFSv4 number was also a
   degraded-state measurement.

### Implications

- The shared ~10 k op/s ceiling (4-32 conn) is a separate bottleneck
  from the O(N²) and is the next investigation target after the
  reverse-index fix.
- The matrix `--duration-secs 30` masks the steady-state behavior
  for write-heavy workloads. Snapshot interpretation note: NFS PUT
  matrix numbers are "average over 30 s of a O(N²)-degrading run."
  Real-world NFS workloads with file lifecycle (create, write,
  delete) won't hit this; benchmark workloads that only PUT will.

### Fix — landed same day

`crates/kiseki-gateway/src/nfs_dir.rs`: replaced the per-namespace
`HashMap<String, DirEntry>` with a `NamespaceDir { by_name,
by_handle }` pair. `name_for` is now an O(1) lookup in
`by_handle`. `insert` / `remove` / `rename` maintain both maps;
`insert` drops a stale fh→name back-edge if the same name is
re-pointed at a fresh fh (covers the flush-writes re-insert path).
4 new unit tests pin the reverse-index contract; all 12 nfs_dir
tests pass.

### Verification (same harness, post-fix)

NFSv3 PUT c=16, 64 KiB, no warmup — degradation eliminated:

| duration | pre-fix | post-fix | gain | post-fix p99 |
|---:|---:|---:|---:|---:|
| 10 s | 9 970 op/s | **45 730 op/s** | 4.6× | 849 µs |
| 30 s | 4 984 op/s | **43 648 op/s** | 8.8× | 843 µs |
| 60 s | 3 394 op/s | **41 687 op/s** | 12.3× | 858 µs |

NFSv4 / pNFS PUT c=16, 30 s:

| Protocol | pre-fix | post-fix | gain |
|---|---:|---:|---:|
| NFSv4 | 5 008 op/s | **51 876 op/s** | 10.4× |
| pNFS | 4 970 op/s | **51 702 op/s** | 10.4× |

NFSv3 GET regression check (c=16, 30 s, warmup=256):
post-fix 109 485 op/s vs pre-fix 107 830 op/s — within noise.

The "shared 10 k op/s ceiling" hypothesis from the addendum
was wrong: the ~10 k starting point was already mid-curve
because the dir index had ~5 k files in it after the first
half-second. The real steady-state ceiling on this hardware is
~45-52 k op/s for NFS PUT — competitive with FUSE (52 k) and
S3 (42 k). The next investigation target is the v3-vs-v4 gap
(45 k vs 52 k — v3 ~17 % slower despite being a simpler protocol).
