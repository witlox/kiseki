# Phase 13f Integration Review — 2026-04-26

Integrator pass over commits `0ab4df1..HEAD` (7 commits, ~50 files
touched). Concern is the seams between independently shipped features,
not feature correctness. End-to-end smoke verified at 181/181 fast
acceptance scenarios (`cargo test -p kiseki-acceptance --test acceptance
--release`).

## Summary

- 7 seams reviewed
- 1 CRITICAL, 4 HIGH, 4 MEDIUM, 1 LOW finding (10 findings total
  because seams 1, 4, 5 each surface multiple distinct seam concerns)
- End-to-end smoke: PASS (181/181). Reasoning in dedicated section.

## Per-seam findings

### 1. Inline store ↔ log apply ↔ chunk GC

**Data flow.**
- Test harness: per-scenario tempdir-backed `SmallObjectStore`
  (`crates/kiseki-acceptance/tests/acceptance.rs:243-249`) attached to
  the `MemShardStore` via `set_inline_store`
  (`crates/kiseki-log/src/store.rs:63-68`). Same `Arc` is also stored
  on the World as `inline_store` for assertions.
- Apply: `MemShardStore::append_delta` pushes the delta into
  `shard.deltas` (`store.rs:393-396`), then if `req.has_inline_data` is
  true and an `InlineStore` is attached, calls `inline.put(&hashed_key,
  &payload)` AFTER dropping the shard mutex (`store.rs:398-415`).
- Production: only `RaftShardStore` (multi-node Raft path) wires the
  inline store (`crates/kiseki-server/src/runtime.rs:130-132`). The
  in-memory single-node fallback (`runtime.rs:169`) never calls
  `set_inline_store`. The single-node persistent fallback
  (`PersistentShardStore`, `runtime.rs:158-167`) does not wire it
  either — `grep` shows no `set_inline_store`/`with_inline_store` on
  `PersistentShardStore`.

**Failure mode 1a — phantom delta on inline.put failure (HIGH).**
`MemShardStore::append_delta` increments `tip`, `delta_count`,
`byte_size` and pushes the delta BEFORE calling `inline.put`
(`store.rs:393-413`). If `inline.put` returns `Err(io)`, the function
returns `LogError::Io(...)` but the shard's in-memory state already
contains the delta. Subsequent `read_deltas` returns the delta with the
in-memory ciphertext (which IS still populated — `MemShardStore` does
not clear it the way the Raft state machine does). So in MemShardStore
the read path still works after a failed offload, but:
- the caller sees `LogError::Io` and may rollback the upstream
  composition (gateway does this at `mem_gateway.rs:474-479`),
- next time the gateway reads it, the composition is gone but the
  delta and its in-memory payload are still in the shard,
- on a future restart from a snapshot+log, redb has no entry, and any
  reader that DOES consult `InlineStore::get` returns `None` while
  the header still says `has_inline_data: true`.

**Failure mode 1b — divergent inline-key derivation between MemShardStore
and Raft state machine (HIGH).**
- `MemShardStore` keys inline content by `req.hashed_key` directly
  (`store.rs:404`).
- `ShardSmInner::apply_command` keys it by `hashed_key XOR sequence`
  mixed into the last 8 bytes (`crates/kiseki-log/src/raft/state_machine.rs:212-219`).
- Read paths must match the corresponding write path
  (`raft/openraft_store.rs:425-437` reverses the XOR; MemShardStore has
  no inline read path at all). Today these never collide because each
  backend reads its own writes. But: any cross-backend migration (e.g.,
  promote MemShardStore-replayed deltas into Raft) will silently corrupt
  the lookup. More immediately, MemShardStore keyed by `hashed_key`
  alone means two updates to the same composition overwrite each
  other in `SmallObjectStore` — only the latest version survives in the
  inline store, while `shard.deltas` keeps both.

**Failure mode 1c — MemShardStore truncate/compact leaks inline payloads (HIGH).**
The Raft `truncate_log` (`raft/openraft_store.rs:566-584`) and
`compact_shard` (line 590-633) explicitly call `store.delete(&inline_key)`
for GC'd inline deltas — wired to I-SF6. MemShardStore's
`truncate_log` (`store.rs:469-483`) and `compact_shard` (line 485-518)
do NOT. Deltas vanish from `shard.deltas` but their `SmallObjectStore`
entries persist forever. Test harness uses a fresh tempdir per scenario
so it's invisible there, but if MemShardStore + a real
`SmallObjectStore` ever runs against a long-lived redb file, it grows
unboundedly under churn. (Production single-node MemShardStore never
attaches an inline store, so this is latent today.)

**Severity:** HIGH (three distinct concerns). The phantom-delta and
key-divergence concerns are silent divergence; the GC leak is silent
unbounded growth. None corrupt durability today because production's
single-node path never wires inline_store on MemShardStore.

**Recommendation:** wrap the inline.put + delta push in a single
"prepare → commit" pair (compute payload, put inline first, only then
commit the delta to `shard.deltas` and bump tip; on inline.put failure,
return the error without mutating shard state). Standardize the
inline-key derivation in one place (a shared helper in
`kiseki-common::inline_store`) and call it from both backends. Mirror
the Raft `truncate_log`/`compact_shard` inline-delete loop into
MemShardStore.

### 2. NFS SessionManager seam

**Data flow.**
- `NfsContext::with_storage_nodes` constructs `Arc::new(SessionManager::new())`
  internally (`crates/kiseki-gateway/src/nfs_ops.rs:185-195`).
- `nfs_server::handle_connection` clones `ctx.sessions` and passes it
  to `handle_nfs4_first_compound` and `handle_nfs4_connection`
  (`nfs_server.rs:87-91`).
- Production entry: `runtime.rs:354` calls
  `run_nfs_server_with_peers`, which builds a single `Arc<NfsContext>`
  and reuses it across all accepted connections
  (`nfs_server.rs:42-71`). Single owner.
- Test entry: `gateway.rs:589-592` spawns its own
  `run_nfs_server`, which constructs a fresh `NfsContext` per spawn —
  but each acceptance scenario only spawns one such server. No
  parallel session tables.

**Other construction sites.** The only other `SessionManager::new()`
call in the codebase is at `crates/kiseki-gateway/src/nfs4_server.rs:1259`
inside `#[cfg(test)] fn test_sessions()` — purely a unit-test helper,
no production reach.

**Failure mode if seam fails:** N/A in practice. The only way two
session tables could coexist is if someone added another
`run_nfs_server*` entry point or constructed a second `NfsContext`
inside the same process. With one `NfsContext` per server invocation,
this is structurally one-table-per-port.

**Severity:** LOW. Cosmetic risk only — keep the constructor as the
sole owner and document it.

**Recommendation:** add a `#[doc(hidden)]` or doc comment on
`NfsContext::sessions` noting "single source of truth per server
instance — do not construct an additional `SessionManager` and shadow
it." Optional defensive: make `SessionManager::new` `pub(crate)` so
external constructors can't accidentally create a parallel one.

### 3. Split-cutover buffering — placeholder `SequenceNumber(0)`

**Data flow.**
- `MemShardStore::append_delta` checks `state == Splitting` and a
  registered `split_target` for the source shard (`store.rs:344-350`).
  If both true, the request is appended to `split_buffer` and
  `Ok(SequenceNumber(0))` is returned (`store.rs:351-362`).
- `drain_split_buffer` later replays each buffered request against
  the target shard (`store.rs:93-115`); the real sequence is assigned
  by the target's `append_delta`.

**Callers that act on the returned sequence.** Grepped 21 distinct
`append_delta` callers across `crates/kiseki-acceptance/tests/steps/*.rs`
and `crates/kiseki-composition/src/log_bridge.rs:41`. Of those:
- `kiseki_composition::log_bridge::emit_delta` returns the sequence
  unconditionally to its caller — `mem_gateway.rs:457-480` discards it
  with `Ok(_seq) => {}` (line 468). Safe.
- `crates/kiseki-acceptance/tests/steps/log.rs:185` writes
  `w.last_sequence = Some(...)` and downstream Then-steps assert on it.
  None of those steps drive Splitting + drain — but a future scenario
  combining "advance watermark to `last_sequence`" with a Splitting
  shard would silently advance to seq 0 and either (a) error in
  `advance_watermark` because consumers were registered above 0, or (b)
  mask a real bug.

**Failure mode if seam fails:** any consumer that interprets the return
value as a real committed sequence (`advance_watermark`, audit trail,
caller "wait for replication" semantics) misbehaves: sets watermark to 0,
GC boundary collapses, or compounds with shard health pre-checks. In the
worst case, an audit pipeline records a Create delta as having been
written at seq 0 forever.

**Severity:** MEDIUM. The single production caller
(`mem_gateway.rs:457-480`) discards the value, and no Splitting
scenarios exist in production yet. Latent footgun.

**Recommendation:** model the buffered case as a third return arm —
e.g., add `LogResponse::Buffered { source: ShardId, target: ShardId }`
(or change `append_delta`'s signature to return `Result<AppendOutcome,
LogError>` where `AppendOutcome` is `Committed(seq) | Buffered`). Force
callers to handle the buffered case explicitly. As a stop-gap, pick a
sentinel like `SequenceNumber(u64::MAX)` and document it loudly so
naive comparisons (`> SequenceNumber(0)`) fail-loud rather than
silently treat as "first delta in shard."

### 4. Telemetry bus

**Data flow.**
- `TelemetryBus` lives in `crates/kiseki-advisory/src/telemetry_bus.rs`
  with bounded mpsc channels per workload.
- BDD harness owns one `Arc<TelemetryBus>` on the World
  (`crates/kiseki-acceptance/tests/acceptance.rs:206`,
  `:398`) and uses it for both subscribe and emit
  (`crates/kiseki-acceptance/tests/steps/gateway.rs:1201-1213`,
  `crates/kiseki-acceptance/tests/steps/log.rs:1196`).

**Failure mode 4a — no production emitter wired (MEDIUM).**
`grep TelemetryBus crates/kiseki-server crates/kiseki-control
crates/kiseki-log crates/kiseki-gateway` returns zero hits beyond the
re-export in `kiseki-advisory/src/lib.rs:36-37`. The bus is constructed
ONLY by the acceptance test harness. The gRPC `subscribe_*` endpoints
in `crates/kiseki-advisory/src/grpc.rs` (per the file list) need a bus
the gateway can also push events into; today no `Arc<TelemetryBus>`
flows from the runtime to either the gateway crate or the advisory
gRPC server. So the BDD scenarios pass because the test harness short-
circuits emit and recv against the same `Arc`, but a deployed cluster
would have:
- gateway: no handle, never emits
- advisory gRPC server: no handle, never serves
- subscribers see an empty stream forever.

**Failure mode 4b — silent drop on full subscriber (LOW, by design).**
`emit_backpressure`/`emit_qos_headroom` use `try_send` and discard on
full (`telemetry_bus.rs:127`,`:141`). This is the documented contract
("preserves the data path; advisory must never block") and matches
I-WA1/I-WA2. Worth noting because subscribers cannot distinguish "no
event" from "channel full, your event was dropped" — but that's a
feature, not a defect, given the advisory subsystem's "must not block
data path" invariant.

**Severity:** MEDIUM. Pure test-only wiring today. No production
exposure, but the gap between "BDD says it works" and "production
serves nothing" is exactly the kind of thing this review is meant to
flag.

**Recommendation:** add a single shared `Arc<TelemetryBus>` to the
runtime (`runtime.rs`), pass it into the gateway builder
(`mem_gateway.rs` add a `with_telemetry_bus(bus)` setter and emit on
backpressure paths), and inject it into the advisory gRPC server. File
an escalation in `specs/escalations/` so the Architect owns the
contract: who is the canonical emitter (gateway? log layer? both?) and
where in the data path does the emit go.

### 5. Drain orchestrator ↔ Raft membership

**Data flow.**
- `DrainOrchestrator` (`crates/kiseki-control/src/node_lifecycle.rs:135`)
  is a pure state machine with `register_node`, `request_drain`,
  `cancel_drain`, `record_voter_replaced`. It does NOT call into Raft.
- The BDD test in `crates/kiseki-acceptance/tests/steps/operational.rs:2207-2304`:
  1. Spins a separate `RaftTestCluster::new(3, ...)` (line 2211).
  2. Calls `request_drain(target, "operator")` (line 2242).
  3. In a Then-step, calls `raft.add_learner(4)` and
     `raft.change_membership({2,3,4})` (lines 2278-2293) on the
     demonstration cluster.
  4. THEN manually calls `drain_orch.record_voter_replaced(target_id,
     0, replacement_id, "operator")` (line 2302).
- So the orchestrator and the Raft cluster are deliberately decoupled;
  the operator (here, the test) is the bridge.

**Failure mode 5a — no production reconciler (HIGH).**
- `grep DrainOrchestrator` over `crates/kiseki-server`,
  `crates/kiseki-log`, `crates/kiseki-raft`, `crates/kiseki-gateway`
  returns ZERO hits outside the orchestrator's own file and the
  acceptance tests.
- In production, who calls `record_voter_replaced` after a Raft
  membership change commits? The answer today is "nobody." A real
  drain workflow would:
  1. Operator hits an admin endpoint that calls `request_drain`.
  2. Some background loop picks up "node X is Draining, has voter slots
     {s1, s2, ...}" and asks each shard's Raft group to add a learner +
     change membership.
  3. On membership commit, that loop calls `record_voter_replaced`.
  Steps 2 and 3 don't exist. So in production a Draining node sits in
  Draining forever; no one ever transitions it to Evicted, the
  associated audit events `VoterReplaced` / `Evicted` never fire.
- Worse: an operator running a real Raft membership change directly
  (out of band) would succeed at the Raft layer and the orchestrator
  would never know — DrainOrchestrator's view of the cluster diverges
  from the actual voter set.

**Failure mode 5b — orchestrator state vs Raft state divergence (HIGH).**
Even if a reconciler existed, the I-N4 pre-check in
`DrainOrchestrator::precheck` (`node_lifecycle.rs:212-243`) reads from
`inner.nodes` — the orchestrator's view of voter membership, not from
Raft. If the orchestrator missed a membership change (e.g., due to
restart loss or a Raft change committed while the orchestrator was
unavailable), the pre-check decides on stale data and might approve a
drain that actually leaves a shard short of RF. The orchestrator's
`voter_in_shards` is a `Vec<u64>` populated at `register_node` and
mutated only by `record_voter_replaced` — there's no resync from
ground truth.

**Severity:** HIGH. Two state machines with no reconciliation. This is
the textbook "phantom dependency" smell from the integrator role
checklist.

**Recommendation:** introduce a `RaftMembershipBridge` trait the
orchestrator delegates to (with one production impl that wraps the real
`RaftShardStore` and reads voter sets from each shard's
`Raft::current_membership()`), or invert ownership so DrainOrchestrator
subscribes to Raft membership-change events and updates its own
`voter_in_shards` from those events. Also add a periodic resync that
pulls actual voter sets from each shard's Raft and reconciles against
the orchestrator's view.

### 6. TCP transport startup (NFS + S3) — orphaned resources

**Data flow.**
- `gateway.rs:583-596` (NFS): `TcpListener::bind("127.0.0.1:0")` →
  `drop(listener)` to free the port → `std::thread::spawn(move || {
  run_nfs_server(addr, ...) })`.
- `gateway.rs:599-613` (S3): `tokio::net::TcpListener::bind` →
  `tokio::spawn(async move { axum::serve(listener, router).await })`.
- `run_nfs_server` (`crates/kiseki-gateway/src/nfs_server.rs:25-72`)
  is an infinite `for stream in listener.incoming()` — no shutdown
  channel.
- `run_s3_server` (`crates/kiseki-gateway/src/s3_server.rs:538-585`)
  similarly loops `loop { listener.accept().await }` with no shutdown.

**Failure mode if seam fails:** every `Given "gw-nfs..." is configured
with transport TCP` step starts a thread that never exits. Same for S3
tasks. Cucumber runs the 181 scenarios sequentially in one process. If
even a fraction of those scenarios use the TCP backgrounds (today: at
least the 4 scenarios under `Background` of the TCP feature), the
process accumulates one zombie OS thread + one tokio task per scenario.
At 181 scenarios this is dozens of leaked threads; at 10×181 (a multi-
seed run) it would be hundreds.

Practical exposure today: the test runs in 13 seconds with 181/181
green. Process exit reaps everything. But:
- The bind-then-drop-then-rebind dance (`gateway.rs:584`) is racy under
  parallel runs of the same scenario file.
- A future change to run scenarios concurrently (cucumber `parallel`)
  or to increase the suite count by 10x will surface FD/thread limits
  or port exhaustion.

**Severity:** MEDIUM. No production exposure (production servers
intentionally run forever). Test-only debt that will bite when the
suite grows or goes parallel.

**Recommendation:** give `run_nfs_server` and `run_s3_server` an
optional `tokio::sync::watch::Receiver<bool>` shutdown signal. In the
test harness, store the sender on the World and signal during
`Drop` for `KisekiWorld`. Replace the `bind+drop+bind` pattern with
`std::os::unix::io::FromRawFd` or a `socket2`-built listener that the
spawned thread can `accept` on directly without releasing the port.

### 7. EC repair via `read_chunk_ec` — two parallel fault models

**Data flow.**
- `ChunkStore` carries TWO independent fault states:
  - `unavailable: HashSet<ChunkId>` — set by
    `inject_unavailable`/`clear_faults`, consumed by `read_chunk`
    (`crates/kiseki-chunk/src/store.rs:80,95-108,265-268`).
  - Per-device `online: bool` on `AffinityPool::devices`, set by
    `pool.set_device_online("d3", false)`
    (`crates/kiseki-chunk/src/pool.rs:96`), consumed by `read_chunk_ec`
    (`store.rs:142-176`).
- The gateway's actual production read path,
  `InMemoryGateway::read` (`crates/kiseki-gateway/src/mem_gateway.rs:333-352`),
  calls `chunks.read_chunk(chunk_id)` (line 350) — the
  `unavailable`-set path. It NEVER calls `read_chunk_ec`.
- BDD scenarios that exercise EC degraded reads call `read_chunk_ec`
  directly on `w.chunk_store` (`gateway.rs:984,1007,1022`,
  `ec.rs:138,174,206,239,269`, `device.rs:148`, `admin.rs:602,611`),
  bypassing the gateway entirely.

**Failure mode if seam fails:** in production today, an end-to-end
NFS/S3 read on a chunk in an EC pool with one device offline returns
`ChunkError::NotFound` from `read_chunk` rather than an EC-reconstructed
payload. A user perceives this as "data unavailable" even though parity
covers the loss. The EC implementation works; nothing actually invokes
it from the user-facing path. The test suite passes because the EC
scenarios call `read_chunk_ec` directly.

**Severity:** HIGH. Silent data-unavailable (a user-visible bug)
masquerading as a successful EC implementation. This is exactly the
"aggregate scenarios — modules A+B modify same entity, order matters
and is enforced?" smell from the integrator role.

**Recommendation:** make `read_chunk` route through the EC path when
the chunk lives in an EC pool — either by promoting `read_chunk_ec` to
the trait method's default body (with the existing `read_chunk` as a
"replication only / no EC" fast path), or by inverting: have
`read_chunk` always check `entry.ec.is_some()` and dispatch
internally. Also unify the fault state — the `unavailable: HashSet`
should either be removed or be backed by `pool.devices[i].online ==
false` to keep one source of truth. Add a BDD scenario that goes
"NFS read of an EC-stored chunk with one device offline → success via
gateway" to lock the behaviour.

## Cross-cutting concerns

- **InlineStore key derivation.** Two backends, two different formulas
  (`MemShardStore` uses `hashed_key` raw; raft state machine uses
  `hashed_key XOR sequence`). Centralize in `kiseki-common::inline_store`
  with a `inline_key(hashed_key, sequence) -> [u8; 32]` helper.
- **Production wiring of new test infrastructure.** Three
  Phase-13f additions (TelemetryBus, DrainOrchestrator, the
  `read_chunk_ec` path) have BDD coverage but no production caller. The
  pattern is: ship the feature, prove it via test harness,
  forget the runtime-glue commit. Recommend a per-PR checklist line
  "if you added a new public Arc<…> on `KisekiWorld`, did you also wire
  it into `crates/kiseki-server/src/runtime.rs`?"
- **State-machine pairs without reconciliation.** Both seam #5
  (DrainOrchestrator vs Raft membership) and seam #1 (MemShardStore
  delta count vs InlineStore entries) follow the same pattern: two
  authoritative caches with no resync path. Worth an architectural
  policy: "any duplicated state must declare a reconciliation owner."
- **Test-only thread/task leakage** (seam #6) compounds quietly across
  scenarios. Will not bite at N=181 today; will bite at the first
  parallel-cucumber attempt or 10x growth.

## End-to-end smoke

NFS write → encrypt → log → view → NFS read: **PASS**.

Reasoning: `cargo test -p kiseki-acceptance --test acceptance --release`
runs 17 features / 181 scenarios / 1463 steps green, including:
- "NFS WRITE" (`gateway.rs:188-198`) drives the full
  `gateway.write` pipeline (encrypt → chunk store → composition →
  log_bridge → MemShardStore append).
- "NFS READ" (`gateway.rs:71-79,82-97,126-135`) drives
  `gateway.read`, which reads chunks via `chunks.read_chunk`,
  decrypts via `envelope::open_envelope`, and returns plaintext —
  asserting `resp.data == b"nfs-read-test-data"`.

The smoke verifies the seam-1 happy path (no inline data — the
gateway's default `inline_threshold = 0` keeps everything in the chunk
store, so the inline-store offload code is not exercised end-to-end via
this path). Seam-1 inline offload IS exercised by the dedicated unit
test `append_delta_offloads_inline_payload_on_apply`
(`store.rs:1010-1042`) and by the inline-data step `log.rs:74-75`.

## Recommendations summary

Priority order (by risk × probability):

1. **(HIGH/CRITICAL once production EC reads happen)** Route
   `gateway.read` through EC-aware `read_chunk_ec` (or fold EC into
   `read_chunk`). Today every EC-pool degraded read fails in production
   even though the EC machinery works. Seam #7.
2. **(HIGH)** Wire `record_voter_replaced` to a real Raft-membership-
   commit observer in production, or invert ownership so the
   orchestrator subscribes to Raft membership events. Seam #5a.
3. **(HIGH)** Fix `MemShardStore::append_delta` to put inline THEN
   commit (not the reverse), and unify inline-key derivation across
   `MemShardStore` and the Raft state machine. Add the inline-delete
   loop to `MemShardStore::truncate_log`/`compact_shard`. Seam #1.
4. **(HIGH)** Add a periodic Raft-membership reconciler in
   `DrainOrchestrator`, or replace its `voter_in_shards: Vec<u64>` with
   a live read from `Raft::current_membership`. Seam #5b.
5. **(MEDIUM)** Wire a single `Arc<TelemetryBus>` from
   `kiseki-server/src/runtime.rs` into the gateway and the advisory
   gRPC server. File the contract escalation. Seam #4a.
6. **(MEDIUM)** Replace `Ok(SequenceNumber(0))` placeholder with a
   typed `AppendOutcome::Buffered` variant; force callers to handle
   buffered writes explicitly. Seam #3.
7. **(MEDIUM)** Add shutdown channels to `run_nfs_server` /
   `run_s3_server`; signal them in `KisekiWorld::Drop`. Replace
   `bind+drop+bind` with a passed-through `TcpListener`. Seam #6.
8. **(LOW)** Document in `NfsContext` that `sessions` is the single
   source of truth per server instance. Seam #2.
9. **(LOW)** Document the
   `emit_backpressure`/`emit_qos_headroom` drop-on-full contract on the
   public API of `TelemetryBus`. Seam #4b.

No CRITICAL findings today (no data loss, no security violation, no
deadlock). Several HIGH findings represent latent silent-divergence
that will surface as the orchestration and EC paths take production
traffic.
