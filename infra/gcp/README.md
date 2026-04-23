# GCP Performance Test Infrastructure

Terraform configuration for deploying a Kiseki test cluster on GCP
with multiple disk types and network configurations for transport
and protocol benchmarking.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│ VPC: kiseki-perf-test                                    │
│                                                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐      │
│  │ storage-1   │  │ storage-2   │  │ storage-3   │      │
│  │ n2-std-16   │  │ n2-std-16   │  │ n2-std-16   │      │
│  │ NVMe local  │  │ PD-SSD      │  │ PD-Balanced  │      │
│  │ :9000-9102  │  │ :9000-9102  │  │ :9000-9102  │      │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘      │
│         │                │                │              │
│  ┌──────┴────────────────┴────────────────┴──────┐      │
│  │          Internal network (RoCEv2 + TCP)       │      │
│  └──────┬────────────────┬────────────────┬──────┘      │
│         │                │                │              │
│  ┌──────┴──────┐  ┌──────┴──────┐  ┌──────┴──────┐      │
│  │ client-1   │  │ client-2   │  │ client-3   │      │
│  │ n2-std-8   │  │ c3-std-8   │  │ n2-std-8   │      │
│  │ NVMe cache │  │ GPU (T4)   │  │ FUSE+NFS   │      │
│  └─────────────┘  └─────────────┘  └─────────────┘      │
│                                                          │
│  ┌─────────────┐                                         │
│  │ bench-ctrl  │  Orchestrator: runs benchmarks,         │
│  │ e2-std-4    │  collects results, generates reports    │
│  └─────────────┘                                         │
└──────────────────────────────────────────────────────────┘
```

## Quick Start

```bash
cd infra/gcp
terraform init
terraform apply -var="project_id=your-gcp-project"

# SSH to benchmark controller
gcloud compute ssh bench-ctrl --zone=europe-west6-a

# Run all benchmarks
./run-all-benchmarks.sh

# Tear down
terraform destroy
```

## Disk Configurations

| Node | Disk type | Size | Purpose |
|------|-----------|------|---------|
| storage-1 | Local NVMe (C3 SSD) | 375 GB × 2 | Best-case NVMe latency |
| storage-2 | PD-SSD | 500 GB | Standard SSD (network-attached) |
| storage-3 | PD-Balanced | 500 GB | Cost-optimized (comparison baseline) |
| client-* | PD-SSD | 100 GB | Client cache (L2) |

## Benchmarks

| Test | Protocol | Tool | Metric |
|------|----------|------|--------|
| S3 throughput | S3 HTTP | `warp` / `s3bench` | MB/s, IOPS |
| S3 latency | S3 HTTP | custom curl loop | p50/p99/p999 |
| NFS sequential | NFSv4.2 | `fio` | MB/s |
| NFS random | NFSv4.2 | `fio` | IOPS |
| pNFS parallel | pNFS | `fio` (multi-client) | aggregate MB/s |
| TCP throughput | gRPC | `transport_bench` | MB/s |
| RoCEv2 | RDMA verbs | `transport_bench` | MB/s, latency |
| FUSE POSIX | FUSE mount | `fio` + `mdtest` | IOPS, ops/s |
