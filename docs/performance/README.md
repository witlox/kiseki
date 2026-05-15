# Performance

Live view of where the numbers stand and how to reproduce them.
Time-series record of every matrix run lives in
[`specs/performance/`](https://github.com/witlox/kiseki/blob/main/specs/performance/INDEX.md) — those
files are immutable snapshots, this file moves with HEAD.

> Operators tuning a deployment for throughput should also read
> [`docs/operations/durability.md`](../operations/durability.md) —
> the group-commit flags described there trade durability for
> throughput, and the matrix in that doc spells out the loss
> windows under each failure mode.

## Latest snapshot — 2026-05-15 evening (PARTIAL — phase 4 wedged)

3 × `c3-standard-44-lssd` storage + 2 × `c3-standard-44` clients in `europe-west1-b`, binaries built from main `defd8c3` (post all 9 PRs in the sweep). Full breakdown in [`specs/performance/2026-05-15-gcp-compact-evening-partial.md`](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-15-gcp-compact-evening-partial.md).

| Protocol | Number | Status |
|---|---:|---|
| iperf3 client → leader | **46.3 Gbps** | ✓ Tier_1 line confirmed |
| S3 PUT 1 KB latency | **p50 2.5 ms** · p99 93.8 ms | p50 ✓; p99 hit by hydrator backlog |
| S3 PUT 1 MB serial single-client | **103.7 MB/s** | low — Raft-commit bound serial |
| S3 PUT 1 MB × 200 × 16-way single-client | **726 MB/s** | under hydrator-backlog load |
| FUSE mount (RW) | works | mount itself functional |
| FUSE create (any size / any flags) | **EIO** | ✗ — F-1 in the snapshot |
| FUSE metadata (mkdir+create) | **31 ops/s** | ✗ — vs prior 2125, same Raft-commit saturation |
| NFSv4.2 write (aggregate) | **wedged** | phase 4 stalled at +205s; hydrator backlog saturating writes |
| NFSv4.1 pNFS, pNFS LAYOUTGET delegation, multi-shard fan-out, #36 200 GB cumulative, #38 EC-4+2 cap, #39 io_uring runtime toggle, β #48 multi-protocol 307, γ #46 RBAC, δ surfaces | not measured | downstream of the wedge; need a per-protocol isolated run |

### Earlier 2026-05-15 morning snapshot (pre-sweep release `v2026.43.759`)

| Protocol | Throughput | Notes |
|---|---:|---|
| NFSv4.2 write (aggregate) | **1.71 GB/s** | 2 clients × fio bs=1M, direct=1 |
| NFSv4.1 write (no layout — MDS fallback) | 2.92 GB/s | env-var fix in `07e8a96`, awaiting clean re-measure |
| S3 PUT (1MB → 64MB objects) | 673 → 1094 MB/s | single client, 8∥ |
| S3 GET (1 MB × 200) | 1170 MB/s | single client, 8∥ |
| S3 parallel write (2 clients) | 528 MB/s aggregate | |
| S3 PUT latency (1 KB) | p50 7.0 ms · p99 7.5 ms | |
| FUSE mount + metadata | 2125 ops/s | 1000 × mkdir+create — pre-α |
| FUSE I/O throughput | (unmeasurable) | GH #37 — fixed in PR #41 (released, not validated on GCP yet) |

### Issues now open (post-evening run)

- **GH [#36](https://github.com/witlox/kiseki/issues/36)** — chunk-store fills after ~200 GB cumulative writes. PR #45 wired the GC plumbing; awaits GCP re-run to confirm on real hardware.
- **GH [#37](https://github.com/witlox/kiseki/issues/37)** — `kiseki-client mount` direct_io. PR #41 landed `FOPEN_DIRECT_IO`; end-to-end GCP verification deferred (the evening run's EIO is F-1, not #37).
- **GH [#38](https://github.com/witlox/kiseki/issues/38)** — EC-4+2 fragment cap. PR #43 fixed; not validated on a real 6-node cluster.
- **GH [#39](https://github.com/witlox/kiseki/issues/39)** — io_uring backend. PR #42 + follow-up wiring landed; runtime toggle not exercised on GCP yet.
- **F-1** (new, file as bug) — Composition hydrator backlog saturates write path under sustained load. Symptom: NFS / FUSE / S3 writes all stall after a ~30-second burst.
- **F-2** (new, file as bug) — FUSE `create()` defaults to RO; `--read-write` should be the default OR the daemon should log the RO posture loudly.

### Verification gaps (need a clean re-run)

- pNFS LAYOUTGET delegation post-`07e8a96` boot fix
- #37 end-to-end FUSE `O_DIRECT` against real cluster
- #36 200 GB cumulative chunk-fill resolved by PR #45
- #38 EC-4+2 cap on 6-node default profile
- Multi-shard write fan-out (Step B): `gateway_requests > 0` on all 3 nodes
- β #48 307 routing to follower
- γ #46 admin-tier Bearer-token enforcement

## Snapshot index

The chronological record lives in
[`specs/performance/INDEX.md`](https://github.com/witlox/kiseki/blob/main/specs/performance/INDEX.md).
Latest few:

| Date | Snapshot | One-liner |
|---|---|---|
| 2026-05-15 evening | [GCP compact PARTIAL](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-15-gcp-compact-evening-partial.md) | Phase 4 wedged on hydrator backlog; NFS/FUSE/pNFS not measured. F-1/F-2 to file. |
| 2026-05-15 morning | [GCP compact](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-15-gcp-compact.md) | First post-libfuse-swap GCP run; 3 product bugs surfaced (#36/#37/#38). |
| 2026-05-09 | [libfuse-swap](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-09-libfuse-swap.md) | FUSE GET +25% on multi-thread libfuse session loop. NFSv4.1 read 0.5 → 923 MB/s. |
| 2026-05-07 | [post-pNFS-pool](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-07-post-pnfs-pool.md) | pNFS GET 17 k → 80 k op/s (round-robin DS pool). |
| 2026-05-07 | [local matrix](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-07-local-matrix.md) | FUSE leapfrogs every protocol (52 k PUT / 115 k GET); NFS PUT degradation surfaced. |
| 2026-05-05 | [ADR-042 native local](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-05-adr042-native-local.md) | First end-to-end native-binding measurement. A-NG11 gate at 15 % — Phase 9 perf slice pending. |
| 2026-05-03 | [GCP transport](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-03-gcp-transport.md) | First multi-node GCP run. Surfaced fabric write quorum-loss bug (fixed in `f362060`). |
| 2026-05-03 | [local baseline](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-03-local-baseline.md) | Post-fix May matrix; the "May 2026 baseline" later snapshots delta against. |

## How to run the matrix

### Local single-node (kiseki-profile)

```bash
# Build server with profiling features
cargo build --release -p kiseki-server --features pprof
CARGO_TARGET_DIR=target-dhat cargo build --release \
  -p kiseki-server --features dhat

# Build the driver
cargo build --release -p kiseki-profile

# Full 5×3 matrix (CPU + heap, ~30 min)
bash crates/kiseki-profile/run-all.sh

# Resume only missing combinations (idempotent)
bash crates/kiseki-profile/resume.sh
```

### Multi-node on GCP

Set your GCP project ID once:

```bash
export KISEKI_GCP_PROJECT=<your-gcp-project>
```

Put the rest in `infra/gcp/perf.auto.tfvars` (gitignored — never commit it):

```hcl
project_id  = "<your-gcp-project>"
region      = "europe-west1"     # required for transport (c3-standard-88-lssd unavailable in west6)
zone        = "europe-west1-b"
profile     = "compact"          # or "default" (broken — GH #38), "transport", "gpu"
release_tag = "v2026.43.759"     # pulls tarballs from GitHub releases
```

Then:

```bash
cd infra/gcp
terraform init
terraform apply -auto-approve

# Drive each phase manually rather than running the full suite at
# once — that way you stop at the first error instead of carrying
# on for several minutes through 500-class failures.
bash .gcp-build/ssh-helper.sh kiseki-ctrl
# on ctrl: source /etc/kiseki-bench.env, then run individual sections
```

Tear down when done — `c3-standard-44-lssd` is ~€10/hr; `c3-standard-88-lssd` is ~€22-30/hr:

```bash
terraform destroy -auto-approve
```

**Testing unreleased binaries**: build in a rocky9 container, push to your own
GCS staging bucket, and override `binary_url_base` in your tfvars to point at
it. The boot scripts append `/kiseki-{server,client}-<arch>.tar.gz` to whatever
URL you set.

```bash
docker run --rm \
  -v $PWD/../..:/src \
  -v $PWD/../../.gcp-build/cache-target:/src/target \
  -v $PWD/../../.gcp-build/cache-cargo:/root/.cargo \
  -v $PWD/../../.gcp-build/dist:/out \
  -w /src rockylinux:9 \
  bash /src/.gcp-build/build.sh

gcloud storage cp ../../.gcp-build/dist/kiseki-{server,client}-x86_64.tar.gz \
  gs://<your-staging-bucket>/
# then in perf.auto.tfvars:
#   binary_url_base = "https://storage.googleapis.com/<your-staging-bucket>"
```
