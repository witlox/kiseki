# Performance

Last refreshed: **2026-05-03** (after the May 2026 perf-fix sweep).

Two data sources currently:

1. **[Local single-node matrix](#local-single-node-matrix)** — `kiseki-profile`
   driving 5 protocols × 3 workload shapes against a fresh `kiseki-server`
   process on one host. Captures both CPU (pprof flamegraphs) and heap
   (dhat). Used to drive the perf fixes below.
2. **[GCP transport profile (2026-05-03)](#gcp-transport-profile-2026-05-03)** —
   3-storage + 3-client cluster on `c3-standard-88-lssd` /
   `c3-standard-44`. **Partial**: the run surfaced a fabric write
   quorum-loss bug (cross-node `PutFragment` averaging 2 s on a
   28 Gbps wire). Throughput data from this run is not representative
   until the bug is fixed — see [Open issues](#open-issues).

## Perf-fix history (May 2026)

| Commit | Change | Local matrix impact |
|---|---|---|
| `b0f048d` | server: single-node MDS advertises local DS uaddr | pNFS GET 0 op/s · 3528 errors → 62 op/s · 0 errors |
| `56ec297` | client/nfs: `tokio::sync::Mutex` on session — std mutex starved tokio runtime under concurrency | NFSv4 c=16 read p99: 30 s → 667 ms |
| `e058ded` | client+gateway: TCP_NODELAY on NFS RpcTransport + pNFS DS listener | NFSv4 c=1 GET: 24 op/s · 41 ms → 9285 op/s · 199 µs |
| `eebc7f0` | profile harness: tokio mutex on FuseDriver + pNFS session pool *(harness-only)* | n/a — measurement fix |
| `59cab58` | client/nfs: connection pool — N parallel sessions per Nfs3/Nfs4Client | NFSv4 c=16 GET: 9 k → 27 k op/s |

Each commit references the metric it was driven by; the local-matrix
section below is the post-fix snapshot.

## Local single-node matrix

Run via `kiseki-profile`; outputs land in `/tmp/kiseki-prof/`. See
[`reference_profile_matrix`](../../crates/kiseki-profile/) for usage.

### Configuration

| | |
|---|---|
| Machine | dev workstation (Linux, x86_64, 16 cores) |
| Cluster | single-node (1 × `kiseki-server`, ephemeral ports) |
| Object size | 64 KiB |
| Concurrency | 16 (matches NFS connection-pool default cap) |
| Duration | 30 s per scenario |
| Warmup | 256 objects pre-created for get-heavy / mixed |

### Throughput post-fixes (concurrency=16, 64 KiB)

| Protocol | put-heavy | get-heavy | mixed (70 P / 30 G) |
|---|---:|---:|---:|
| **S3 (HTTP)** | 7124 op/s · 445 MiB/s | **25 843 op/s · 1.6 GiB/s** | 8470 op/s · 529 MiB/s |
| **NFSv3** | 2042 op/s · 128 MiB/s | 26 615 op/s · 1.6 GiB/s | 778 op/s · 49 MiB/s |
| **NFSv4.1** | 8327 op/s · 520 MiB/s | **27 291 op/s · 1.7 GiB/s** | 808 op/s · 50 MiB/s |
| **pNFS Flex Files** | 8327 op/s · 520 MiB/s | 16 549 op/s · 1.0 GiB/s | 2254 op/s · 141 MiB/s |
| **FUSE** | 2790 op/s · 174 MiB/s | 10 789 op/s · 674 MiB/s | 3375 op/s · 211 MiB/s |

### Tail latencies post-fixes (p99 µs, c=16)

| Protocol | put-heavy | get-heavy | mixed |
|---|---:|---:|---:|
| S3 | 3 297 | 6 205 | 3 102 |
| NFSv3 | 11 277 | 4 038 | 49 157 |
| NFSv4.1 | 10 528 | 4 234 | 46 076 |
| pNFS | 10 540 | 21 116 | 23 493 |
| FUSE | 159 613* | 134 | 126 747* |

*FUSE put p99 tail (160 ms) is the next investigation target. p50 is
0.35 ms; the bimodal distribution suggests batched composition flush
or redb checkpoint contention. Not blocking — the median is fast.

### Total trajectory across the May fix sweep

| | starting matrix | after the 5 fixes | gain |
|---|---:|---:|---:|
| NFSv3 GET (c=16) | 12 op/s · p99 31 s | 26 615 op/s · p99 4 ms | **2 220×** throughput / 7 700× p99 |
| NFSv4.1 GET (c=16) | 24 op/s · p99 30 s | 27 291 op/s · p99 4 ms | **1 137×** / 7 100× |
| pNFS GET (c=16) | **0 op/s · 100 % errors** | 16 549 op/s · p99 21 ms | broken → working |
| pNFS PUT (c=16) | 583 op/s · p99 553 ms | 8 327 op/s · p99 11 ms | 14× / 50× |
| S3 GET (c=16) | 4 580 op/s | 25 843 op/s | 5.6× |

Numbers above are server-side ceiling on a single host. Multi-node
ceilings (and EC) are pending the GCP run.

### Captured profiles

- `/tmp/kiseki-prof/cpu-{protocol}-{shape}.svg` — pprof flamegraphs
- `/tmp/kiseki-prof/heap-{protocol}-{shape}.json` — dhat heap

Hot stacks in the post-fix S3 PUT path (server side):
- 22 % SHA256 in `kiseki_crypto::chunk_id::derive_chunk_id`
- 17 % redb `name_insert` in `CompositionStore::bind_name`
- 13 % AEAD seal envelope
- 13 % Raft `append_delta`

These are the candidates for the next round of optimization.

## GCP transport profile (2026-05-03)

### Cluster

| | |
|---|---|
| Profile | `transport` (`infra/gcp/perf-cluster.tf`) |
| Storage | 3 × `c3-standard-88-lssd` (88 vCPU, 8 × local NVMe) |
| Clients | 3 × `c3-standard-44` (44 vCPU) |
| Ctrl | 1 × `e2-standard-4` |
| Region / zone | europe-west1-b (NOT west6 — `c3-...-lssd` is west1-only) |
| Tier_1 NIC | 100 Gbps egress on storage; ~50 Gbps on clients |

### Run timing

- Apply: ~2 min after binaries on GCS
- Setup scripts: ~3 min on storage / client / ctrl
- Suite (`perf-suite-transport.sh`): ~3 min for sections 1-4, hung in section 5 (pNFS) until killed

### What the run measured (sections 1–4 only)

iperf3 baseline (4 stream, 30 s):

| client → storage-1 | Gbps |
|---|---:|
| 10.0.0.30 → 10.0.0.10 | 28.2 |
| 10.0.0.31 → 10.0.0.10 | 28.0 |
| 10.0.0.32 → 10.0.0.10 | 28.6 |

(The 4-stream count under-saturates the 100 Gbps wire; not enough
streams to compete with TCP slow-start ramp-up.)

S3 PUT concurrency sweep (64 MB objects, against the leader):

| streams | throughput |
|---:|---:|
| 1 | 1.4 Gbps |
| 4 | 4.4 Gbps |
| 16 | 10.0 Gbps |
| 64 | 11.4 Gbps |
| 256 | 16.4 Gbps (cap) |

S3 GET sweep:

| streams | throughput |
|---:|---:|
| 1 | 7.2 Gbps |
| 4 | 10.0 Gbps |
| 16 | 10.1 Gbps |
| 64 | 10.3 Gbps |
| 256 | 110.3 Gbps (page-cache effect) |

**These numbers are not trustworthy as-is** — see next section.

### What the run actually surfaced: fabric write quorum loss

During the S3 PUT sweep, storage-1's `/metrics` showed:

```
kiseki_fabric_quorum_lost_total       1940       ← matches the PUT-500 count
kiseki_fabric_op_duration_seconds     count=1552 sum=3177 s
                                                  → avg fabric PUT = 2.05 s
                                                  → 75 % of fabric PUTs > 1 s
```

Storage-1's logs:

```
WARN kiseki_chunk_cluster: peer PutFragment timed out peer=node-2
WARN gateway write: chunks.write_chunk failed
       error=quorum lost: only 1/2 replicas acked
```

So the cap of "16.4 Gbps PUT throughput" is misleading: half the
PUTs are actually 500-ing because cross-node `PutFragment` times
out at the 5 s default. **The reported throughput is throughput of
successful writes only**, not the cluster's actual write capacity.

Until the underlying cause is fixed, all GCP throughput numbers in
this section should be considered indicative, not authoritative.

### Suspected cause

`kiseki-server::runtime::build_fabric_channel` (runtime.rs:104) builds
the per-peer fabric `tonic::transport::Channel` without
`tcp_nodelay(true)`. Same Nagle / 40 ms-delayed-ACK problem fixed for
the NFS clients in `e058ded`, but the cross-node fabric path still
has it. A single-call round trip with Nagle on a 64 MB chunk involves
many ack windows; combined with chunk encoding it plausibly explains
the 2 s avg.

Local single-node profiling never exercised this path — single-node
clusters don't fan out fragments to peers. The only way to catch
this kind of bug is multi-node testing.

## Open issues

- [ ] **Fabric channel missing `tcp_nodelay`** (`runtime.rs:build_fabric_channel`) —
  prime suspect for the GCP `quorum_lost_total` regression. Fix
  pattern: same as `e058ded`, just on the tonic `Endpoint`.
- [ ] **Re-run GCP transport profile after the fabric fix** to get
  trustworthy multi-node throughput.
- [ ] **`perf-suite-transport.sh` mount option `pnfs` is rejected by
  modern kernels** (silently — `mount.nfs4` returns 0 with an
  "incorrect mount option" message). Already patched in the
  in-cluster copy of the script for the 2026-05-03 run; not yet
  back-merged to `infra/gcp/benchmarks/`.
- [ ] **`perf-suite-transport.sh` mounts at `/`** but kiseki's
  pseudo-root is non-writable; should mount `/default`. Caused the
  pNFS aggregate test to hang on a 0-byte fio write. Same back-merge.
- [ ] **FUSE put-heavy p99 = 160 ms tail** — local single-node, c=16.
  p50 is 0.35 ms; bimodal. Likely a redb checkpoint or batched
  composition flush. Not blocking but worth a flamegraph dive.

## Running the matrix locally

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

## Running on GCP

```bash
cd infra/gcp
terraform init

# Build VM-target binaries (rocky9 container)
docker run --rm \
  -v $PWD/../..:/src \
  -v $PWD/../../.gcp-build/cache-target:/src/target \
  -v $PWD/../../.gcp-build/cache-cargo:/root/.cargo \
  -v $PWD/../../.gcp-build/dist:/out \
  -w /src rockylinux:9 \
  bash /src/.gcp-build/build.sh

gcloud storage cp ../../.gcp-build/dist/kiseki-{server,client}-x86_64.tar.gz \
  gs://kiseki-bench-binaries-pwitlox-20260502/

# transport profile must run in europe-west1 (c3-standard-88-lssd
# is not available in west6 as of 2026-05-03)
terraform apply \
  -var=project_id=cscs-400112 \
  -var=region=europe-west1 -var=zone=europe-west1-b \
  -var=profile=transport \
  -var=binary_url_base=https://storage.googleapis.com/kiseki-bench-binaries-pwitlox-20260502

# Drive each phase manually rather than running the full suite at
# once — that way you stop at the first error instead of carrying
# on for several minutes through 500-class failures.
bash .gcp-build/ssh-helper.sh kiseki-ctrl
# on ctrl: source /etc/kiseki-bench.env, then run individual sections
```

Tear down when done — `c3-standard-88-lssd` is ~$22-30/hr.

```bash
terraform destroy -var=project_id=cscs-400112 \
  -var=region=europe-west1 -var=zone=europe-west1-b \
  -var=profile=transport
```
