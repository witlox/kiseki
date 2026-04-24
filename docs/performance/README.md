# Performance Tests

Benchmark results for kiseki on GCP infrastructure.

## Test Environment

| Component | Spec |
|-----------|------|
| **HDD nodes** (3) | n2-standard-16, 3 x PD-Standard 200GB each |
| **Fast nodes** (2) | n2-standard-16, 2 x local NVMe + 2 x PD-SSD 375GB |
| **Client nodes** (3) | n2-standard-8, 100GB SSD cache |
| **Ctrl node** (1) | e2-standard-4, orchestrator |
| **Network** | GCP VPC, single subnet 10.0.0.0/24 |
| **Region** | europe-west6-c (Zurich) |
| **Raft** | Single group, 5 nodes, node 1 bootstrap |
| **Release** | v2026.1.332 |

## Results (2026-04-24)

### Network Bandwidth

| Path | Throughput |
|------|-----------|
| Client → Leader (n2-standard-8 → n2-standard-16) | 15.2 - 15.3 Gbps |
| HDD → Fast cross-tier (n2-standard-16 → n2-standard-16) | 18.3 - 20.4 Gbps |

### S3 Gateway

All S3 tests run from client nodes (n2-standard-8) with 8-way parallelism.

#### Write Throughput (single client → leader)

| Object Size | Count | Parallelism | Time | Throughput |
|-------------|-------|-------------|------|------------|
| 1 MB | 200 | 8 | 1,640 ms | 122.0 MB/s |
| 4 MB | 50 | 8 | 246 ms | 813.0 MB/s |
| 16 MB | 25 | 8 | 350 ms | 1,142.9 MB/s |

#### Read Throughput

| Object Size | Count | Parallelism | Time | Throughput |
|-------------|-------|-------------|------|------------|
| 1 MB | 200 | 8 | 176 ms | 1,136.4 MB/s |

#### PUT Latency (1 KB objects, sequential)

| Percentile | Latency |
|------------|---------|
| p50 | 7.6 ms |
| p99 | 8.8 ms |
| avg | 7.7 ms |
| max | 10.3 ms |

#### Aggregate Write (3 clients, parallel)

| Workload | Time | Aggregate Throughput |
|----------|------|---------------------|
| 3 x 100 x 1 MB (8 concurrent/client) | 2,263 ms | 132.6 MB/s |

### NFS / pNFS / FUSE

Not yet tested on GCP. NFS mount from client nodes requires SSH key
distribution from the ctrl node (OS Login configuration pending).
FUSE requires the kiseki-client binary installed on client nodes.

Local testing (3-node cluster on localhost) confirms all protocols
functional via unit and integration tests.

### Prometheus Metrics

Gateway request counters showed 0 during the test. The
`requests_total` atomic counter in `InMemoryGateway` is not wired
to the Prometheus metrics exporter yet.

## Local Test Results (same binary, localhost)

For comparison, local 3-node cluster results (loopback network,
no disk I/O latency):

| Test | Result |
|------|--------|
| S3 Write 1 MB x 200 (32 parallel) | 39.5 MB/s |
| S3 Write 4 MB x 50 (32 parallel) | 337.3 MB/s |
| S3 Write 16 MB x 25 (8 parallel) | 346.6 MB/s |
| S3 Parallel 3 x 100 x 1 MB (96 parallel) | 135.3 MB/s |
| S3 Latency 1 KB | p50: 35 ms, p99: 39 ms |
| S3 Read 1 MB x 200 (32 parallel) | 917.4 MB/s |

## Observations

1. **Write throughput scales with object size.** 1 MB writes are
   bottlenecked by per-object Raft consensus overhead (~8 ms per
   round-trip). 16 MB writes amortize this cost, reaching 1.1 GB/s.

2. **Read throughput exceeds write.** Reads bypass Raft consensus
   (served from the local composition + chunk store) and hit 1.1 GB/s
   even for 1 MB objects.

3. **GCP outperforms localhost for large objects.** The GCP network
   (15+ Gbps) and n2-standard-16 nodes have more bandwidth than
   localhost loopback under contention. 16 MB writes: 1,143 MB/s
   (GCP) vs 347 MB/s (local).

4. **Latency is network-bound.** p50 latency on GCP (7.6 ms)
   includes network RTT + Raft consensus (3-node quorum). Local
   p50 is 35 ms due to higher contention on shared CPU.

5. **Single Raft group is the write bottleneck.** All writes go
   through one leader. Multi-shard deployment would distribute
   leaders across nodes, scaling write throughput linearly.

## Known Issues

- **Concurrent write deadlock (fixed).** Blocking redb I/O in the
  Raft state machine `apply()` path starved the async runtime under
  concurrent load. Fixed by: `block_in_place` in S3 handlers +
  dynamic Raft runtime thread count (`KISEKI_RAFT_THREADS`, default
  = CPUs/2). Proper fix: `spawn_blocking` for redb writes in
  `apply()`.

- **NFS mount on GCP.** Requires SSH key distribution from ctrl to
  client nodes. The ctrl service account needs `osAdminLogin` role
  and OS Login key registration.

- **Prometheus counters.** `gateway_requests_total` not exported to
  `/metrics` endpoint.

## Running the Benchmark

```bash
# Local 3-node test
cargo build --release --bin kiseki-server
# Start 3 nodes (see examples/cluster-3node.env.node{1,2,3})
# Run: bash infra/gcp/benchmarks/perf-suite.sh

# GCP deployment
cd infra/gcp
terraform apply -var="project_id=PROJECT" -var="zone=ZONE" \
  -var="release_tag=v2026.1.332"
# Deploy perf-suite.sh to ctrl node and run
```

See `infra/gcp/benchmarks/perf-suite.sh` for the full benchmark
script and `infra/gcp/benchmarks/run-perf.sh` for the local
deployment wrapper.
