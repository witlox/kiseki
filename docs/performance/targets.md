# Performance targets — back-of-envelope expectations

What kiseki *should* reach on a given hardware shape. Test runs measure
actual numbers; this doc says what "good" looks like so each measurement
can be passed or failed against a number rather than vibes.

Targets are derived, not aspirational — every cell here is `min(NIC,
storage, CPU, fabric) ÷ replication`. If the measurement is below the
target, something is wrong (bug, regression, contention) and should be
investigated. If a measurement *exceeds* a target, the derivation is
likely too pessimistic — update the derivation, don't celebrate.

> The **gap analysis + plan** for closing these targets on the 6-node
> `default` profile lives in [`roadmap.md`](roadmap.md). Short version:
> the writes are commit-bound (one Raft round per write, #126), not
> NIC/disk/CPU-bound — so the per-node ceilings below are not the binding
> constraint on writes until batched commit (W1) lands.
>
> The **competitive view** — these same targets compared against
> Lustre / Ceph / VAST on identical GCP hardware — lives in
> [`competitive-targets.md`](competitive-targets.md). Use it to
> sanity-check whether a re-measurement is in striking distance of
> what other well-hardened systems achieve on the same shape.

This doc is the missing layer between
[`specs/architecture/adr/042-native-gateway-data-service.md` §14](https://github.com/witlox/kiseki/blob/main/specs/architecture/adr/042-native-gateway-data-service.md)
(native binding per-node targets, single CPU class) and the
per-snapshot measurements in
[`specs/performance/`](https://github.com/witlox/kiseki/blob/main/specs/performance/INDEX.md) (actual
numbers, no targets). ADR-042 §14 is the floor; everything below
derives upward from it for real deployments.

## Methodology — how each cell is computed

For sequential bulk I/O (S3 PUT/GET, NFS write/read, FUSE seq),
per-node throughput target is:

```
target = min(
  NIC_bandwidth × utilization_factor,         # network ceiling
  N_devices × per_device_bandwidth,           # storage ceiling
  N_cores × per_core_crypto_bandwidth,        # AES-256-GCM ceiling
)
```

Cluster-aggregate target multiplies by node count, divides by the
replication / EC overhead:

```
cluster_target = (N_nodes × per_node_target) / replication_factor
```

Constants used:

| Quantity | Value | Source |
|---|---|---|
| TCP NIC utilization (single stream) | 70 % of line rate | empirical; tonic + h2 + per-syscall context switches |
| TCP NIC utilization (multi-stream, ≥ 8∥) | 90 % of line rate | observed on 2026-05-15 GCP compact (46 Gbps line, 1170 MB/s GET single client = ~25 %, scales to ~90 % at 8∥) |
| RoCEv2 utilization | 95 % of line rate | RDMA bypass eliminates kernel TCP stack, near line-rate |
| Slingshot 11 utilization | 95 % of line rate, < 2 µs p99 | Cassini NIC's hardware retransmit + ordered delivery |
| Per-NVMe sequential read | 3.2 GB/s | c3 local NVMe spec; ~5 GB/s peak read on a 375 GB partition |
| Per-NVMe sequential write | 1.6 GB/s | same partition, write half of read due to garbage collection |
| Per-HDD sequential read/write (ClusterStor E1000 spinning) | 250 MB/s | typical 7.2 k RPM enterprise HDD; ClusterStor groups 8-12 per OST |
| AES-256-GCM per core (AES-NI) | 5 GB/s | aws-lc-rs measured; varies ±20 % by µarch |
| HKDF-SHA256 per core | 2 GB/s | derive_chunk_id flame, post-fjall-sweep |
| Replication overhead R-3 | 3× writes (1× reads) | each PUT writes 3 copies; GET reads 1 |
| Replication overhead EC-4+2 | 1.5× writes (1× reads) | 6 fragments per 4 data shards |

Small-block IOPS uses a different formula — bound by the per-op
context switch + syscall + crypto cost, not bandwidth. ADR-042 §14
ships those numbers per-binding; this doc cites them.

## Status legend

| | |
|---|---|
| ✓ | Meets target (within 20 % below) |
| ≈ | Marginal — within 50 % below |
| ✗ | Far below — investigate |
| 🚧 | Blocked by an open bug; can't measure cleanly yet |
| — | Not measured yet; target documented for future runs |

---

## GCP profile: `compact` — 3 × c3-standard-44-lssd + 2 × c3-standard-44

**Cluster shape**: 3 storage (44 vCPU, Tier_1 100 Gbps egress, 8 × 375 GB NVMe = 3 TB raw/node) + 2 clients (44 vCPU, Tier_1 100 Gbps, 100 GB PD-SSD cache). R-3 replication (3 nodes; EC-4+2 not eligible).

**Per-node ceiling**:
- NIC: 46 Gbps (Tier_1 egress, measured on 2026-05-15) = 5.75 GB/s per direction
- Storage: 8 × 1.6 GB/s write = 12.8 GB/s write; 8 × 3.2 GB/s read = 25.6 GB/s read
- AES-256-GCM: 44 cores × 5 GB/s = 220 GB/s (not bottleneck — far above NIC)
- **Per-node ceiling = NIC = 5.75 GB/s sequential**

**Targets**:

`m1` = measured 2026-05-15 morning run (pre-sweep release `v2026.43.759`); `m2` = measured 2026-05-15 evening run (current main `defd8c3`, post all 9 sweep PRs — PARTIAL because phase 4 wedged).

| Op | Target (per node) | Target (cluster aggregate) | `m1` morning | `m2` evening | Status |
|---|---:|---:|---:|---:|---|
| iperf3 client → leader | 12.5 GB/s line | — | 46 Gbps (5.75 GB/s) | **46.3 Gbps** | ✓ line confirmed |
| S3 PUT 64 MB, single client | 4 GB/s | — | 1.01 GB/s | not run as 64 MB | ✗ (m1) — single-leader bottleneck |
| S3 PUT 1 MB × 200 × 16∥ single-client | (not in table — ad-hoc) | — | — | **726 MB/s** | regressed under hydrator backlog (F-1) |
| S3 PUT 1 MB serial single-client | (not in table — ad-hoc) | — | — | **103.7 MB/s** | serial: Raft-commit bound |
| S3 PUT aggregate 2 clients | — | 7 GB/s ÷ R-3 = **2.3 GB/s** | 528 MB/s | not run (multi-client) | ✗ — single-leader (m1); F-1 (m2) |
| S3 GET 1 MB, single client | 4 GB/s | — | 1.17 GB/s | not run | ≈ (m1) — read-side serialization |
| S3 GET aggregate 2 clients | — | 10 GB/s (no R-3 on reads) | — | — | — |
| NFSv4.2 write aggregate 2 clients | — | 7 GB/s ÷ R-3 = **2.3 GB/s** | 1.71 GB/s | **WEDGED** (phase 4 stalled) | ≈ (m1); 🚧 F-1 (m2) |
| NFSv4.1 (pNFS with layouts) write | — | per-DS spread × 3 nodes = **6.9 GB/s ÷ R-3 = 2.3 GB/s** | 2.92 GB/s (no layouts — env-var bug) | not reached | 🚧 |
| FUSE seq write, single client | 4 GB/s | — | 0 (GH #37 pre-fix) | **EIO** (F-1, not #37) | 🚧 F-1 |
| FUSE seq read, single client | 4 GB/s | — | 0 (GH #37 pre-fix) | not reached | 🚧 F-1 |
| FUSE 4 KB random read IOPS | 60 k IOPS | — | — | not reached | — |
| FUSE metadata (mkdir + create) | 5 k ops/s | — | 2125 ops/s | **31 ops/s** | ≈ (m1); 🚧 F-1 (m2) — 68× regression under hydrator load |
| Native gRPC PUT 64 KiB | 22 k op/s (ADR-042 §14 gate) | 66 k op/s aggregate | — | — | — (kiseki-profile not on GCP) |
| Native TCP-framed PUT 64 KiB | 60 k op/s (ADR-042 §14 gate) | 180 k op/s aggregate | — | — | — |
| S3 PUT latency 1 KB | p99 ≤ 5 ms | — | p50 7.0 ms · p99 7.5 ms | **p50 2.5 ms · p99 93.8 ms** | p50 ✓ (m2); p99 ✗ (F-1 backlog spike) |

**Why m2 is worse than m1 on almost every row**: the composition hydrator on the leader cannot drain a 30-second fio write burst before downstream phases run. m1 ran on a quieter pre-sweep binary; m2 ran on current main and hit the wall (F-1, to file). Performance numbers under hydrator backlog are not meaningful — they reflect commit-queue depth, not protocol throughput.

**Why aggregate is so far off**: today's perf-suite routes every PUT through the single leader (see write-routing posture plan). Post plan A+B+C, the 2.3 GB/s aggregate target becomes the right comparison; pre-plan, single-leader bottleneck holds the cluster at ~530 MB/s.

---

## GCP profile: `default` — 6 × c3-standard-22-lssd + 3 × c3-standard-22

**Cluster shape**: 6 storage (22 vCPU, sub-Tier_1 NIC ~21 Gbps, 4 × 375 GB NVMe = 1.5 TB raw/node) + 3 clients (22 vCPU, sub-Tier_1 ~21 Gbps, 200 GB PD-SSD cache). **EC-4+2 eligible** (6 nodes) — 1.5× write overhead, not 3×.

**Per-node ceiling**:
- NIC: 21 Gbps = 2.6 GB/s per direction
- Storage: 4 × 1.6 GB/s write = 6.4 GB/s; 4 × 3.2 GB/s read = 12.8 GB/s
- AES-256-GCM: 22 × 5 = 110 GB/s
- **Per-node ceiling = NIC = 2.6 GB/s**

**Targets**:

| Op | Target (per node) | Target (cluster aggregate) | Measured 2026-05-15 | Status |
|---|---:|---:|---:|---|
| S3 PUT 64 MB, single client | 1.8 GB/s | — | — | 🚧 GH #38 blocks all ≥ 6-node writes |
| S3 PUT aggregate 3 clients | — | 7.8 GB/s ÷ EC-1.5 = **5.2 GB/s** | — | 🚧 GH #38 |
| S3 GET aggregate 3 clients | — | 16 GB/s (no replication overhead on reads) | — | 🚧 |
| NFSv4.1 (pNFS) write aggregate | — | 6 nodes × 2.6 ÷ EC-1.5 = **10 GB/s** | — | 🚧 |
| Native TCP-framed PUT aggregate | — | 6 × 60 k = **360 k op/s** | — | 🚧 |

**Status overall**: default is unusable for write measurement until GH #38 (EC-4+2 fragment 16 MiB+8 byte cap) lands. Once that's fixed, this is the most useful profile for cluster-aggregate measurement on a budget.

---

## GCP profile: `transport` — 3 × c3-standard-88-lssd + 3 × c3-standard-44

**Cluster shape**: 3 storage (88 vCPU, Tier_1 100 Gbps, 16 × 375 GB NVMe = 6 TB raw/node) + 3 clients (44 vCPU, Tier_1 100 Gbps, 100 GB PD-SSD cache). R-3 (3 nodes).

**Per-node ceiling**:
- NIC: ~100 Gbps Tier_1 = 12.5 GB/s
- Storage: 16 × 1.6 GB/s write = 25.6 GB/s
- AES-256-GCM: 88 × 5 = 440 GB/s
- **Per-node ceiling = NIC = 12.5 GB/s**

**Targets**:

| Op | Target (per node) | Target (cluster aggregate) | Last measured (2026-05-03) | Status |
|---|---:|---:|---:|---|
| iperf3 bandwidth | 12.5 GB/s line | — | 28.6 Gbps (4 stream — under-saturates wire) | ≈ |
| S3 PUT 64 MB aggregate 3 clients | — | 37 GB/s ÷ R-3 = **12 GB/s** | 16.4 Gbps cap = 2 GB/s | ✗ (was fabric quorum-loss bug, fixed in `f362060`; re-measure pending) |
| S3 GET aggregate 3 clients | — | 37 GB/s | 110 Gbps reported (page-cache effect — see snapshot) | not trustworthy |
| Native TCP-framed PUT aggregate | — | 3 × 60 k = **180 k op/s** | — | — |

**Status overall**: re-measure pending. The 2026-05-03 numbers had the fabric Nagle bug; should re-run on this profile after the A+B+C write-routing posture lands.

---

## GCP profile: `gpu` — 3 × c3-standard-44-lssd + 2 × a2-highgpu-1g

**Cluster shape**: 3 storage (44 vCPU, Tier_1 100 Gbps, 8 × 375 GB NVMe) + 2 clients (12 vCPU + 1× A100, sub-Tier_1 NIC). R-3.

This profile is for the **cuFile / GPU-direct receive path**, not general-purpose. Targets are different from the CPU profiles:

- **GPU-direct read**: 50 GB/s per A100 (PCIe Gen4 × 16 ceiling ≈ 32 GB/s realistic). Two clients → ~64 GB/s aggregate read into GPU memory. Network is the bottleneck (each client has sub-Tier_1 NIC ≈ 32 Gbps = 4 GB/s).
- **Per-node target = NIC = 4 GB/s** for client-side ingest; storage and CPU not the bottleneck.

This profile isn't on the critical path for the current perf sweep. Re-evaluate when ADR-042's CUDA target (§14 row reserved for GPU-direct binding) is implemented.

---

## NIC layer swaps — what happens if we change the fabric

For a given cluster shape, the only fabric-dependent term is the
"NIC × utilization" component of the per-node ceiling. Everything
else (storage, CPU, replication overhead) is unchanged. The table
below shows the compact + default + transport ceilings recalculated
for three fabric classes.

### Per-node ceiling with different fabrics

| Profile | TCP (today) | RoCEv2 (same NIC, RDMA driver) | Slingshot 11 (200 Gbps Cassini) |
|---|---:|---:|---:|
| compact (Tier_1 100 Gbps) | 5.75 GB/s (46 Gbps real × 90 %) | 11.25 GB/s (100 × 90 %) | 22.5 GB/s (200 × 90 %) |
| default (sub-Tier_1 21 Gbps) | 2.6 GB/s | 2.4 GB/s (NIC is the cap) | 2.4 GB/s |
| transport (Tier_1 100 Gbps) | 12.5 GB/s | 12 GB/s (NIC line is the cap) | 22.5 GB/s |

**Reading this:**

- **RoCEv2 lifts compact significantly** (2× per-node) because today's TCP only achieves 46/100 of the Tier_1 wire. RDMA recovers the unused 54 Gbps. Requires ADR-042 ibverbs binding (currently spec-only, §14.1 procedure). No hardware change — same Mellanox CX-7-class NIC, different driver path.
- **RoCEv2 on default / transport** is bottlenecked by the NIC itself, not the TCP stack — limited lift. Worth doing only if latency p99 matters (RoCEv2 drops from p99 ~200 µs to < 5 µs).
- **Slingshot 11** is the lift HPC users care about: 4× per-node throughput on compact, 2× on transport. Requires libfabric/cxi binding (ADR-042 §14.1 procedure, no implementation yet). Hardware-dependent: GCP doesn't expose Slingshot; this is an on-prem-fabric story.
- **Latency lift** (separate from throughput): TCP p99 ≈ 200 µs round-trip; RoCEv2 p99 ≈ 5 µs; Slingshot p99 < 2 µs. For metadata-heavy workloads (small file IOPS), this matters more than bandwidth.

### What changes in kiseki to enable each

| Layer | TCP (today) | RoCEv2 | Slingshot |
|---|---|---|---|
| ADR-042 binding | `gRPC/h2` (default) + `TCP-framed-postcard` | `ibverbs` (new) | `libfabric/cxi` (new) |
| Operator pin (`KISEKI_NATIVE_TRANSPORT`) | unset → TCP-framed | `verbs` | `cxi` |
| Cluster config | nothing | DCBx + PFC + ECN tuning on switches | Cassini provider available |
| Client | `kiseki://host:9100` | same endpoint, fabric resolved via topology | same |

The wire-shape work is done in spec — ADR-042 §4 already commits to per-edge binding selection. Implementation order per `specs/architecture/build-phases.md` is gRPC → TCP-framed → ibverbs → cxi.

---

## Hypothetical: 10 × ClusterStor E1000 (spinning) + 4 × ClusterStor E1000f (flash)

A real HPC tier: 10 spinning ClusterStor E1000 nodes for cold/warm
data + 4 flash ClusterStor E1000f for hot, fronted by Slingshot 11.
This is a Lustre-shaped deployment; kiseki would replace the Lustre
software stack while keeping the hardware.

**Per-node spec (rough — actual depends on E1000 generation + SKU)**:

| Node class | Devices/node | Per-device seq | NIC | Per-node ceiling |
|---|---|---|---|---|
| E1000 spinning | 8 × 18 TB HDD + 2 × NVMe accel | 250 MB/s (HDD); 3.2 GB/s (NVMe accel) | 2 × 200 Gbps Slingshot | 2 GB/s sustained (HDD-bound), bursts to 6 GB/s through NVMe cache |
| E1000f flash | 8 × 3.84 TB NVMe | 1.6 GB/s write; 3.2 GB/s read | 2 × 200 Gbps Slingshot | 12.8 GB/s write, 25.6 GB/s read |

**Cluster aggregate (raw, before replication)**:

- Spinning tier: 10 × 2 GB/s = 20 GB/s sustained write, 30 GB/s read
- Flash tier: 4 × 12.8 GB/s = 51 GB/s write, 100 GB/s read
- **Total raw: 71 GB/s write, 130 GB/s read**

**With replication / EC**:

| Topology choice | Effective write (×) | Effective read (×) |
|---|---|---|
| Spinning R-3 + flash R-3 (uniform) | 24 GB/s write / 130 GB/s read | conservative — no benefit from spinning capacity |
| Spinning EC-8+2 + flash R-3 (mixed) | 16 GB/s spinning + 17 GB/s flash = 33 GB/s write / 130 GB/s read | needs ADR-005 §EC + tier-aware placement |
| Spinning EC-8+2 + flash EC-4+2 (cost-optimized) | 16 GB/s + 34 GB/s = **50 GB/s write** / 130 GB/s read | needs ADR-038 mirror-list to declare per-tier policy |

**NIC ceiling**: 14 nodes × 200 Gbps × 2 = 700 GB/s aggregate fabric ≫ disk-bound, so Slingshot is not the bottleneck.

**Per-protocol expectation in this deployment**:

| Op | Target | Notes |
|---|---:|---|
| S3 PUT aggregate (post A+B+C) | **40-50 GB/s** | bound by mixed-tier EC + spinning ingest; flash absorbs bursts |
| S3 GET aggregate | **100 GB/s** | flash tier serves hot data; spinning serves cold |
| pNFS write aggregate (post GH #38 + layout fix) | **40-50 GB/s** | LAYOUTGET points each NFS client at the DS that owns the target shard; flash DSes for hot namespaces, spinning for cold |
| Native gRPC PUT aggregate | **400-500 k op/s** | 14 nodes × 30 k op/s/node (lower than TCP-framed because of gRPC tax) |
| Native TCP-framed PUT aggregate | **800 k - 1 M op/s** | 14 nodes × 60 k op/s/node (ADR-042 §14 per-node gate) |
| Native libfabric/cxi PUT aggregate | **> 2 M op/s** | RDMA bypass; latency-dominant workloads (4 K random) lift 5-10× over TCP |
| FUSE 4 K random read IOPS aggregate | **2-4 M IOPS** | flash tier; FUSE direct_io + io_uring (GH #37 + #39) required to actually measure this |

**What kiseki is missing to land this scenario**:

1. **Tier-aware placement** (ADR-030 small-file placement is the right spec section; needs extension for "hot tier vs cold tier"). Today's DeviceBackend treats all devices uniformly within a node — no class differentiation across nodes.
2. **Mixed-EC policy per tier** — ADR-005 supports both R-N and EC-K+M; ADR-038 mirror-list encodes the policy. Operator-side, the policy needs to be declared per-tier (today it's per-namespace).
3. **Mixed-NIC operator pin per node-pair** — ADR-042 §4 already supports this (heterogeneous-binding clusters); operator just needs to configure.
4. **GH #38 (EC-4+2 cap)** is on the critical path — without it, EC-4+2 doesn't work at all, so the spinning tier's EC-8+2 sibling is at risk too.
5. **GH #37 (FUSE direct_io)** is on the critical path for the IOPS number — without it, the FUSE row above can't be measured.

---

## Open bugs that gate validation of these targets

| Bug | Blocks | Why |
|---|---|---|
| [GH #36](https://github.com/witlox/kiseki/issues/36) — chunk allocator fills after ~200 GB | Long-running tests on compact + default + ClusterStor | The "device full" path doesn't reclaim across runs |
| [GH #37](https://github.com/witlox/kiseki/issues/37) — FUSE O_DIRECT not exposed | All FUSE bulk-IO + 4 K random rows | Workaround `dd conv=fdatasync` gives partial signal only |
| [GH #38](https://github.com/witlox/kiseki/issues/38) — EC-4+2 cap | All ≥ 6-node measurements (default, ClusterStor, any future profile with EC) | Every write fails with "device full" — measurement impossible |
| [GH #39](https://github.com/witlox/kiseki/issues/39) — io_uring | FUSE 4 K random IOPS target | Current `spawn_blocking` floor is ~tens of k IOPS; io_uring required for ≥ 60 k |
| Write-routing posture (A+B+C plan) | Cluster-aggregate write targets across all profiles | Today routes all writes through one leader; targets assume per-shard leader spread |

## When to update this doc

- Whenever a new ADR changes a per-node ceiling (e.g. crypto algorithm swap, transport change).
- Whenever a new GCP profile is added to `infra/gcp/perf-cluster.tf`.
- Whenever a measurement consistently exceeds a target (the derivation is too pessimistic — fix it).
- Whenever a measurement consistently falls short of a target and the root cause is identified (link the issue, update the cell's status).
