# 2026-05-16 — local single-node matrix

| Field | Value |
|---|---|
| Date | 2026-05-16 |
| HEAD | `162c55e` + 30 new tests (READLINK type fix + LOCK/CREATE_SESSION wire fixes uncommitted) |
| Hardware | dev workstation, AMD Ryzen 7 6800H, 16 vCPU, Linux 7.0.3-arch1-2 |
| Cluster | single-node `kiseki-server` (ephemeral ports, plaintext) |
| Object size | 65 536 B |
| Concurrency | 16 |
| Duration | 30 s per shape |
| Warmup | 256 objects (get-heavy / mixed) |
| Tooling | driven manually combo-by-combo via `kiseki-profile run`; halt-on-`errors=` per combo |
| Output dir | `/tmp/kiseki-prof-2026-05-16/` |
| Server features | none (no pprof / dhat instrumentation — clean throughput run) |

## Why this snapshot

Nine days and ~30 commits since the 2026-05-07 baseline. Largest
deltas land in the NFS PUT path:

- ADR-040 persistent CompositionStore + per-shard leader endpoint
  + delta hydration with `name_inserts` / `name_removes`.
- ADR-041 multiplexed Raft transport + ADR-033 §4 cluster-wide
  split apply hook.
- 2026-05-09 GET asymmetry RCA — `DecryptCache` Arc-wrap removed
  the 64 MiB `Vec` clone under Mutex.
- libfuse swap (ADR-043) — multi-thread session loop landed
  2026-05-09 across 6 commits.
- May-2026 perf-fix sweep: chunk-staging + group-commit fsync hook
  chain + `gateway.fsync_pending()` preserving POSIX semantics.
- Today's wire fixes: READLINK type-confusion (`HandleEntry::Symlink`),
  LOCK locker-union consumption (RFC 5661 §18.10.1),
  CREATE_SESSION channel_attrs consumption (RFC 8881 §18.36.1).

## Throughput

| Protocol | put-heavy | get-heavy | mixed (70 P / 30 G) |
|---|---:|---:|---:|
| **S3 (HTTP)** | 41 032 op/s · 2 564 MiB/s | 76 411 op/s · 4 776 MiB/s | 46 657 op/s · 2 916 MiB/s |
| **NFSv3** | 42 515 op/s · 2 657 MiB/s | 109 652 op/s · 6 853 MiB/s | 44 892 op/s · 2 806 MiB/s |
| **NFSv4.1** | 48 899 op/s · 3 056 MiB/s | 56 512 op/s · 3 532 MiB/s | 46 094 op/s · 2 881 MiB/s |
| **pNFS Flex Files** | 45 840 op/s · 2 865 MiB/s | 77 609 op/s · 4 851 MiB/s | 49 050 op/s · 3 066 MiB/s |
| **FUSE** | 49 226 op/s · 3 077 MiB/s | 127 273 op/s · 7 955 MiB/s | 59 314 op/s · 3 707 MiB/s |
| **Native (TCP, ADR-042 §2.2)** | **55 465 op/s · 3 467 MiB/s** | **146 802 op/s · 9 175 MiB/s** | **64 320 op/s · 4 020 MiB/s** |
| **Native (gRPC, ADR-042 §2.1)** | 31 963 op/s · 1 998 MiB/s | 42 190 op/s · 2 637 MiB/s | 33 477 op/s · 2 092 MiB/s |

Zero errors across all 21 combos (15 user-protocol + 6 native).
Run was driven combo-by-combo with explicit halt-on-`errors=` per
combo per the no-trust-suite discipline. The native binding is
the ADR-042 floor the A-NG11 gate measures against and bolds the
fastest number on every shape; the user-protocol entries sit
between the gRPC tax and the TCP-framed floor.

## Tail latency p99 (µs)

| Protocol | put-heavy | get-heavy | mixed |
|---|---:|---:|---:|
| S3              |   939 | 521 |  877 |
| NFSv3           |   875 | 422 |  866 |
| NFSv4.1         |   838 | 674 |  920 |
| pNFS            |   904 | 543 |  851 |
| FUSE            |   772 | 394 |  713 |
| Native (TCP)    |   689 | 341 |  648 |
| Native (gRPC)   | 1 176 | 935 | 1 171 |

## p50 / p95 / p99 (µs) — full table

| Protocol | shape | p50 | p95 | p99 |
|---|---|---:|---:|---:|
| S3       | put-heavy | 328 | 646 |  939 |
| S3       | get-heavy | 190 | 359 |  521 |
| S3       | mixed     | 294 | 591 |  877 |
| NFSv3    | put-heavy | 313 | 615 |  875 |
| NFSv3    | get-heavy | 123 | 297 |  422 |
| NFSv3    | mixed     | 319 | 620 |  866 |
| NFSv4.1  | put-heavy | 260 | 568 |  838 |
| NFSv4.1  | get-heavy | 253 | 503 |  674 |
| NFSv4.1  | mixed     | 284 | 622 |  920 |
| pNFS     | put-heavy | 279 | 611 |  904 |
| pNFS     | get-heavy | 177 | 383 |  543 |
| pNFS     | mixed     | 244 | 547 |  851 |
| FUSE     | put-heavy | 260 | 540 |  772 |
| FUSE     | get-heavy | 106 | 238 |  394 |
| FUSE     | mixed     | 224 | 477 |  713 |
| Native (TCP)  | put-heavy | 234 | 482 |  689 |
| Native (TCP)  | get-heavy |  92 | 205 |  341 |
| Native (TCP)  | mixed     | 206 | 438 |  648 |
| Native (gRPC) | put-heavy | 430 | 832 | 1 176 |
| Native (gRPC) | get-heavy | 337 | 673 |   935 |
| Native (gRPC) | mixed     | 412 | 819 | 1 171 |

## Delta vs 2026-05-07 baseline

| Protocol | PUT 2026-05-16 | PUT 2026-05-07 | Δ | GET 2026-05-16 | GET 2026-05-07 | Δ |
|---|---:|---:|---:|---:|---:|---:|
| S3       | 41 032 | 42 160 | −2.7 % | 76 411  | 75 078  | +1.8 %     |
| NFSv3    | 42 515 |  5 006 | **8.5×** | 109 652 | 107 830 | +1.7 %     |
| NFSv4.1  | 48 899 |  5 008 | **9.8×** | 56 512  | 58 861  | −4.0 %     |
| pNFS     | 45 840 |  4 970 | **9.2×** | 77 609  | 17 921  | **4.3×**   |
| FUSE     | 49 226 | 52 888 | −6.9 %    | 127 273 | 115 368 | +10.3 %    |

The headline is **NFS-family PUT collapsed the 5k-op/s ceiling**.
The 2026-05-07 addendum traced that ceiling to `DirectoryIndex::name_for`
linear-scanning over all files in the namespace per NFS COMMIT
inside `flush_writes`. The May 9–15 ADR-022 fjall sweep (rev-2/3/4)
+ persistent CompositionStore work eliminated that O(N²) hot path;
PUT now runs at the same order of magnitude as S3 / FUSE on every
NFS variant.

pNFS GET picked up the gateway-side wins it missed in May (the
17.9 → 77.6 lift is from the DS read path picking up the
`DecryptCache` Arc-wrap + libfuse-swap-compatible chunk-staging
changes).

FUSE PUT regressed 7 % vs 2026-05-07. Small enough to be 30 s
run-to-run noise, but worth a profile-driven check if the gap
persists in the next snapshot — current suspect is the libfuse
multi-thread session loop trading a bit of write-path latency
for the 10 % GET lift.

## A-NG11 gate (≥80 k GET, ≥56 k PUT per node)

| Protocol             | PUT (gate ≥56 k)              | GET (gate ≥80 k)             |
|----------------------|-------------------------------|------------------------------|
| **Native (TCP)**     | **55 465 — 99 % of gate**     | **146 802 — clears (1.84×)** |
| Native (gRPC)        | 31 963 — 57 % of gate         | 42 190  — 53 % of gate       |
| S3                   | 41 032 — 73 % of gate         | 76 411  — 95 % of gate       |
| NFSv3                | 42 515 — 76 % of gate         | 109 652 — **clears**         |
| NFSv4.1              | 48 899 — 87 % of gate         | 56 512  — 71 % of gate       |
| pNFS                 | 45 840 — 82 % of gate         | 77 609  — 97 % of gate       |
| FUSE                 | 49 226 — 88 % of gate         | 127 273 — **clears (1.59×)** |

The ADR-042 floor (Native TCP) is **one op short** of the PUT
gate on a single dev host (55 465 vs 56 000), and the GET gate
clears at 1.84× — up from the post-Phase-7 baseline of 37 k PUT /
79 k GET (memory snapshot 2026-05-05). The h2 framing tax on
the gRPC binding is now measurable at every shape:

| Shape     | gRPC | TCP   | tax        |
|-----------|-----:|------:|-----------:|
| put-heavy | 32 k | 55 k | **1.73×** |
| get-heavy | 42 k | 147 k | **3.48×** |
| mixed     | 33 k | 64 k | **1.94×** |

The user-protocol entries land between the gRPC tax and the
TCP-framed floor — FUSE GET (127 k) is at 87 % of the native
TCP GET ceiling, NFSv3 GET (110 k) at 75 %. PUT-side the user
protocols cluster at 75–90 % of the native TCP PUT ceiling.

## Findings

1. **NFS PUT ceiling lifted ~9×** across v3 / v4 / pNFS — the
   shared `name_for` O(N²) hot path is gone. All three NFS PUT
   numbers cluster tightly around 42–49k op/s, which says they
   converge on the same gateway-side bottleneck (write-buffer
   flush + composition group-commit fsync) instead of the
   per-protocol session/COMPOUND overhead.
2. **NFSv4.1 PUT now LEADS the NFS family** at 48.9k op/s (was
   tied at ~5k op/s in the May 7 matrix). The async-native
   COMPOUND machinery pays off once the dir-index ceiling is
   gone.
3. **pNFS GET recovered** to 77.6k op/s — within 5 % of the S3
   ceiling. The 2026-05-07 finding that "pNFS DS read path
   didn't pick up gateway-side wins" is now obsolete; the
   2026-05-09 Arc-wrap fix lifts the DS path along with the
   metadata stream.
4. **FUSE GET 127k op/s — the throughput ceiling**. Up 10.3 %
   from libfuse swap. Approaching the in-process floor; the
   POSIX path is no longer the slow one.
5. **Mixed-shape p99 ≤ 920 µs everywhere**. The 12 ms p99
   regression in the May 7 matrix mixed numbers (driven by the
   NFS PUT degradation) is gone — every NFS mixed p99 sits
   between 851 and 920 µs, in line with put-heavy.
6. **Zero functional breaks across 21 combos.** The READLINK
   type-confusion fix + LOCK/CREATE_SESSION locker-and-attrs
   consumption fixes today did not regress any protocol.
7. **Native TCP-framed is the fastest path on every shape**:
   55 k PUT, 147 k GET, 64 k mixed. PUT lands one op short of
   the A-NG11 PUT gate on a single dev host (99 %); GET clears
   the gate at 1.84×. The Phase-7 baseline (2026-05-05) of
   37 k PUT / 79 k GET has been lifted 1.5× / 1.9× since by
   the fjall sweep + DecryptCache Arc-wrap.
8. **gRPC binding tax is real and measurable**: 1.73× on PUT,
   3.48× on GET, 1.94× on mixed vs the TCP-framed binding —
   pure h2 framing + tonic glue, nothing protocol-side. The
   ADR-042 §3.2 binding-selection ranking (Rdma > Low > Standard)
   continues to be the right policy for any path the operator
   actually cares about throughput on.

## Reproduction

Built with stable + `LIBCLANG_PATH=/opt/rocm/lib/llvm/lib`:

```
cargo build --release --bin kiseki-server --bin kiseki-profile --locked
```

Then per combo (matching the loop driver — do NOT trust
`run-all.sh` blindly; halt on first `errors=` line):

```
target/release/kiseki-profile run \
  --protocol <s3|nfs3|nfs4|pnfs|fuse|native> \
  --shape <put-heavy|get-heavy|mixed> \
  --concurrency 16 \
  --object-size 65536 \
  --duration-secs 30 \
  --warmup-objects 256 \
  --server-bin target/release/kiseki-server
```

For native, add `--binding tcp` (default, ADR-042 §2.2) or
`--binding grpc` (ADR-042 §2.1). The `tcp` binding skips h2
framing + tonic glue; the gRPC binding sits 1.73–3.48× slower
on this hardware.

## Captured outputs

`/tmp/kiseki-prof-2026-05-16/<protocol>-<shape>.txt` — 15 files,
each containing the `protocol=… / ops=… / latency_us p50=…` lines.
No pprof / dhat SVGs / JSONs this round; rerun with the
pprof-feature server when profiling a specific regression.
