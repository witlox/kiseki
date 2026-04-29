# GCP Performance Test Infrastructure

Terraform configuration for deploying a Kiseki performance test cluster on
GCP. A single `var.profile` switch selects one of three orthogonal cluster
shapes — each shape provisions the right hardware *and* points the bench
controller at the right benchmark suite.

## Profiles

| Profile     | Purpose                            | Storage                                        | Clients                                         | Tier_1 | Suite                       | ~$/hr   |
|-------------|-------------------------------------|------------------------------------------------|-------------------------------------------------|--------|-----------------------------|---------|
| `default`   | Broad coverage (release regression) | 6 × c3-standard-22, 4 × local NVMe (1.5 TB)    | 3 × c3-standard-22, 200 GB PD-SSD cache         | 50 Gbps  | `perf-suite.sh`             | ~$13–18 |
| `transport` | NIC + protocol ceiling              | 3 × c3-standard-88, 8 × local NVMe (3 TB)      | 3 × c3-standard-44                              | 100 Gbps | `perf-suite-transport.sh`   | ~$22–30 |
| `gpu`       | ML training scenario                | 3 × c3-standard-44, 4 × local NVMe (1.5 TB)    | 2 × a2-highgpu-1g (1 × A100), 1 TB PD-SSD cache | mixed¹   | `perf-suite-gpu.sh`         | ~$15–22 |

¹ Storage is Tier_1 50 Gbps; a2-highgpu-1g (12 vCPU) is below the Tier_1
floor and gets standard ~24 Gbps egress, which is fine for A100 single-GPU
clients.

All profiles use `nvidia` device class on raw local NVMe (no PD-Standard /
HDD), so per-tier replication is consistent. EC-4+2 is only reachable on
`default` (≥ 6 nodes); the other profiles are deliberately scoped down.

## Quick start

```bash
cd infra/gcp
terraform init

# Pick your profile
terraform apply -var="project_id=your-gcp-project" -var="profile=default"
# or:    -var="profile=transport"
# or:    -var="profile=gpu"

# Wait ~3-5 min for setup-* scripts to complete on each VM, then:
gcloud compute ssh kiseki-ctrl --zone=europe-west6-a

# On ctrl: run the suite this profile selected
sudo bash /opt/kiseki-bench/$(grep -oP 'KISEKI_BENCH_SUITE="\K[^"]+' /etc/kiseki-bench.env)

# Or trigger from your workstation, with progress streamed locally:
./benchmarks/run-perf.sh --project your-gcp-project

# Tear down
terraform destroy -var="project_id=your-gcp-project" -var="profile=default"
```

## What each profile actually measures

### `default` (broad coverage)

`perf-suite.sh` exercises every gateway: S3, NFSv4, pNFS (flex-files), FUSE.
Each fio invocation uses `--direct=1` and per-job sizes (4 GB write,
8 GB read, 2 GB random) large enough to defeat the 88 GB host page cache —
without that the read paths cache after warmup and the headline numbers
become RAM bandwidth, not kiseki bandwidth. The trap and reasoning are
captured in `specs/findings/phase-15c10-nfs41-perf-investigation.md`.

Tests in order:
0. cluster health + Raft leader discovery
1. cluster state snapshot
2. transport selection (informational — GCP exposes no RDMA on c3)
3. inter-node TCP bandwidth (iperf3 baseline)
4. NFSv4 sequential write (3 clients → leader)
4b. pNFSv4.1 write+read (layout delegation, mountstats verification)
5. FUSE native client (write/read/random/metadata) on client-1
6. S3 PUT latency p50/p99 (1 KB × 100)
7. S3 sequential write sweep (1/4/16/64 MB objects)
8. S3 read throughput
9. S3 parallel write (3 clients aggregate)
10. Prometheus metrics snapshot

### `transport` (find the protocol overhead)

`perf-suite-transport.sh` is single-axis: take `iperf3 -P 4` between each
pair of hosts as the wire ceiling, then run kiseki's S3 + pNFS paths over
the same wire and report the percentage we keep. Disks are deliberately
faster than the NIC (8 × local NVMe per c3-standard-88 ≈ 32 GB/s aggregate
read vs the 12.5 GB/s 100 Gbps Tier_1 ceiling), so anything we cap below
iperf3 is gateway/grpc overhead, not I/O.

Tests:
1. iperf3 baseline (30 s, 4 streams) — every "% of wire" later refers here
2. S3 single-stream PUT 1 GB / 10 GB (single-client peak)
3. S3 PUT concurrency sweep (1 / 4 / 16 / 64 / 256 streams of 64 MB objects)
4. S3 GET concurrency sweep
5. pNFS aggregate (3 clients × parallel reads of one 16 GB file — flex-files
   should fan out to all 3 storage nodes; sum of per-client Gbps should
   approach 3× per-client baseline)
6. mTLS overhead (placeholder — needs a parallel plaintext cluster)
7. metrics snapshot

Deliberately skipped: EC, metadata ops, FUSE — those belong in `default`.

### `gpu` (ML training scenario)

`perf-suite-gpu.sh` answers: *can a GPU client keep its training step fed
from kiseki?* Boots the GCP Deep Learning VM image (Debian 11 + CUDA 12.3 +
nvidia drivers preinstalled) and adds `nvidia-fs` + a default
`/etc/cufile.json` so cuFile / GPUDirect Storage is reachable.

Tests:
1. GDS env check: GPU present, CUDA driver, `nvidia_fs` module loaded,
   `/etc/cufile.json` present, `gdscheck -p` output
2. Bulk dataset stage-in (10 × 1 GB shards → client-local PD-SSD cache —
   simulates a Slurm prolog)
3. Training-loop simulation (random 256 KB batches × 8 jobs × 60 s with
   `--direct=1` and 10 s ramp-time, p50/p99 reported)
4. Epoch repeat (same workload × 3 — epoch 1 cold, epochs 2–3 should hit
   `/cache` in organic mode; cluster-side `kiseki_cache_hit_total` reported)
5. Local-NVMe baseline (same fio against `/cache/staged/*` directly — the
   "free" tax floor; kiseki's tax = delta from this number)
6. metrics snapshot

## Disk paths

GCP exposes local NVMe SSDs under stable by-id symlinks regardless of NVMe
namespace ordering, so the storage setup script consumes a comma-separated
list like `/dev/disk/by-id/google-local-ssd-{0..N-1}`. Boot disk on c3 is
also NVMe (separate symlink, never enumerated).

## File layout

```
infra/gcp/
├── perf-cluster.tf                  # var.profile drives every choice
├── README.md                        # this file
├── scripts/
│   ├── setup-raw-storage.sh         # all storage nodes (any profile)
│   ├── setup-perf-client.sh         # CPU clients (default + transport)
│   ├── setup-gpu-client.sh          # GPU clients (gpu profile)
│   └── setup-bench-ctrl.sh          # ctrl: writes /etc/kiseki-bench.env
└── benchmarks/
    ├── perf-common.sh               # shared helpers (sourced by all suites)
    ├── perf-suite.sh                # default profile
    ├── perf-suite-transport.sh      # transport profile
    ├── perf-suite-gpu.sh            # gpu profile
    ├── metrics-collector.sh         # background Prometheus scraper
    └── run-perf.sh                  # local wrapper: scp suites + tail logs
```

## Costs

Hourly estimates above are for europe-west region, on-demand pricing as of
the time this file was last updated. Local SSD storage is included in the
machine cost; PD-SSD cache adds ~$0.04/GB/month per client. Add the
`bench-ctrl` (e2-standard-4, ~$0.13/hr) + a small results-bucket egress.

## Notes on RDMA / RoCE

GCP does **not** expose user-mode RDMA on c3 or n2 instances. The README's
prior "RoCEv2 + TCP" claim was aspirational; nothing in this configuration
runs RoCE, and `perf-suite-transport.sh` is honest about this — it reports
TCP overhead vs iperf3, not RDMA performance. Real RoCE on GCP today
requires HPC-targeted instance types (h3-standard-88 with Falcon-RoCE) or
GPU-cluster A3-series, both of which are special-allocation and outside the
on-demand path. Add a profile only when there's a concrete demand for it.
