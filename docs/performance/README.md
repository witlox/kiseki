# Performance

Last refreshed: **2026-05-07** (local matrix re-run on HEAD = `51c48aa`,
after the FUSE-via-TCP-framed wiring + NFS async-native conversion
+ ADR-022 fjall sweep). The 2026-05-03 baseline is preserved below for
comparison.

> Operators tuning a deployment for throughput should also read
> [`docs/operations/durability.md`](../operations/durability.md) —
> the group-commit flags described below trade durability for
> throughput, and the matrix in that doc spells out the loss
> windows under each failure mode.

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

## Local matrix — 2026-05-07 refresh

Re-run on HEAD = `51c48aa` after ~20 perf commits landed since the
2026-05-03 snapshot (TCP-framed default, FUSE-via-TCP wiring, NFS
async-native server, sharded composition store, fjall sweep, V3
wire format, mem-gateway zero-copy GET). Same configuration as the
older matrix (single-node `kiseki-server`, 64 KiB, c=16, 30 s,
warmup=256). CPU phase via pprof; numbers below are CPU-phase
throughput.

### Throughput

| Protocol | put-heavy | get-heavy | mixed (70 P / 30 G) |
|---|---:|---:|---:|
| **S3 (HTTP)** | 42 160 op/s · 2.6 GiB/s | 75 078 op/s · 4.7 GiB/s | 47 584 op/s · 2.9 GiB/s |
| **NFSv3** | 5 006 op/s · 313 MiB/s | **107 830 op/s · 6.7 GiB/s** | 6 618 op/s · 414 MiB/s |
| **NFSv4.1** | 5 008 op/s · 313 MiB/s | 58 861 op/s · 3.7 GiB/s | 6 634 op/s · 415 MiB/s |
| **pNFS Flex Files** | 4 970 op/s · 311 MiB/s | 17 921 op/s · 1.1 GiB/s | 6 453 op/s · 403 MiB/s |
| **FUSE** | **52 888 op/s · 3.3 GiB/s** | **115 368 op/s · 7.2 GiB/s** | **61 230 op/s · 3.8 GiB/s** |

### Tail latencies (p99 µs, c=16)

| Protocol | put-heavy | get-heavy | mixed |
|---|---:|---:|---:|
| S3 | 901 | 525 | 831 |
| NFSv3 | 12 652 | 410 | 11 889 |
| NFSv4.1 | 12 582 | 630 | 11 913 |
| pNFS | 12 476 | 1 177 | 13 308 |
| FUSE | 705 | 412 | 668 |

### Delta vs 2026-05-03 baseline

| Protocol | PUT now / was / Δ | GET now / was / Δ |
|---|---|---|
| S3 | 42 160 / 7 124 / **5.9×** | 75 078 / 25 843 / 2.9× |
| NFSv3 | 5 006 / 2 042 / 2.5× | 107 830 / 26 615 / **4.1×** |
| NFSv4.1 | 5 008 / 8 327 / **0.6× ↓** | 58 861 / 27 291 / 2.2× |
| pNFS | 4 970 / 8 327 / **0.6× ↓** | 17 921 / 16 549 / 1.1× |
| FUSE | 52 888 / 2 790 / **19×** | 115 368 / 10 789 / **10.7×** |

### Findings

1. **FUSE is now the fastest path on every shape.** TCP-framed
   wiring (`29a6a35`) + 3-phase RwLock (`6035bab`) + bypass of the
   KisekiFuse runtime detour (`c10cc65`) compounded into a 10–19×
   lift. FUSE clears both A-NG11 gates (≥80 k GET, ≥56 k PUT)
   on a single host.
2. **NFSv3 GET (107 k op/s) is the throughput ceiling on this
   host.** NFSv4 GET at 58 k is well behind v3 — the v4 session
   machinery costs ~2× per op even after the async-native rewrite.
3. **NFS-family PUT measures ~5 k op/s but it is run-time
   degradation, not a structural regression.** The matrix's 30 s
   duration captures the degraded end of an O(N²) curve in
   `DirectoryIndex::name_for` (`crates/kiseki-gateway/src/
   nfs_dir.rs:66`) — a linear scan over all files in the
   namespace, called once per NFS COMMIT. Standalone NFSv3 PUT
   c=16 starts at 9 970 op/s (10 s), halves to 4 984 op/s by
   30 s, drops to 3 394 op/s at 60 s. v3 / v4 / pNFS all
   converge on the same shared ceiling (~10 k op/s at startup,
   uniform across protocols). The May 3 baseline's 8 327 op/s
   NFSv4 number was also a degraded-state measurement; the 8 k
   → 5 k delta reflects worse degradation rate (likely fjall
   journal growth compounding) rather than a steady-state regression.
   See addendum in `specs/performance/2026-05-07-local-matrix.md`
   for the full investigation. Fix tracked in Open issues.
4. **pNFS GET barely moved** (16 549 → 17 921). Every other GET
   path gained 2–10×. The pNFS DS data path didn't pick up the
   gateway-side wins — likely a frame copy or sync mutex on the DS
   server that the other read paths shed.
5. **A-NG11 gate** (≥80 k GET, ≥56 k PUT per node, single host):
   FUSE clears both. S3 misses both narrowly (PUT 42 k, GET 75 k).
   Native binding wasn't in this matrix — `run-all.sh` doesn't
   include `--protocol native` yet.

### Captured profiles

- `/tmp/kiseki-prof/cpu-{protocol}-{shape}.svg` — pprof flamegraphs
- `/tmp/kiseki-prof/heap-{protocol}-{shape}.json` — dhat heap

(Heap-phase op/s numbers in the script output are dhat-instrumented
and not throughput-representative. Use them for heap analysis only,
via dh_view.html.)

---

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

## ADR-042 native gateway data service — local matrix (2026-05-05)

The Phase 7 work added `--protocol native` to `kiseki-profile`. This
section records the first end-to-end measurement on the
single-node plaintext harness and compares it to the in-process
floor (Phase 8 of `specs/implementation/adr-042-native-gateway.md`).

### Configuration

| | |
|---|---|
| Machine | dev workstation (Linux, x86_64, 16 cores) — same as the May matrix |
| Object size | 64 KiB |
| Concurrency | 16 |
| Duration | 10 s |
| Warmup | 64 objects pre-created for get-heavy |
| Cluster | single-node `kiseki-server` (plaintext data port; SanInterceptor falls through to the synthetic "dev" tenant) |

### Throughput

| Protocol | put-heavy | get-heavy |
|---|---:|---:|
| **InProcess (floor)** | 216 212 op/s · 13.5 GiB/s | 218 660 op/s · 13.7 GiB/s |
| **S3 HTTP** | 8 260 op/s · 516 MiB/s | 48 862 op/s · 3.05 GiB/s |
| **Native gRPC (ADR-042)** | 7 373 op/s · 461 MiB/s | **12 293 op/s · 768 MiB/s** |

### A-NG11 gates

A-NG11 commits to **≥80 k op/s GET, ≥56 k op/s PUT per node** on the
profile harness. The above run shows:

- GET: 12 293 op/s — **15.4 % of the gate** (gate not cleared)
- PUT: 7 373 op/s — **13.2 % of the gate** (gate not cleared)

ADR-042's status remains **Proposed** — A-NG11 is not yet
satisfied. The wire shape, auth boundary, and feature surface are
in place (Phases 2-6), but the gRPC tax on this single-host config
is far higher than the targets allow. Specifically:

- Native GET runs at **25 % of S3 HTTP GET** on the same workload.
  The gRPC tax should be lower than HTTP's tax, not higher — there
  is a real bottleneck on the get path.
- Native PUT runs at **89 % of S3 HTTP PUT** — close to parity, so
  the issue is concentrated on the read side.

### Where the GET tax lives (next-investigation candidates)

Without a fresh flamegraph the analysis is informed-guess level —
the @perf @smoke BDD scenario in `native-gateway.feature` will
land the rigorous attribution once it has a step driver. Concrete
suspects from code inspection:

1. **Per-call codec setup**: every call clones the channel and
   constructs a fresh `GatewayDataServiceClient` with
   `max_decoding_message_size(64 MiB)`. The codec config touches
   tonic-internal fields per call; a process-wide pre-built client
   would eliminate that. Estimated cost: 1-3 µs / call.
2. **UUID `parse_str` per request**: `OrgId` /
   `NamespaceId` / `CompositionId` arrive as proto `string value`
   fields and the handler runs `uuid::Uuid::parse_str` three times
   per call. The wire shape is fixed (proto3 contract) but the
   handler could intern parsed UUIDs in a small per-stream cache
   if the same tenant/namespace dominates a session. Estimated
   cost: ~150 ns / call — small per call but measurable at >10 k
   op/s.
3. **HTTP/2 vs HTTP/1.1 framing**: tonic's HTTP/2 with
   `initial_stream_window_size = 16 MiB` should be FASTER than
   HTTP/1.1 keepalive, not slower. The 4× regression vs S3 hints
   at something specific to tonic's per-call work — possibly
   HEADERS frame compression overhead under high message rate.
4. **InterceptedService dispatch**: every native RPC pays
   `SanInterceptor::intercept` which reads `TlsConnectInfo` and
   stashes a `CanonicalSanUri` clone (the OnceLock cache landed in
   `5c9ef9b`, but the `req.extensions_mut().insert(...)` itself
   takes a TypeMap insert per call).

### Targeting Phase 9 (perf optimization slice)

The path from 12 k → 80 k op/s GET is concrete:
- Land a flamegraph capture against the harness server while the
  native driver is in steady-state (KISEKI_PPROF_OUT supports this
  on `--features pprof` builds).
- Audit the four candidates above against the flame.
- Iterate per-candidate, re-running the matrix.

Until that work lands, the wire-shape and security surface of
ADR-042 are validated (Phases 2-6 + the Phase 7 driver) but the
perf gate (A-NG11) blocks the ADR `Accepted` flip.

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

- [x] **`DirectoryIndex::name_for` is O(N)** — fixed 2026-05-07.
  Replaced per-namespace `HashMap<String, DirEntry>` with a
  `NamespaceDir { by_name, by_handle }` pair so `name_for` is
  O(1). NFSv3 PUT c=16 went 5 k → 45 k op/s steady-state (8.8×
  at 30 s, 12.3× at 60 s); NFSv4 / pNFS to ~52 k op/s (10.4×).
  See `specs/performance/2026-05-07-local-matrix.md` addendum.
- [ ] **NFS v3 vs v4 gap** — post-fix v3 sits ~17 % below v4
  (45 k vs 52 k op/s on PUT). v4 is a more complex protocol
  on paper; gap suggests v3-specific overhead worth a flame.
- [ ] **pNFS GET stagnation** — only 17 921 op/s vs FUSE's 115 k
  and NFSv3's 107 k on the same workload. Every other GET path
  picked up the gateway-side wins; pNFS DS data path didn't.
  Likely a frame copy or sync mutex on the DS server.
- [ ] **`run-all.sh` missing `--protocol native`** — the harness
  doesn't include the ADR-042 native binding in the matrix, so
  every refresh has to run native separately. Add it to
  `PROTOCOLS=(s3 nfs3 nfs4 pnfs fuse native)`.
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
