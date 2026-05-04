# GCP perf cluster — 2026-05-04 2nd-run findings + Phase A audit

Captured during the 2026-05-04 transport-profile run on a 3 × c3-
standard-88-lssd storage + 3 × c3-standard-44 client cluster in
europe-west1-b after Bugs 1+2+3 had landed (commits up to `72aef6f`).

iperf3 wire baseline: **38.7 Gbps** with 4 streams.

## Bugs found in production

| # | Surface | Symptom | Layer | Fix |
|---|---|---|---|---|
| 4 | S3 PUT >256 MB | HTTP 500 "h2 protocol error / quorum lost" | gateway | Split PUTs >`MAX_PLAINTEXT_PER_CHUNK` (64 MiB) into N chunks |
| 5 | S3 GET (any size) | HTTP 500 "CRC mismatch — corruption" on every GET (346/346) | block + chunk-store | Strict alloc + write/read extent guards + multi-extent chunks via `extra_extents` |
| 6 | NFS READ (any size) | Fixed 5.12 s per call, throughput <1 MB/s | gateway / fabric | Likely cleared by Bug 5 fix (corruption was triggering fabric fallback per read); regression-guard test bounds gateway path <200 ms |
| 7 | FUSE getattr | mtime = Jan 1 1970 | client | `to_fuser_attr` uses `SystemTime::now()` |
| 8 | FUSE READ | 180 MB/s on 38 Gbps wire (~3.7 % of wire) | client | `FuseDaemon::inner` switched from `Mutex` to `RwLock` so read-path callbacks run in parallel |
| 9 | FUSE WRITE | 158 MB/s on 38 Gbps wire | client | Per-inode dirty buffer + `flush()`/`fsync()`/`release()`; eliminates O(N²) RMW under streaming pwrites |
| 10 | NFSv3 mount | "Connection refused" before any RPC | gateway + server | Minimal portmapper (RFC 1057) on TCP/111 advertising NFS3 + MOUNT3 → 2049 |
| 11 | NFSv4.0 mount | "Protocol not supported" | spec | ADR-023 rev 4 drops NFSv4.0 from scope; supported floor is 4.1 |

## Phase A — workspace lock-poison audit

Triggered while reviewing the FUSE Mutex bug (#8). The workspace
had two contradictory patterns for handling poisoned locks:

  - 207 sites using `PoisonError::into_inner` — silent recovery
  - 97 sites using `.lock().unwrap()` — silent panic

For a storage system, **silent recovery on a data-path lock is
worse than panicking**: the structure may be in a half-applied
mutation and continuing risks reading from the wrong extent.

`kiseki-common::locks` introduces two helpers:

  - `LockOrDie::lock_or_die("name")` — for data-path locks. Emits
    `tracing::error!` then panics. The tokio runtime catches the
    task panic and propagates as `JoinError`. The lock stays
    poisoned so subsequent ops also fail loudly. The cluster's
    Raft + advisory layers route around the degraded node.
  - `LockOrWarn::lock_or_warn("name")` — for telemetry / metrics
    locks. Emits `tracing::warn!` then `into_inner` for recovery.

156 data-path recovers + 49 data-path raw unwraps + 48 control-
plane raw unwraps + 31 leftover recovers + 14 try_into expects =
~298 sites converted. `clippy::unwrap_used = "deny"` enabled
workspace-wide; `#[cfg(test)]` modules and integration test files
exempted.

## What changed for the next GCP run

  - Storage logs now emit `tracing::error!` events with `lock =
    "<name>"` fields if any data-path lock poisons — the new
    Phase A signal. Pre-fix runs would have silently recovered.
  - Server startup logs `"portmapper listening (NFS3 + MOUNT3 →
    NFS port)"` — confirms Bug 10 wiring before any client
    touches it.
  - Composition chunk count: a 384 MB PUT should produce
    `comp.chunks.len() >= 6` (64 MiB per chunk).
  - `KISEKI_PORTMAP_ADDR` defaults to `0.0.0.0:111` (privileged —
    needs `CAP_NET_BIND_SERVICE` or root); set to `disabled` to
    skip the portmapper listener.
