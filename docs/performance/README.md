# Performance

Live view of where the numbers stand and how to reproduce them.
Time-series record of every matrix run lives in
[`specs/performance/`](../../specs/performance/INDEX.md) — those
files are immutable snapshots, this file moves with HEAD.

> Operators tuning a deployment for throughput should also read
> [`docs/operations/durability.md`](../operations/durability.md) —
> the group-commit flags described there trade durability for
> throughput, and the matrix in that doc spells out the loss
> windows under each failure mode.

## Latest snapshot — 2026-05-15 GCP compact

3 × `c3-standard-44-lssd` storage + 2 × `c3-standard-44` clients in
`europe-west1-b`, release `v2026.43.759`. Full breakdown in
[`specs/performance/2026-05-15-gcp-compact.md`](../../specs/performance/2026-05-15-gcp-compact.md).

| Protocol | Throughput | Notes |
|---|---:|---|
| NFSv4.2 write (aggregate) | **1.71 GB/s** | 2 clients × fio bs=1M, direct=1 |
| NFSv4.1 write (no layout — MDS fallback) | 2.92 GB/s | server-side env-var fix landed in `07e8a96`, will re-measure |
| S3 PUT (1MB → 64MB objects) | 673 → 1094 MB/s | single client, 8∥ |
| S3 GET (1 MB × 200) | 1170 MB/s | single client, 8∥ |
| S3 parallel write (2 clients) | 528 MB/s aggregate | |
| S3 PUT latency (1 KB) | p50 7.0 ms · p99 7.5 ms | |
| FUSE mount + metadata | 2125 ops/s | 1000 × mkdir+create |
| FUSE I/O throughput | (unmeasurable) | GH #37 — see below |

Open product issues surfaced by this run:

- **GH [#36](https://github.com/witlox/kiseki/issues/36)** — chunk-store fills after ~200 GB cumulative writes despite 3 TB raw NVMe per node.
- **GH [#37](https://github.com/witlox/kiseki/issues/37)** — `kiseki-client mount` doesn't expose libfuse's `direct_io`; FUSE perf unmeasurable under `fio --direct=1`. Workaround in suite: `dd conv=fdatasync`.
- **GH [#38](https://github.com/witlox/kiseki/issues/38)** — EC-4+2 fragment exceeds 16 MiB per-extent cap by 8 bytes; ≥ 6-node profiles unusable until this lands.

## Snapshot index

The chronological record lives in
[`specs/performance/INDEX.md`](../../specs/performance/INDEX.md).
Latest few:

| Date | Snapshot | One-liner |
|---|---|---|
| 2026-05-15 | [GCP compact](../../specs/performance/2026-05-15-gcp-compact.md) | First post-libfuse-swap GCP run; 3 product bugs surfaced (#36/#37/#38). |
| 2026-05-09 | [libfuse-swap](../../specs/performance/2026-05-09-libfuse-swap.md) | FUSE GET +25% on multi-thread libfuse session loop. NFSv4.1 read 0.5 → 923 MB/s. |
| 2026-05-07 | [post-pNFS-pool](../../specs/performance/2026-05-07-post-pnfs-pool.md) | pNFS GET 17 k → 80 k op/s (round-robin DS pool). |
| 2026-05-07 | [local matrix](../../specs/performance/2026-05-07-local-matrix.md) | FUSE leapfrogs every protocol (52 k PUT / 115 k GET); NFS PUT degradation surfaced. |
| 2026-05-05 | [ADR-042 native local](../../specs/performance/2026-05-05-adr042-native-local.md) | First end-to-end native-binding measurement. A-NG11 gate at 15 % — Phase 9 perf slice pending. |
| 2026-05-03 | [GCP transport](../../specs/performance/2026-05-03-gcp-transport.md) | First multi-node GCP run. Surfaced fabric write quorum-loss bug (fixed in `f362060`). |
| 2026-05-03 | [local baseline](../../specs/performance/2026-05-03-local-baseline.md) | Post-fix May matrix; the "May 2026 baseline" later snapshots delta against. |

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
