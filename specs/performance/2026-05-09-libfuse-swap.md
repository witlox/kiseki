# 2026-05-09 — libfuse-swap snapshot

**HEAD:** `527c2e6` (single-node) / `da45687` (multi-node).
**Hardware:** dev workstation 16c (single-node) + 3-node docker compose on the same host (multi-node).
**Driver:** `kiseki-profile` matrix for single-node (5 protocols × 3 shapes, c=16, 64 KiB, 30 s, warmup=256, CPU phase via pprof) + `tests/e2e/test_perf_baseline.py` for multi-node (fio `bs=1M size=8M runtime=10s time_based direct=0` against `docker-compose.3node.yml`).
**What changed since previous snapshot:**

- ADR-043: `fuser` 0.17 → libfuse 3.x via `kiseki-fuse-sys` + `kiseki-fuse` safe wrapper. Phase 0+1a+1b+2 collapsed into commits `7bdfdbf` / `570a227` / `2b7fa0c` / `7c25a27`.
- ABI portability: `fuse_session_new@@FUSE_3.0` (commit `4129aa7`), Rocky 9 SONAME pin + bindgen blocklist (commit `527c2e6`).
- pNFS layout regression caught by post-swap perf run (unrelated to libfuse swap; same kernel client path) — fixed `da45687` by setting `KISEKI_DISABLE_PNFS_LAYOUT=1` on the 3-node compose.

## Single-node — 2026-05-09 post-libfuse-swap

Same configuration as 2026-05-07-post-pool. Only FUSE was affected by the swap; other rows are within noise of the prior snapshot and not re-measured here.

### FUSE delta

| Shape | Pre-swap (2026-05-07) | Post-swap | Δ |
|---|---:|---:|---:|
| put-heavy | 52,888 op/s · 3,305 MiB/s | 54,504 op/s · 3,406 MiB/s | **+3.1%** |
| get-heavy | 115,368 op/s · 7,210 MiB/s | **144,552 op/s · 9,034 MiB/s** | **+25%** |
| mixed | 61,230 op/s · 3,826 MiB/s | 65,596 op/s · 4,099 MiB/s | **+7%** |

### FUSE p99 latency (µs)

| Shape | p50 | p95 | p99 |
|---|---:|---:|---:|
| put-heavy | 235 | 483 | 683 |
| get-heavy | 94 | 208 | 343 |
| mixed | 200 | 428 | 624 |

### Finding — multi-thread session loop unblocked GET

The +25% lift on get-heavy is the durable libfuse-swap win. fuser's single-thread inline dispatch was the structural ceiling on concurrent FUSE reads even though `KisekiFuse::read` is `&self` and the `RwLock<KisekiFuse>` allows shared access. libfuse's `fuse_session_loop_mt_31` lets multiple FUSE READ ops dispatch in parallel; the gateway already serves them concurrently, the kernel-side throttle was fuser's inline loop.

put-heavy and mixed gain less because the 3-phase write-lock pattern (Bug 8/9 GCP 2026-05-04 fix) still serializes `flush_apply_response` through the write-lock — that's the bottleneck on the write path, not the session loop.

## Multi-node — 2026-05-09 (3-node Raft compose)

`tests/e2e/test_perf_baseline.py` against `docker-compose.3node.yml`. fio `bs=1M`, `size=8M`, `runtime=10s`, `time_based`, **`direct=0`**.

### Throughput

| Protocol | seq-read | seq-write |
|---|---:|---:|
| S3 (direct HTTP) | 240 MB/s (PUT 123 MB/s) | — |
| NFSv3 | 2,265 MB/s | 134 MB/s |
| NFSv4.1 (post `da45687`) | **923 MB/s** | 1,644 MB/s |
| FUSE (Rocky 9 image, post-swap) | 351 MB/s | 1,955 MB/s |

### Finding — NFSv4.1 read 0.5 MB/s pre-fix

Server advertises `LAYOUT4_FLEX_FILES` via `FATTR4_FS_LAYOUT_TYPES`. Kernel NFSv4.1 client picks pNFS by default, issues `LAYOUTGET` per OPEN, hits the per-file DS-session-establishment tax (EXCHANGE_ID + CREATE_SESSION + RECLAIM_COMPLETE on every layout, torn down on CLOSE). Each 1 MiB read takes 5+ seconds wall-clock (regardless of `bs` size); `dd if=… bs=1M count=1` reports 1.7 ms internally but the close(2) blocks 5 s on layout machinery.

The env-var gate `KISEKI_DISABLE_PNFS_LAYOUT=1` (existing since Phase 15a, see `nfs4_server.rs:1376-1393`) makes the server return `NFS4ERR_LAYOUTUNAVAILABLE` on `LAYOUTGET`. Kernel falls back to MDS inline reads — same code path NFSv4 inline READ uses. Compose now sets the env var (`da45687`); BDD pNFS scenarios use their own compose without it to exercise the layout path intentionally.

DS WRITE support is the structural fix; tracked separately.

### Finding — Rocky 9 release strategy

The libfuse swap exposed a release-strategy gap: libfuse 3.17 bumped the SONAME from `libfuse3.so.3` to `libfuse3.so.4`. A binary built against the dev-box's libfuse 3.18 (Arch) won't load on Rocky 9 / Ubuntu 24.04 / Debian bookworm (all `.so.3`). `tests/e2e/Dockerfile.fuse-client` is now a multi-stage build on `rockylinux:9` mirroring `.gcp-build/build.sh` — release artifacts MUST come from a Rocky 9 build container. Documented in `specs/implementation/libfuse-swap.md` §"Release-strategy implication".

### Methodology note — fio `--direct=0`

`direct=0` lets writes return as soon as they hit the kernel page cache; the actual gateway commit happens during writeback after the test ends. This makes write numbers look 5-15× faster than they "should" and produces an asymmetric shape (writes faster than reads on FUSE/NFS in this matrix) that disappears with `direct=1` (the GCP perf-suite default). The 0.5 MB/s NFSv4.1 read regression was a real bug; the FUSE/NFS write/read asymmetries elsewhere are measurement artifacts.

## Cross-references

- `specs/architecture/adr/043-system-library-ffi.md` — the ADR.
- `specs/implementation/libfuse-swap.md` — implementation plan (status: complete).
- `specs/escalations/2026-05-09-libfuse-syncfs-not-in-318-release.md` — Option A acceptance.
- `specs/escalations/2026-05-09-adr-013-ops-pending-data-plane.md` — ADR-013 ops awaiting data-plane.
- Commits: `570a227` `2b7fa0c` `7c25a27` `4129aa7` `527c2e6` `da45687`.
