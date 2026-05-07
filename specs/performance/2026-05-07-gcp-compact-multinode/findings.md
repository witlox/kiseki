# 2026-05-07 GCP `compact` perf run — findings + fix plan

3-node Raft cluster (3×c3-standard-44-lssd + 2×c3-standard-44, europe-west1-b),
fresh terraform apply, manual perf matrix, then teardown. Three concurrent
investigations of the failures uncovered three distinct bugs and forced the
retraction of half the perf table I quoted at run-end.

---

## TL;DR — what I got wrong in the run summary

| Claim during the run | Reality |
|---|---|
| "S3 PUT sweep `/sweep`: 4.83→42.93 Gbps, knee at 64∥" | All `/sweep` PUTs returned **404 NoSuchBucket**. `curl -sf … &` ate the failures; the body bytes never reached the gateway. The numbers measure nothing. |
| "S3 PUT sweep `/sweep2`: same shape" | Real. Bucket pre-created + canary content-length verified before the sweep. |
| "FUSE seq write 1 GB at 885 MB/s" | **Did not use the cluster.** `kiseki-client mount --endpoint 10.0.0.10:9100` silently fell through to a local in-process `InMemoryGateway` sandbox. The 885 MB/s is a memcpy + AES-GCM seal into a `HashMap` inside the FUSE daemon's own address space. Server side saw zero traffic from this client. |
| "FUSE seq read hang — multi-node bug" | Same in-process sandbox. The hang is the 256 MB decrypt cache thrashing against a 1 GB working set with full-composition re-decrypt on every range read. No network involved. |
| "compositions.create namespace-not-found is the multi-node bootstrap bug" | The bootstrap "default" namespace was registered fine. The warnings were against the `sweep` bucket UUIDv5 — a **separate** bug in `create_bucket` rollback. |

The S3 numbers from the **second** sweep (`/sweep2`) and the single-stream PUT
to `/transport` are honest; everything else from the run table needs an
asterisk.

---

## Bug 1 — `create_bucket` doesn't roll back `state.buckets` on namespace-register failure

**Source:** `crates/kiseki-gateway/src/s3_server.rs:773-803`

`create_bucket` inserts the bucket name into `S3State::buckets` before it
calls `ensure_namespace_exists`. When the Raft emit fails (or a follower is
mid-hydration), `mem_gateway.rs:1043-1058` rolls back the namespace
registration but has no callback into the S3 layer. Subsequent
`PUT /<bucket>` short-circuits at `s3_server.rs:783-789` with
`BucketAlreadyExists` and never re-runs `ensure_namespace`, so every
`PUT /<bucket>/<key>` lands on `compositions.create → NamespaceNotFound →
404 NoSuchBucket`. The name appears taken, the namespace is missing, and the
gap is invisible to clients that aren't checking exit codes.

**Why I missed it during the run:** the perf-suite-style PUT loop uses
`curl -sf -X PUT … >/dev/null &` (`infra/gcp/benchmarks/perf-suite-transport.sh:108`).
`-sf` silently fails with exit 22 on 4xx; `&` discards the exit code. My
manual sweep inherited the same shape until I added an explicit canary HEAD.

**Fix:** call `ensure_namespace` *before* mutating `state.buckets`, and only
insert on success. As a defense-in-depth follow-up, `put_object`
(`s3_server.rs:218 / 211`) should opportunistically re-`ensure_namespace`
(idempotent on hit) so a follower mid-hydration self-heals.

**Status:** always-broken since `ensure_namespace_exists` rollback landed in
ADR-040 Phase 18. No regression test exists for this partial-failure path.

---

## Bug 2 — `kiseki-client mount` silently falls through to in-process sandbox

**Source:** `crates/kiseki-client/src/bin/kiseki_client.rs:166-209`

The client binary parses `--endpoint`. Only `http://` / `https://` URLs are
routed to `RemoteHttpGateway` (and that branch is gated on
`#[cfg(all(feature = "fuse", feature = "remote-http"))]`). Any plain
`host:port` value silently falls through to a fresh in-memory `CompositionStore`
+ `InMemoryGateway` with a private 256-bit master key. There is no network
gateway path, no error.

**Compounding factor:** `.gcp-build/build.sh:33-34` builds with
`--features ffi,fuse` only — `remote-http` is not enabled. Even an
`http://` endpoint compiles out of the network branch and lands in the same
sandbox.

**Why I missed it:** `echo hi > canary.txt && cat canary.txt` works because
both ops talk to the same daemon's in-memory state. Looks like a
functioning mount.

**Fix:** two parts.
- `kiseki_client.rs:166-209` — reject plain `host:port` with a clear error
  (`--endpoint must be http(s)://… or unix:…`); never fall through to the
  in-process sandbox unless an explicit `--in-memory` flag is set.
- `.gcp-build/build.sh` — add `remote-http` (and the native client feature
  once it's wired) to the feature set. Verify with a smoke test that
  rejects the build if a known feature flag is missing.

**Status:** always-broken in this configuration. Latent because no one
hit it before — `kiseki-profile` exercises native bindings directly, and
e2e Python tests use S3/NFS/FUSE through other entry points.

---

## Bug 3 — TCP-framed native listener port collision with advisory at 9101

**Source:** `crates/kiseki-server/src/runtime.rs:1830-1832` and
`crates/kiseki-server/src/config.rs:212-213`

Two ADRs independently picked **0.0.0.0:9101**:
- ADR-021 advisory listener (`KISEKI_ADVISORY_ADDR`, default 9101)
- ADR-042 §2.2 TCP-framed native binding (`KISEKI_NATIVE_TCP_ADDR`, default 9101)

The advisory listener wins the race by ~112 ms; TCP-framed errors out with
EADDRINUSE inside `listener.run().await`. Server keeps running on the
gRPC native binding (port 9100); the TCP-framed path is dark.

The systemd unit at `infra/gcp/scripts/setup-raw-storage.sh:79-84` doesn't
set `KISEKI_NATIVE_TCP_ADDR`, so neither default gets overridden.

**Note:** because of Bug 2, no client in this run was actually using the
native binding anyway — FUSE went through S3 HTTP at 9000. So this
collision had no observable client impact, only a startup-log error and
a latent capability gap.

**Fix:**
- Move the TCP-framed default to **0.0.0.0:9103** in `runtime.rs:1831`
  (port plan: 9100 data-gRPC, 9101 advisory, 9102 advisory-stream, 9103
  native-TCP).
- Add `KISEKI_NATIVE_TCP_ADDR=0.0.0.0:9103` to
  `infra/gcp/scripts/setup-raw-storage.sh` for explicit operator control.
- Update ADR-042 §2.2 with the new port and a cross-reference to the
  ADR-021 reservation.

**Status:** pre-existing; commit `51c48aa` (change profile default to TCP)
made it newly observable but didn't introduce it.

---

## Bug 4 — full-composition decrypt on every range read (latent, will bite once Bug 2 is fixed)

**Source:** `crates/kiseki-gateway/src/mem_gateway.rs:1257-1346` plus
`MAX_PLAINTEXT_PER_CHUNK = 64 MiB` (line 43, 1481) and decrypt cache
`MAX_CACHE_BYTES = 256 MiB` (line 239).

Any range read against a composition reconstructs the **full plaintext** of
all its chunks, then slices out the requested range. For a 1 GiB file
(16 × 64 MiB chunks) with a 256 MiB cache, only 4 chunks fit. `dd bs=1M
count=1024` issues 1024 reads; each touches all 16 chunks; FIFO eviction
ensures ~12/16 chunks per read need fresh AES-GCM decrypt. Per the agent
estimate, ≈ **786 GiB of AES-GCM work for the full 1 GiB read**. That's the
"hang" once a real network path is wired.

**Fix:** range-aware loop that only decrypts chunks intersecting
`[offset, offset+length)`. The `Composition` already carries chunk offsets,
so the rewrite is local to `mem_gateway.rs:1257-1346`. Worth a microbench
once the client path actually reaches the gateway.

**Status:** pre-existing; Bug 2 hid it.

---

## What's left of the perf table

| Phase | Result | Honest? |
|---|---|---|
| iperf3 8-stream wire baseline | 29.3 Gbps client→storage | Yes |
| S3 single-stream PUT 1 G / 4 G (`/transport`) | 569 / 568 MB/s | Yes — HEAD verified |
| S3 PUT sweep `/sweep2` (1/4/16/64/128∥) | 4.83 / 17 / 34 / 42 / 43 Gbps | Yes — canary verified |
| S3 GET sweep `/sweep2` (1/4/16/64/128∥) | 7 / 16 / 17 / 17 / 16 Gbps | Yes — actual bytes confirmed |
| FUSE write 885 MB/s | **Retracted** — in-process sandbox | No |
| FUSE read hang | **Retracted** — local decrypt-cache thrash | No |
| NFSv3 EBADHANDLE | Real symptom; root cause **unknown** (NFS-side handle registry, not the namespace bug). Needs a separate dig — agent flagged `crates/kiseki-gateway/src/nfs_ops.rs:42-178` and per-export `HandleRegistry` as the likely site. |

---

## Actual vs theoretical, asymmetry

Wire ceiling on Tier_1 c3-standard-44: **50 Gbps**. iperf3 8-stream
measured 29.3 Gbps — that's the CPU ceiling at 8 streams, not the wire.
At higher concurrency (the S3 64∥ run hit 42 Gbps, comparable to other
GCP perf reports of ~80% of Tier_1) we're closer to wire.

| Path | Best observed | % of 50 Gbps wire |
|---|---|---|
| S3 single-stream PUT | 4.77 Gbps | 9.5% |
| S3 single-stream GET | 7.0 Gbps | 14% |
| S3 PUT 64∥ | 42.2 Gbps | 84% |
| S3 GET 4∥+ (flat) | 17 Gbps | **34%** |

PUT scales linearly to wire. GET caps at one-third of wire and won't
budge past 4 streams. The asymmetry is real and worth a dedicated
investigation:

**Hypotheses to test (separate from the bugs above):**
1. **Single-listener bottleneck.** All client traffic lands on the leader's
   one `axum`/`hyper` S3 service. PUT can fan out internally (chunks +
   replication async); GET has to gather chunks back (potentially across
   3 nodes via fabric) and stream them out one HTTP body at a time.
2. **HTTP/1.1 connection reuse vs new-connection-per-curl.** Each PUT
   stream reuses keep-alive after the first request; each GET in our
   sweep used a fresh `curl` invocation = fresh TCP + TLS-less handshake.
   Plausible factor in the single-stream gap (PUT 569 / GET 834) but not
   in the multi-stream cap.
3. **Cross-node fragment fetch on GET.** With Replication-3 the leader's
   local copy should serve every read — but if chunk placement is
   sticky to a non-leader peer, the leader pays a `GetFragment` round
   trip per chunk. Worth a wire-trace next run.
4. **gRPC fabric flow control.** May 2026 fix bumped H2 windows to
   16 MiB stream / 32 MiB conn (commit `f362060`), but if GETs use
   different message sizes than PUTs the windows may be sized wrong
   for the read path.
5. **`mem_gateway` read-path hot lock.** The view-store RwLock comment
   at `runtime.rs:1027-1029` says reads take a read lock per gateway
   read. Worth flame-graphing under sustained 16 Gbps GET load.

---

## Fix plan, priority order

| # | Fix | Why this priority | Cost |
|---|---|---|---|
| 1 | `kiseki-client mount` rejects plain `host:port` | Blocks every real-cluster FUSE/native test until landed; without it any "perf number" is suspect | Small — ~30 line edit + error string |
| 2 | `.gcp-build/build.sh` adds `remote-http` (and any other client features) | Same blocker for the GCP rig | Trivial — feature-flag string |
| 3 | `create_bucket` reorders `ensure_namespace` before `state.buckets` insert | Silent 404s under multi-node bucket churn break any S3 perf run | Small — order of two operations |
| 4 | TCP-framed default port → 9103 + `KISEKI_NATIVE_TCP_ADDR` in systemd unit | Server starts cleanly; native path actually reachable | Small — port number + one env var |
| 5 | Range-aware composition decrypt in `mem_gateway.rs:1257-1346` | Once #1 lands, large-file reads will cliff hard without it | Medium — needs careful range-math + tests |
| 6 | GET-path asymmetry investigation (read-only first: flame-graph + wire-trace) | Drives the next ADR/fix | Medium — needs another GCP run |
| 7 | NFSv3 `HandleRegistry` audit (`nfs_ops.rs:42-178`) | EBADHANDLE on multi-node mkdir/create blocks the NFS perf path | Medium — needs an NFS-savvy reader |

Items 1-4 are bug fixes that should land before the next perf run. 5-7 are
performance work that follows.

---

## Followup investigations queued

- **GET 17 Gbps ceiling — re-measure first, then flame-graph.** Fix #5
  (range-aware decrypt) reduced read-path memcpy by N× for large
  objects; the next GCP run will produce different numbers and may
  partially close the gap by itself. *After* the re-measurement,
  attach a flame graph if the ceiling persists.
- **FUSE perf on a real client path.** Now unblocked: Fix #1+#2 land
  the binary's network branch. After the next GCP build, redo the
  matrix.
- **S3 PUT 64∥ residual gap.** 42 Gbps vs 50 Gbps Tier_1 — re-run
  iperf3 at 64+ streams (not 8) to get a real wire ceiling, then
  compare. Last run's 29.3 Gbps was 8-stream-CPU-bound, not wire.

---

## Status of fix landings (2026-05-07 evening)

| # | Item | Status |
|---|---|---|
| 1 | `kiseki-client mount` rejects plain `host:port` | **landed** — `crates/kiseki-client/src/bin/kiseki_client.rs`. Plain `host:port` now exits 2 with a clear error; sandbox is opt-in via `--in-memory`. Builds without `remote-http` reject network endpoints up front. |
| 2 | `.gcp-build/build.sh` adds `remote-http` | **landed** — both `cargo build` invocations now pass `ffi,fuse,remote-http`. |
| 3 | `create_bucket` reorder | **landed** — `crates/kiseki-gateway/src/s3_server.rs:767-820`. `ensure_namespace` runs before `state.buckets.insert`; on success the insert returns `false` for a concurrent create racing on the same name (409). Skipped the opportunistic `put_object` ensure: only the leader serves S3 today, so the multi-leader race the agent flagged isn't reachable yet. |
| 4 | TCP-framed default port → 9103 | **landed** — `runtime.rs:1830-1837`, `probe.rs` doc, `harness.rs` doc, ADR-042 §2.2 + banner, `setup-raw-storage.sh` `KISEKI_NATIVE_TCP_ADDR`, `perf-cluster.tf` firewall. |
| 5 | Range-aware composition decrypt | **landed** — `crates/kiseki-gateway/src/mem_gateway.rs:1252-1380` plus `copy_chunk_range_into` helper. 6 new unit tests cover within-chunk, cross-boundary, beyond-first-chunk, EOF, zero-length, and over-read cases. The pre-fix path reconstructed the full plaintext for every range read; the new path touches only the chunks intersecting `[start, end)` and pre-allocates the output. |
| 6 | GET-path asymmetry | **investigation refined** — see plan below. Re-measure on the next GCP run since Fix #5 changes the read path enough that the asymmetry numbers will move. |
| 7 | NFSv3 `HandleRegistry` audit | **landed (mkdir leg)** — `crates/kiseki-gateway/src/nfs_ops.rs`. Root cause: `mkdir` minted a 32-byte handle and inserted into `dir_index` (for listing) but never registered it in `HandleRegistry::handles`. Every kernel follow-up GETATTR on the new fh hit the missing-handle branch and returned `NFS3ERR_BADHANDLE` (kernel errno 521). Added `HandleEntry::Directory` variant + `register_dir_handle` + `is_directory`; `mkdir` now registers; `lookup_by_name` reports `FileType::Directory` for dir handles. New regression test in `nfs3_server::tests`. **Out of scope**: nested-dir routing (parent fh isn't propagated through CREATE / WRITE; the namespace is still flat) — track separately. |

### Test impact

`cargo test -p kiseki-gateway --lib` now passes 281 tests (was 274) +
1 pre-existing race (`retry_budget_env_override_is_honored` flakes
when the global `OnceLock` was already initialized by a prior test;
passes in isolation, unrelated to this work).

### Refined GET-asymmetry plan for next cluster run

Running ahead of any code change, take these measurements first:
1. **iperf3 64+ stream baseline**, both directions. Last run's 29.3 Gbps
   was 8-stream and CPU-limited; we need an unambiguous wire ceiling
   to compare 42 Gbps PUT and 17 Gbps GET against.
2. **S3 GET sweep with a single warm connection.** Reuse one HTTP
   connection across the 4-128 ranges instead of one curl per request.
   Tools: `wrk` or a custom tokio loadgen. Distinguishes connection-
   setup overhead from body-streaming bandwidth.
3. **S3 GET sweep with single TCP keepalive vs forced new-connection.**
   curl's default behavior is per-process new connection; an `mc`-style
   client reuses. Splits hypothesis #2 (HTTP/1.1 reuse) from the rest.
4. **flame-graph the leader at sustained 16 Gbps GET.** `perf record -g`
   for 30s. Look for: (a) AES-GCM decrypt as a fraction of CPU, (b)
   memcpy weight (alloc + Vec growth in `read`), (c) any gRPC fabric
   round-trips for chunk fetches on read.
5. **chunk placement audit.** On the leader, log which node owns the
   primary copy of each chunk in a 64 MB PUT. If reads always fan out
   to peers via the chunk-cluster fabric (gRPC), the cap may be the
   fabric's H2 flow control, not the S3 listener.

Hypotheses to confirm/reject with the data above:
- (H1) GET pays a buffer-copy tax PUT doesn't. **Mostly addressed by
  Fix #5** — re-measure first.
- (H2) HTTP/1.1 connection-per-curl. Plausible single-stream factor;
  step 2-3 above isolate it.
- (H3) Cross-node fabric fetch on every read. Step 5 isolates it.
- (H4) Single-listener saturation on the leader. Step 4's flame
  graph distinguishes per-core load from a true axum/hyper bottleneck.
