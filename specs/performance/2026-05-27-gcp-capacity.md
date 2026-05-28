# 2026-05-27 — GCP `default` profile, capacity-management run (PR #116)

Immutable snapshot. Branch `feat/115-capacity-tiering` (PR #116), Rocky9
bins. 6 × `c3-standard-22-lssd` storage (4× 375 GB local NVMe = 1.5 TB/node)
+ 3 × `c3-standard-22` clients, `europe-west1-b`, EC-4+2.

**Purpose:** verify the GH #115 raw-device-pool fix + ADR-045 capacity
observability on real hardware, and re-measure the protocol matrix now
that the chunk store isn't capped at 4 GiB.

## Headline — GH #115 fixed, confirmed live

| Check | Result |
|---|---|
| Chunk device per node | **1500 GiB** (full 4× 375 GB NVMe) — was the silent 4 GiB file fallback |
| `kiseki-admin capacity` | cluster 10.3 TB pool, per-class (fast NVMe), system-disk meta/small tiers — live |
| Dedup ratio (20 identical objs) | **1.54×** (logical 13.5 MB → physical 8.8 MB) — the once-dead gauges work |
| Multi-shard S3 (6 distributed-leader shards) | 60 PUT / 40 cross-node GET — **0 errors, 0 mismatches** (#111 + #102 + #107 holding) |

## Protocol matrix (parallel × 3 clients unless noted; 64 KB, conc 16, 30 s)

| Protocol / shape | Aggregate | Note |
|---|---:|---|
| native get-heavy | **10,598 op/s · 662 MiB/s** | p99 ~90 ms, 0 err — reads scale |
| native mixed | 305 op/s · 19.1 MiB/s | write-dominated |
| native put-heavy | 253 op/s · 15.8 MiB/s | **commit-bound** |
| S3 PUT (curl, 300 ops/client) | 368 op/s | commit-bound + curl per-op overhead |
| S3 GET (curl) | 2,286 op/s | curl per-op overhead caps vs native |
| NFSv4.2 write (fio bs=1m, 2 jobs, direct=1) | **6.9 MB/s** | commit-bound (per-COMMIT wall) |
| NFSv4.2 read | inconclusive | fio read of non-pre-written file; not chased on a fast pass |
| pNFS / FUSE write+read | — | FUSE mount didn't attach (see findings); pNFS not run |

## Findings

1. **Distributed multi-shard WRITES are commit-bound** (~250–370 op/s native/S3,
   6.9 MB/s NFS) while reads scale (10.6k op/s / 662 MiB/s). Root cause in the
   logs: **22,354 `forward to leader` events** — every write to a remote-led
   shard does a synchronous forward → leader composition + Raft commit per op.
   The ~40× read/write asymmetry is this per-COMMIT + per-forward cost. Correct
   (0 errors), but the write path is the bottleneck. → perf issue.
2. **`forward to leader` is logged at WARN** (22k lines for a *normal* path) —
   logging noise + overhead at write rates; should be DEBUG. → quick fix.
3. **FUSE mount does not attach** — `kiseki-client mount` prints "Mounting…via
   native" then hangs; no server-side error (the hang is client-side, libfuse
   session). Rocky9 build. → bug.
4. **NOT product bugs (operator setup error this run):** 64,863 `tenant mismatch`
   + 7,216 `namespace not found` WARNs came from hand-creating the bench
   namespace under the bootstrap tenant instead of the bench tenant
   (`179e565c…`). The correct method is `setup-shards.sh`; the read path
   resolves tenant from the namespace registration, so a wrong-tenant namespace
   404s reads — which incidentally re-confirms the IAM write-authz gap (#117).
   Fixed mid-run by creating the namespace under `179e565c`.

## Verification gaps (need follow-up)
- pNFS not measured; NFSv4 read inconclusive; FUSE blocked on the mount hang.
- Tiered placement *on class* (fast vs cold) needs a **mixed-media profile**
  (NVMe + HDD) — `default` is all-NVMe.
