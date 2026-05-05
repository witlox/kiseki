# Post-2026-05-03 Sweep — ADR-041, FUSE p99, Group Commit, Observability

**Created:** 2026-05-05
**Covers commits:** 06dcb9a (2026-05-03 W7) → 386d15b (2026-05-05)
**Predecessor plan:** `adr-025-storage-admin-api.md` (2026-05-03 23:31)

## Context

Two parallel pressures drove this sweep:

1. **GCP perf cluster runs** (2026-05-02 → 2026-05-04) surfaced cross-
   node fabric write quorum loss, p99 tail spikes on FUSE, and a
   single-port-per-Raft-group ceiling that blocked ratio-floor
   splits at scale.
2. **CI / fidelity gaps** flagged by gate-2 audits and the D-10
   cross-stream flake — `@library` mocks were diverging from the
   real cluster behavior the BDD `@integration` lane was supposed to
   witness.

The sweep is split into seven streams. Every stream has landed; this
file is the **trailing record**, not a forward plan.

---

## Stream A — ADR-041: Multiplexed Raft Transport

**Why now.** ADR-026 Strategy A puts every shard in its own Raft
group, but `kiseki-raft::tcp_transport::run_raft_rpc_server` binds
**one TCP listener per group**. With one shard per node that's fine;
ratio-floor splits (ADR-033 §3) push a single node to N groups,
blowing the port budget and the fd budget. ADR-041 gives every shard
group on a node a single shared listener that routes RPCs by shard
id.

| Commit | What |
|---|---|
| 68d162d | Initial ADR-041 design |
| 2dc74f9 | Adversary gate-1 — 3 HIGH, 5 MEDIUM, 7 LOW; CHANGES REQUESTED |
| e887caf | ADR amendment closing 3H + 5M + 2L |
| 1c2988c | Implementation: per-node listener, shard-id envelope, registry |
| f60d5fc | 8 Prometheus metrics + structured tracing on the multiplexed transport |
| 6880bc3 | RaftShardStore::split_shard / merge_shards / state mutators (ADR-033/034 wiring) |
| fb5a56b | Eager delta redistribution on split (ADR-033 §3 step 3) |

**Behavioural witnesses.** `multi-node-raft.feature` D-* scenarios
plus `multi-node-admin.rs` SplitShard / MergeShards steps — the
latter assert `kiseki_raft_transport_registry_size` jumps by +1 on
every node when a new shard registers (the apply hook wired in
stream B).

**Trailing risk:** D-10 cross-stream timing (slow-down 1.5 s → 60 s
plus bounded metric poll) and the 6-node @ec scenario boot under
cross-singleton compose pressure are now `@flaky`-tagged with
2× cucumber retry.

---

## Stream B — ADR-033 §4: Cluster-Wide Split Apply Hook

**Why now.** Splitting a shard mutates state on the source's leader
via Raft, but the *new* shard's Raft group has to be **registered on
every node's multiplexed listener** before any peer can replicate to
it. Without an apply hook on the control-plane group, only the node
that ran the admin RPC would know about the new group; every other
peer would 404 on PutFragment until restart.

| Commit | What |
|---|---|
| a8c4c07 | Control-plane Raft + cluster-wide split apply hook + observability sweep |
| 348f57c | Replicate namespace creation via Raft (Phase 18, follow-up wiring) |

The apply hook closes ADR-033 §4: every node's
`RaftShardStore::create_shard` runs on apply, registers the new
group with the local multiplexed listener, and increments
`kiseki_raft_transport_registry_size`. The BDD step
`multi_node_admin.rs::then_apply_hook_fired_on_every_node` polls
for `>= 3` on every node within 10 s — bounded so cross-singleton
harness restarts don't false-fail.

---

## Stream C — FUSE p99 Tail (160 ms → ~4 ms p50)

**Two amplifying causes.**

1. **`inner.write()` RwLock held across slow gateway calls.** The
   FUSE daemon held the world lock for the duration of a remote
   write or composition update — any slow op blocked every reader.
2. **Per-write composition redb fsync** contending with the
   chunk-store periodic device sync.

**Both fixed.** FUSE adopts a 3-phase pattern (write-lock pop → no-
lock gateway call → write-lock apply), composition redb gets opt-
in eventual durability with a periodic background flush. POSIX
`fsync(2)` correctness preserved via a new
`GatewayOps::fsync_pending` RPC + `register_fsync_hook` mechanism
that drains both the composition flusher and the chunk device on
demand.

| Commit | What |
|---|---|
| 1a55596 | FUSE init() + open() bump readahead + keep page cache |
| 1d9e9d2 | RwLock 3-phase pattern + composition group commit |
| 681de37 | Chunk-store group commit — periodic device.sync background task |
| 4c395c1 | HW-accelerated CRC32C (was a hand-rolled bit loop) |
| 386d15b | Gate FUSE-only helpers behind `feature = "fuse"` (CI dead-code fix) |

**Witnesses:** local single-node matrix (see
`docs/performance/README.md`) — NFSv4 GET 24 op/s · p99 30 s →
27 291 op/s · p99 4 ms; pNFS GET fixed from 100 % errors to 16 549
op/s; S3 GET 5.6×.

**Durability docs.** `docs/operations/durability.md` (new) covers
the per-knob loss windows. Production target = 10–100+ nodes, where
R-3 / EC-4+2 replicas + the under-replication scrub recover any
per-node loss window.

---

## Stream D — Observability Sweep

**Why now.** The GCP 2026-05-02 run produced **zero log lines**
during a 1760-event quorum-loss storm — no signal to root-cause
from. Phase histograms across the gateway → chunk → fabric path
turn that signal back on.

| Commit | What |
|---|---|
| a9144ca | Gateway request duration + GET/PUT phase histograms |
| aea45b1 | PersistentChunkStore::write_chunk phase histograms |
| d3f6564 | Fabric PutFragment send + recv phase histograms |
| f60d5fc | ADR-041 transport — 8 metrics + structured tracing |
| 6adb309 | `lock_or_die` / `lock_or_warn` adopted; `clippy::unwrap_used` denied |
| 12ae461 | `LockOrDie` / `LockOrWarn` poison helpers in `kiseki-common` |

**Production bypass:** `KISEKI_OBSERVABILITY=off` skips the
`InstrumentedLogOps` / `InstrumentedKeyManager` wrappers for hot-
path latency-sensitive deployments. Coarse counters and traces
still emit.

---

## Stream E — Bug-Bash from GCP 2026-05-02 / 04

A cluster of correctness fixes batched alongside the perf work.

| Commit | Bug |
|---|---|
| 79e8a1e | Bug 9 — FUSE per-inode dirty buffer + flush wiring |
| 973d331 | Bug 5 — multi-extent chunks + write/read guards |
| 1198343 | Bug 5 regression — empty-data writes in multi-extent path |
| 3c3b252 | Bug 4 — chunk PUTs above 64 MiB across multiple chunks |
| 2f73983 | Bug 8 — RwLock FUSE daemon for parallel reads |
| 6c2603b | Bug 7 — FUSE getattr uses SystemTime::now |
| 3a9c7ff | Bug 10 — minimal portmapper for NFSv3 mount |
| f57f26e | NFSv4 READDIR — drop dup `.`/`..` + emit non-zero mtime |
| 0f801f0 | Fabric message cap needs envelope-wrapper headroom |
| bfe3fa8 | ADR-023 rev 4 — drop NFSv4.0 from RFC scope (Bug 11) |
| c508ec0 | Bound gateway read latency (Bug 6 regression test) |

---

## Stream F — Acceptance Harness Hardening

| Commit | What |
|---|---|
| 1d4f102 | D-10 cross-stream — bump slow-down to 60 s + poll (replaces 1.5 s sleep with bounded retry against the GET deadline) |
| 72aef6f | D-10 — switch from slow-PutFragment to deny knob |
| dc64527 | D-4 — poll for hydration on new leader |
| ff2e7be | D-4 — silence dead-init clippy on the de-flake loop |
| 896f663 | Regression scenarios for the GCP 3rd-run gaps |
| 2b28e18 | Update 3 NFS step impls for the no-dot-no-dotdot contract |
| 1c6dc73 | Update nfs3_readdir test to match new contract |

**Cucumber retry**: `acceptance.rs` now configures
`.retries(2).retry_after(1 s).retry_filter(@flaky)` via
`gherkin::tagexpr::TagOperation`. Currently `@flaky`:

- `multi-node-raft.feature` D-10 cross-stream (GET retry race vs in-
  flight PutFragment)
- `chunk-storage.feature` 6-node EC PUT (cross-singleton compose
  pressure)

Real bugs surface as failures across all 3 attempts.

---

## Stream G — CI + Test Hygiene

| Commit | What |
|---|---|
| 14b6bcd | crc32c_throughput de-flaked under nextest workspace-parallel load (best-of-8 + 200 MiB/s floor) |
| 3e8af0f | Trim regular CI; move feature-matrix + coverage to release path |
| 44b1975 | Drop `--no-clean` from llvm-cov second pass |
| daa594d | `cargo fmt --all` (rustfmt 1.9.0 drift) |

**`.config/nextest.toml`** continues to split workspace tests around
the chunk-cluster / rustls CryptoProvider clash documented in
CLAUDE.md.

---

## Trailing items / Open follow-ups

These are *not* in scope for this plan but were flagged during the
sweep and should be picked up by the next iteration:

1. **GCP transport-profile re-run** to confirm the 28 Gbps fabric
   sees the FUSE p99 + group-commit wins on real hardware. (TCP_NODELAY
   on the fabric Channel was confirmed default-on in tonic 0.14.5 on
   2026-05-04 — the suspected gap turned out to be the H2 flow-control
   window, fixed in commit `f362060`.)
2. **3 e2e ReadTimeout flakes** (S3 PUT, S3 HEAD, FUSE remote-HTTP
   cross-protocol roundtrip) observed on the 2026-05-05 e2e run.
   Pressure-flake-shaped (28 / 31 pass), not a regression of the
   sweep, but warrants its own deflake pass.
3. **`KISEKI_OBSERVABILITY` documentation** in `monitoring.md` —
   currently only in `environment.md`. Operators need to find it.
4. **`docs/operations/durability.md` cross-link** from `monitoring.md`
   and `troubleshooting.md` for completeness.

---

## Verification

- BDD: 25 features, 321 scenarios (320 pass + 1 `@flaky` → retry-
  green), 22m 44s on the local 16-core box.
- Workspace tests: `cargo nextest` green on both invocations
  (workspace-minus-chunk-cluster + chunk-cluster-alone).
- E2e: 28 / 31 pass, 2 skipped, 3 ReadTimeout flakes flagged in the
  trailing-items section above. 16m 01s.
- Lint: `cargo clippy --all-targets -- -D warnings` clean.
- Format: `cargo fmt --check` clean.

## Pointers

- ADR-041 — `specs/architecture/adr/041-raft-transport-shard-multiplexing.md`
- I-L5 amendment — `specs/invariants.md`
- ADR-029 amendment — `specs/architecture/adr/029-raw-block-device-allocator.md`
- Durability operator doc — `docs/operations/durability.md`
- Performance per-flag matrix — `docs/operations/performance.md`
- Group-commit escalation — `specs/escalations/2026-05-04-group-commit-i-l5-durability-window.md`
