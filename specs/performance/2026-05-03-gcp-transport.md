# 2026-05-03 — GCP transport-profile snapshot (partial)

**HEAD:** Pre-fjall-sweep May matrix baseline (per `INDEX.md`).
**Hardware:** 3 × `c3-standard-88-lssd` (88 vCPU, 8 × local NVMe) storage + 3 × `c3-standard-44` (44 vCPU) clients + 1 × `e2-standard-4` ctrl. `europe-west1-b` (NOT west6 — `c3-...-lssd` is west1-only). Tier_1 NIC: 100 Gbps egress on storage, ~50 Gbps on clients.
**Driver:** `infra/gcp/benchmarks/perf-suite-transport.sh` against the in-cluster ctrl. iperf3 (4 stream, 30 s) + S3 PUT/GET concurrency sweep with 64 MB objects.
**What changed since previous snapshot:** First multi-node GCP perf run since the local-matrix sweep. **Partial run** — surfaced a fabric write quorum-loss bug; numbers below are not authoritative until the bug is fixed (see Findings).

## Run timing

- Apply: ~2 min after binaries land on the GCS staging bucket
- Setup scripts: ~3 min on storage / client / ctrl
- Suite: ~3 min for sections 1-4; hung in section 5 (pNFS) until killed

## iperf3 baseline (4 stream, 30 s)

| client → storage-1 | Gbps |
|---|---:|
| 10.0.0.30 → 10.0.0.10 | 28.2 |
| 10.0.0.31 → 10.0.0.10 | 28.0 |
| 10.0.0.32 → 10.0.0.10 | 28.6 |

The 4-stream count under-saturates the 100 Gbps wire — not enough streams to compete with TCP slow-start ramp-up.

## S3 PUT concurrency sweep (64 MB objects, against the leader)

| streams | throughput |
|---:|---:|
| 1 | 1.4 Gbps |
| 4 | 4.4 Gbps |
| 16 | 10.0 Gbps |
| 64 | 11.4 Gbps |
| 256 | 16.4 Gbps (cap) |

## S3 GET sweep

| streams | throughput |
|---:|---:|
| 1 | 7.2 Gbps |
| 4 | 10.0 Gbps |
| 16 | 10.1 Gbps |
| 64 | 10.3 Gbps |
| 256 | 110.3 Gbps (page-cache effect) |

**These numbers are not trustworthy as-is.** See next section.

## Finding — fabric write quorum loss

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

So the "16.4 Gbps PUT throughput" cap is misleading: half the PUTs are 500-ing because cross-node `PutFragment` times out at the 5 s default. **The reported throughput is throughput of *successful* writes only**, not the cluster's actual write capacity.

Until the underlying cause is fixed, all GCP throughput numbers in this section should be considered indicative, not authoritative.

## Suspected cause (and follow-up)

`kiseki-server::runtime::build_fabric_channel` (`runtime.rs:104`) builds the per-peer fabric `tonic::transport::Channel` without `tcp_nodelay(true)`. Same Nagle / 40 ms-delayed-ACK problem fixed for the NFS clients in `e058ded`, but the cross-node fabric path still has it. A single-call round trip with Nagle on a 64 MB chunk involves many ack windows; combined with chunk encoding it plausibly explains the 2 s avg.

Local single-node profiling never exercised this path — single-node clusters don't fan out fragments to peers. The only way to catch this kind of bug is multi-node testing.

**Fix:** TCP_NODELAY on the fabric Channel was confirmed default-on in tonic 0.14.5 (commit `f362060` also bumped H2 flow-control window to 16 MiB stream / 32 MiB connection). Re-run pending on real hardware.

## Cross-references

- Re-run snapshot: see [2026-05-15 GCP compact](2026-05-15-gcp-compact.md) for the next GCP measurement (compact profile, different shape, surfaced different bugs).
- Commit `f362060` — fabric H2 flow-control + TCP_NODELAY fix.
