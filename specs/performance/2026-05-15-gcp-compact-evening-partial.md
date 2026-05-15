# 2026-05-15 evening — GCP compact-profile snapshot (PARTIAL — wedged)

**HEAD:** main at `defd8c3` (post all 9 PRs from the 2026-05-15 sweep: A+B+C, auditor, integrator, UI+CLI, #36, #37, #38, #39, α, β, γ, δ).
**Hardware:** 3 × `c3-standard-44-lssd` storage + 2 × `c3-standard-44` clients + 1 × `e2-standard-4` ctrl. `europe-west1-b`. Tier_1 100 Gbps confirmed (iperf3 = 46.3 Gbps).
**Driver:** binaries built fresh from main via `.gcp-build/build.sh` in `rockylinux:9` (~6 min); pushed to GCS staging; deployment fixes (`KISEKI_DISABLE_PNFS_LAYOUT` removed, port 2052 open, perf-suite phase 9 fan-out wired) all in tree.
**What changed since previous snapshot:** All this morning's correctness PRs landed on main (auditor fixed metric label arity, integrator wired 307 in S3 path, α added `idempotency_key` + `forwarded_from_node` to the proto with 86 call sites updated, β extended 307 to multipart/DELETE, γ added Bearer-token RBAC, δ shipped 4 dashboard tabs + 9 admin CLI subcommands).

## Status: PARTIAL — phase 4 wedged the suite

Phase 4 (NFSv4 sequential write) hit a hard wall and never completed. The kiseki-server gateway was alive and answering S3 calls, but every NFSv4 write fio issued blocked on Raft-commit backpressure. The composition hydrator on the leader was running 50/s catching up on ~100k queued compositions; fio at 0.4% CPU for 4 minutes.

Killed the suite at +205s in phase 4. NFS unmount hung in-kernel (writes still queued behind the hydrator). Phases 5 (FUSE), 6-9 (S3), 10 (Prom snapshot) never ran from the suite.

Ran a hand-stitched stripped bench directly via SSH for the S3 and FUSE axes that don't need the suite to coordinate. Numbers below.

## What we got

| Op | Number | Workload | Status vs target |
|---|---:|---|---|
| iperf3 client → leader | **46.3 Gbps** per direction | Tier_1 line; matches spec | ✓ |
| S3 PUT 1 KB latency | **p50 2.5 ms** · p99 93.8 ms | 50 serial PUTs from client-1 to leader's S3 endpoint | p50 ✓ (target p99 ≤ 5 ms; p99 missed due to hydrator backlog from killed phase 4) |
| S3 PUT 1 MB serial single-client | **103.7 MB/s** | 100 × 1 MB sequential via curl | low — Raft-commit bound serial; target table doesn't include serial baseline |
| S3 PUT 1 MB × 200 × 16-way single-client | **726 MB/s** | parallel curl, single client, against leader's S3 | target was 4 GB/s single-client; missed — bench under hydrator-backlog load from the wedge |
| FUSE mount (read-write) | works | `kiseki-client mount --endpoint kiseki://10.0.0.10:9100 --read-write` | mount itself fine |
| FUSE write (any size, any flags) | **EIO** on every `create()` | `dd if=/dev/zero of=/mnt/kiseki-fuse/x` with and without `oflag=direct` | ✗ — every create returns EIO; server log shows no rejection, just the hydrator pegging the leader |
| FUSE metadata mkdir+create | **31 ops/s** | 100 dirs+files via mkdir+tee | ✗ — vs prior 2125 ops/s; same Raft-commit saturation |
| pNFS LAYOUTGET delegation | not measured | phase 4b never ran (blocked by phase 4 wedge) | — |
| Multi-shard fan-out (Step B verdict) | not measured | phases 9+9b never ran | — |
| #36 chunk-fill at 200 GB | not measured | only one partial run; ~50 GB cumulative writes | — |

## Findings — to file as bugs

### F-1 (HIGH) — Composition hydrator backlog saturates write path under sustained load

The post-α main does what looks like a single-pass hydrator (one apply_hydration_batch at a time, capped at ~50/sec). When the NFS phase queues 100k+ compositions in ~30 seconds (fio at 48 GB / 3 clients / 4 jobs × 1 MB blocks), the hydrator can't keep up. Every subsequent write (NFS, FUSE, S3 — protocol-agnostic) blocks waiting for commit acks because the leader's mutex stack is saturated.

Symptom: fio at 0.4% CPU, FUSE `create()` returns EIO with no log error, S3 PUT p99 spikes to 90+ ms while p50 stays at 2.5 ms.

Probable root: the hydrator's batching is too small, or the locking discipline serializes hydration with new writes. Either way the post-α code does not have headroom for sustained-load shape.

### F-2 (HIGH) — FUSE `create()` defaults to RO

Default `kiseki-client mount` (with no `--read-write`) opens RW namespace as RO. `mountpoint -q` reports success but every write fails "Read-only file system". Confusing UX; the `--read-write` flag should probably be the default OR the RO state should be loud in the daemon's startup log.

### F-3 (MED) — `mountpoint -q` returns non-zero on a working FUSE RO mount

When mounted RO, `mountpoint -q /mnt/kiseki-fuse` returns 32 ("Permission denied"). The mount IS there (per `/proc/mounts`), the dir IS readable with sudo. Some interaction between FUSE's permission model and the `mountpoint` helper.

### F-4 (MED) — Phase coupling: phase-4 NFS wedge poisons all downstream phases

The perf-suite doesn't isolate phases. Once phase 4's fio queued writes onto Raft, every subsequent phase inherits the leader's hydrator backlog. No way to get clean phase-5+ numbers without tearing down + reapplying the cluster.

## What this run did NOT prove

These remain unverified on real hardware:

- **#36 chunk-GC at 200 GB cumulative** — only ~50 GB written.
- **pNFS LAYOUTGET delegation** — phase 4b never started.
- **#37 FUSE O_DIRECT (FOPEN_DIRECT_IO)** — `dd oflag=direct` returned EIO on every attempt, but EIO is the F-1 / F-2 symptom, not a #37 regression — the same call returns EIO without `oflag=direct` too.
- **#38 EC-4+2 cap at 6 nodes** — compact is 3-node R-3, doesn't trip the path.
- **#39 io_uring runtime toggle** — `KISEKI_IO_URING=1` never set this run.
- **β #48 multi-protocol 307** — no follower-routed writes attempted.
- **γ #46 Bearer-token RBAC** — `/admin/*` not hit this run.
- **δ #47 dashboard / kiseki-admin / kiseki-client surfaces** — surfaces exist; not exercised in this measurement.

## What to do next

1. File F-1 and F-2 as GH issues.
2. Stop running the full perf-suite. Run **one protocol per cluster lifetime** so phase coupling can't poison measurements. Spin compact, run only S3, tear down. Spin compact, run only NFS, tear down. Etc.
3. Resolve F-1 before any further perf measurement is meaningful. Without sustained-load headroom on the write path, every protocol's measurement is hydrator-bound.

## Cross-references

- Compact target table: [`docs/performance/targets.md`](../../docs/performance/targets.md).
- Snapshot before this one: [2026-05-15 morning](2026-05-15-gcp-compact.md).
- The 4 follow-up PRs whose interaction may be the trigger: #46, #47, #48, #49.
