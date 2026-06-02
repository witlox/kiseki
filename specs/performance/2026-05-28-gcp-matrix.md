# 2026-05-28 — GCP `default` profile, full protocol-matrix pass (PR #116)

Immutable snapshot. Branch `feat/115-capacity-tiering` (PR #116), rocky9
bins built fresh (glibc-2.34 floor, non-FIPS, includes the #124 FUSE
connect-timeout fix). 6 × `c3-standard-22-lssd` storage (1.5 TB NVMe/node)
+ 3 × `c3-standard-22` clients, `europe-west1-b`, EC-4+2, bench namespace
`6658810a…` (18 shards) under bench tenant `179e565c…` via the canonical
`namespace-create`.

**Purpose:** verify the #124 FUSE fix on real hardware and run the full
protocol matrix (native, S3, NFSv3, NFSv4, pNFS, FUSE) — **functionality
first**.

## Headline

| Check | Result |
|---|---|
| #115 chunk pool | **10.3 TB** cluster (1.5 TB/node) — holds |
| **#124 FUSE connect-timeout fix** | **VERIFIED LIVE** — `kiseki-client mount` now **attaches** on rocky9 (was a silent hang); `Connecting…`/`Connected…` progress prints work |
| Dedup observability | **6.03×** (387 MB logical → 64 MB physical, 2,796 chunks) |
| native + S3 | fully functional, 0 errors |
| **FUSE + NFSv4 + pNFS read-by-name** | **BROKEN — 0 bytes (#127)** |
| **NFSv3 mount** | **BROKEN (#128)** |

## Protocol matrix (parallel × 3 clients for native; 64 KB, conc 16, 30 s)

| Protocol / shape | Result | Status |
|---|---:|---|
| native get-heavy | **22,668 op/s · 1,417 MiB/s** | ✓ 0 err — **2× the 2026-05-27 run** (10.6k); reads scale |
| native put-heavy | 261 op/s · 16 MiB/s | ✓ 0 err — commit-bound (#126) |
| native mixed | 319 op/s · 20 MiB/s | ✓ 0 err — commit-bound |
| S3 PUT + cross-node GET (8 objs, curl -T) | 8/8, bytes verified | ✓ 0 err, 0 mismatch |
| FUSE mount | **attaches** | ✓ #124 fix verified live |
| FUSE read-by-name (256 MiB, cache-dropped) | **0 bytes** | ✗ #127 |
| NFSv4.2 / pNFS write→read (cache-dropped) | **0 bytes** | ✗ #127 |
| NFSv3 mount | fails (`showmount` RPC) | ✗ #128 |

## Findings

1. **POSIX name-based reads (FUSE + NFSv4 + pNFS) return 0 bytes (#127).**
   Writes persist (compositions created, dedup 6.03×, chunks land) but the
   directory entry name→composition binding doesn't resolve on read:
   `readdir` lists composition-UUIDs as names, `lookup(name)` → 0-byte
   getattr + 0-byte read, `REMOVE` → `composition not found`. Reproduces
   **same-node** (write+read both on the leader), so not cross-node
   replication. native (by composition_id) + S3 (by key) bypass the POSIX
   name index and work perfectly — so the gateway core is healthy; the bug
   is the POSIX filesystem name/directory layer on multi-node. The default-
   namespace shard `00…0001` hydrator was at `last_applied_seq=67` vs the
   bench shards' ~1000 — name deltas for the POSIX root dir look unapplied.
   The #124 fix exposed this by letting FUSE mount for the first time.
2. **NFSv3 mount fails (#128)** — `showmount -e` → `RPC: Unable to receive`,
   `mount vers=3` → `No such file or directory`. The MOUNT/export handshake
   isn't answering on multi-node. (NFSv4.x mounts attach.)
3. **#124 FUSE connect-timeout fix confirmed live** — the whole reason FUSE
   was measurable this run. Mount attaches; the progress prints made the
   read-path break easy to localize.

## Verification gaps / next
- #127 + #128 block any FUSE/NFS throughput numbers (won't report perf while
  ops fail). native + S3 are the clean cells this run.
- Tiered placement-on-class still needs a mixed-media (NVMe+HDD) profile.

Teardown: 24 resources destroyed, state empty, 0 stray instances.
