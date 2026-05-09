# 2026-05-09 GCP `compact` perf — wrapper-fix verification + asymmetry probe

3-node Raft cluster (3×c3-standard-44-lssd + 2×c3-standard-44, europe-west1-b),
fresh terraform apply on a binary built from `main` with the post-2026-05-07
fix bundle:

- Wrapper false-NotFound fix (`a69e490`)
- `kiseki://` native client wiring (`64e8a67`)
- NFSv3 `mkdir` HandleRegistry fix (`b0ae487`)
- Range-aware composition decrypt (`5be6448`)
- TCP-framed default port → 9103 (`5be6448`)

Goal: verify the fixes hold under real Tier_1 wire and the indefinite NFS read
hangs from 2026-05-07 don't reproduce.

---

## Headline

| | |
|---|---|
| Wrapper-fix verified at protocol level | ✓ NFSv3 read 1 GiB completed at 67 MB/s; pre-fix this hung indefinitely. No `Io`-error needed because no peer call timed out. |
| S3 PUT scales linearly to wire | ✓ 5.4 GB/s @64∥ (84 % of Tier_1 50 Gbps) |
| S3 GET ceiling persists | ⚠ ~2 GB/s flat at 4-256∥; Fix #5 lifted single-stream 7→9.7 Gbps but didn't move the concurrency ceiling |
| FUSE-via-native (`kiseki://`) hangs at mount | **NEW BUG** — banner prints, mountpoint never registers; same shape on `http://` from a fresh client. Not the same hang as 2026-05-07. |

---

## Phase 1 — iperf3 baseline

| Streams | Throughput |
|---|---|
| 8 | 28.5 Gbps |
| 64 | 27.3 Gbps |

8-stream and 64-stream both ~28 Gbps → real wire ceiling, not CPU-bound. Tier_1
50 Gbps theoretical; practical real bandwidth on this c3-standard-44 pair is
**~28-29 Gbps**. Updates the previous run's "29.3 Gbps was 8-stream-CPU-bound"
note: the host CPU isn't the bottleneck, the wire really delivers ~28 Gbps.

---

## Phase 2 — S3 single + concurrency

**Single-stream PUT (curl -T):**

| Size | Time | Throughput |
|---|---|---|
| 1 GB | 2.20 s | 466 MB/s (3.91 Gbps) |
| 4 GB | 7.65 s | 535 MB/s (4.49 Gbps) |

**PUT concurrency sweep** (64 MiB objects × 4/stream, byte-verified at every step):

| Streams | Time | Throughput | Spot-stored |
|---|---|---|---|
| 1 | 420 ms | 610 MB/s (4.76 Gbps) | 67108864 |
| 4 | 511 ms | 2004 MB/s (15.66 Gbps) | 67108864 |
| 16 | 959 ms | 4271 MB/s (33.37 Gbps) | 67108864 |
| 64 | 3.05 s | **5367 MB/s (41.93 Gbps)** | 67108864 |
| 128 | 6.11 s | 5367 MB/s (41.93 Gbps) | 67108864 |
| 256 | 12.29 s | 5332 MB/s (41.66 Gbps) | 67108864 |

**GET concurrency sweep** (read-back of PUT'd objects, actual bytes verified):

| Streams | Time | Throughput | actual / total |
|---|---|---|---|
| 1 | 222 ms | 1153 MB/s (9.67 Gbps) | 256/256 |
| 4 | 543 ms | 1886 MB/s (15.82 Gbps) | 1024/1024 |
| 16 | 2.07 s | 1976 MB/s (16.57 Gbps) | 4096/4096 |
| 64 | 8.30 s | 1974 MB/s (16.56 Gbps) | 16384/16384 |
| 128 | 15.88 s | 2063 MB/s (17.31 Gbps) | 32768/32768 |
| 256 | 32.89 s | 1993 MB/s (16.72 Gbps) | 65536/65536 |

**PUT-vs-GET asymmetry: PUT 84 % of wire, GET 34 % of wire.** Fix #5
(range-aware decrypt) lifted single-stream GET from 7.0 Gbps (2026-05-07) to
9.7 Gbps — a real 38 % improvement at full-object size. But the high-
concurrency 16-Gbps ceiling was already there before Fix #5 and is still
there after; the asymmetry is somewhere upstack of the chunk decrypt loop.

Per the queued investigation in `2026-05-07-gcp-compact-multinode/findings.md`:
single-listener saturation on the leader, chunk-fabric fan-out cost, or HTTP/1.1
connection-per-curl overhead are the live hypotheses. A flame graph at sustained
16 Gbps GET would name the actual hot path; we ran out of cluster time to capture it.

---

## Phase 3 — NFSv3 hang test (the wrapper-fix verification)

Same dd matrix that hung on 2026-05-07 (256 MiB read indefinite wait, killed
after 30 s). With wrapper fix `a69e490` in place:

| Size | Write | Read |
|---|---|---|
| 64 MiB | 517 MB/s | 60.5 MB/s |
| 128 MiB | 522 MB/s | 67 MB/s |
| 256 MiB | 553 MB/s | 67.1 MB/s |
| 1024 MiB | **568 MB/s** | **66.9 MB/s** |

All 8 dd ops (4 sizes × write+read) completed cleanly with `wrc=0` / `rrc=0`.
**The 256 MiB indefinite-wait did not reproduce.** No peer-side timeouts surfaced
either — i.e. the wrapper's transient-error fall-through wasn't even needed for
this workload. The fix's value here is structural: even if a transient error
happened, the kernel-FUSE/NFS retries would surface as honest `Io` instead of
escalating to indefinite stalls.

NFSv3 read flat-ceiling at ~67 MB/s. Same shape as local FUSE-via-HTTP — kernel
issues 1 MiB reads serially over one NFS connection, server processes one at a
time. Not a kiseki bug per se; it's the per-connection serial-read pattern.

---

## Phase 4 — FUSE side-by-side (NEW BUG)

`kiseki-client mount --endpoint kiseki://10.0.0.10:9103 --mountpoint /mnt/kn`
prints:
```
Mounting at /mnt/kn via native kiseki://10.0.0.10:9103 (pool=16, cache_mode: bypass)
```
Then **mount(2) registers** (`mount` lists the path as `kiseki on /mnt/kn type
fuse`) but the mountpoint never becomes responsive — `stat`/`mountpoint -q`
hangs indefinitely.

Same shape on `http://10.0.0.10:9000` from a fresh client. So the bug isn't
in `NativeRemoteGateway` or the native binding — it's in the FUSE daemon itself,
common to both transports. Suspects:

1. `KisekiFuse::new` blocks unexpectedly on Rocky 9
2. `fuser::mount2` returns a session whose first FUSE_INIT/LOOKUP doesn't get a
   reply from the daemon thread
3. `fuse_daemon::mount` runtime/thread layout deadlocks on first kernel callback

Side issues uncovered while debugging:

- **TCP-framed per-peer cap collision**: server's `NATIVE_TCP_FRAMED_PER_PEER_MAX
  = 16` exactly equals the client's `DEFAULT_POOL_SIZE = 16`. A transient
  reconnect/race pushes the count to 17 and the next connection is rejected.
  Recommend either bumping server cap to e.g. 32 or dropping client default to
  e.g. 12 to leave headroom.

- **LAST-ACK socket pile-up**: when a kiseki-client process is killed (SIGKILL),
  its 16 TCP connections enter LAST-ACK and persist for the kernel `tcp_fin_timeout`
  (~60 s default). During that window the server's per-peer counter is still
  decrementing toward zero, so an immediate reconnect attempt runs into the
  per-peer cap. Affects test-iteration speed; not a production correctness
  issue but rough on dev workflow.

- **Zombie FUSE mounts**: when the kiseki-client daemon dies without
  `fusermount3 -u`, the kernel keeps the mountpoint registered as a fuse fs
  with no userspace. Subsequent mount attempts on the same path see "already
  mounted" but every op hangs. Cleanup needed: `umount -lf <path>`.

---

## What the run did NOT establish

- FUSE-via-native vs FUSE-via-HTTP throughput comparison (Phase 4 blocker).
- GET-asymmetry root cause (no flame graph captured before teardown).
- Behavior of the TCP-framed per-peer cap under sustained 256 MiB+ reads
  (would only have surfaced if Phase 4 mounted).

## What the run DID establish

- **The wrapper fix works.** No NFSv3 indefinite hangs at any size up to 1 GiB.
- **Fix #5 ships a real single-stream GET improvement** (7.0 → 9.7 Gbps).
- **Concurrent-GET ceiling is structural**, not addressed by Fix #5.
- **PUT scales linearly** to 84 % of wire.
- **A separate FUSE daemon bug** exists, manifesting as mount-then-hang, common
  to both kiseki:// and http:// transports.

---

## Suggested next steps (no GCP needed)

1. **Reproduce the FUSE daemon hang locally** with a regression test that
   mounts kiseki-client against any backend, calls a single `getattr` on the
   root, and asserts response within 5 s. The bug should reproduce on Linux
   without a network — likely in fuser-kernel-callback dispatch.
2. **Bump the per-peer cap** from 16 to 32 in
   `NATIVE_TCP_FRAMED_PER_PEER_MAX`. Cheap, removes the connect-race ceiling.
3. **GET-asymmetry investigation**: write a `kiseki-profile` driver that
   issues N concurrent GETs on the in-process gateway (no network) to see if
   the ceiling is in the gateway's `read` or somewhere upstack (S3 listener
   serialization, body streaming).

---

## Cluster cost

`terraform apply` → matrix run → `terraform destroy`: ~50 minutes wall, 3 c3-standard-
44-lssd + 2 c3-standard-44 + 1 e2-standard-4 + Tier_1 networking. Approx **$13-15**.
