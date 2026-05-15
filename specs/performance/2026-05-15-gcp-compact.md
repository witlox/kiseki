# 2026-05-15 — GCP compact-profile snapshot

**HEAD:** release `v2026.43.759` (commit `f6f6e5b`).
**Hardware:** 3 × `c3-standard-44-lssd` storage (8 × 375 GB NVMe = 3 TB raw per node, Tier_1 100 Gbps) + 2 × `c3-standard-44` clients with 100 GB PD-SSD cache + 1 × `e2-standard-4` ctrl. `europe-west1-b`.
**Driver:** `infra/gcp/benchmarks/perf-suite.sh` against the in-cluster ctrl. fio `bs=1M direct=1 size=4G numjobs=4` for NFS/pNFS; curl + s5cmd for S3; `dd conv=fdatasync` for FUSE I/O (GH #37).
**What changed since previous snapshot:**

- First GCP perf run since 2026-05-02 transport-profile partial.
- Suite-side fixes (commit `07e8a96`): `mount -o pnfs` removed (modern kernels reject it), `kiseki-client mount --endpoint` now uses `kiseki://` scheme, FUSE phase swapped from `fio --direct=1` (always 0 MB/s on FUSE — GH #37) to `dd conv=fdatasync`.
- Deployment-side fixes (commit `07e8a96`): `KISEKI_DISABLE_PNFS_LAYOUT=true` removed from `setup-raw-storage.sh` (obsolete after `d7d90a5` + `6ff8e65`); firewall opens port 2052 (pNFS DS).
- Compact run was on the unpatched cluster (committed fixes apply to the NEXT run). Numbers below reflect the patched perf-suite scripts hot-deployed mid-run; the env-var fix didn't take effect (would have required a kiseki-server restart, which we did but it set off the chunk-fill bug — GH #36 — so pNFS layouts still didn't fire).

## Throughput

| Protocol | Number | Workload |
|---|---:|---|
| NFSv4.2 write (aggregate) | **1.71 GB/s** | 2 clients × fio bs=1M, direct=1, 4G × 4 jobs |
| NFSv4.1 write (aggregate, no layout delegation) | **2.92 GB/s** | same shape, vers=4.1 mount |
| S3 PUT 1 MB × 1024 | 673 MB/s | single client, 8∥ |
| S3 PUT 4 MB × 256 | 972 MB/s | |
| S3 PUT 16 MB × 64 | **1094 MB/s** | |
| S3 PUT 64 MB × 16 | 1014 MB/s | |
| S3 GET 200 × 1 MB | **1170 MB/s** | single client, 8∥ |
| S3 parallel write | 528 MB/s aggregate | 2 clients × 100 × 1 MB |
| S3 PUT latency 1 KB | p50 7.0 ms · p99 7.5 ms | |
| FUSE mount + metadata | 2125 ops/s | 1000 × `mkdir + create` |
| FUSE I/O throughput | (unmeasurable) | GH #37 — see findings |
| iperf3 client → leader | 46 Gbps per direction | |
| iperf3 storage ↔ storage | 31 Gbps per direction | |

## Findings

### GH #36 — chunk-store fills after ~200 GB cumulative writes

After two perf-suite runs (~200 GB total writes) the block allocator hit "device full" with `largest_free_blocks=64` (256 KB largest free extent) despite 3 TB raw NVMe per node. Boot disk fine (~8 GB used / 50 GB). Suspected GC gap or single-disk fan-out — chunks may all land on one of the 8 NVMe partitions per node. Workitem in GH #36.

### GH #37 — FUSE perf unmeasurable under O_DIRECT

`kiseki-client mount` doesn't expose libfuse's `direct_io` flag, so `fio --direct=1` silently returns 0 MB/s on FUSE mounts (kernel either rejects the open or short-circuits page cache, fio measures nothing). Suite swapped to `dd conv=fdatasync` so the fsync time lands in the elapsed total; mount + metadata paths confirmed working (2125 ops/s on 1000 × mkdir+create). Real FUSE throughput numbers pending #37.

### GH #38 — EC-4+2 fragment exceeds 16 MiB per-extent cap

Discovered when we switched to the `default` profile (6-node, EC-eligible): every NFSv4 write loops on `block alloc: request exceeds per-extent cap requested=16777224 max_per_extent=16777216` (16 MiB + 8 bytes header vs 16 MiB cap). The error message even says "caller must split across extents" — code knows what to do but doesn't. Compact's 3-node R-3 path doesn't trip it; any ≥ 6-node profile is unusable until #38 lands.

### pNFS layout delegation never triggered

Both compact runs measured "NFSv4.1 with MDS-only fallback" rather than real pNFS. Root cause: the GCP boot script carried `KISEKI_DISABLE_PNFS_LAYOUT=true` for 8 days after `d7d90a5` made it obsolete. Removed in commit `07e8a96`; also required firewall opening port 2052 (added in same commit). Next run on a clean cluster should see real LAYOUTGET round-trips; will re-measure.

## Cross-references

- GH issues: [#36](https://github.com/witlox/kiseki/issues/36), [#37](https://github.com/witlox/kiseki/issues/37), [#38](https://github.com/witlox/kiseki/issues/38).
- Commit `07e8a96` — perf/gcp: unblock pNFS layouts, FUSE mount, redact project ID (5 fixes).
- Raw results: `infra/gcp/benchmarks/results/20260515-103227-compact/kiseki-perf-compact-20260515-081627/`.
