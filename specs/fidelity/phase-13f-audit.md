# Phase 13f Audit — 2026-04-26

Scope: `git diff 0ab4df1..HEAD` (7 commits, 14 newly-green @integration BDD
scenarios, 11 new unit tests, 2 new production modules).

## Summary

- **30 step definitions** classified across 7 affected scenarios
  (gateway: NFS open/lock, NFS-over-TCP, S3-over-TCP, KMS unreachable,
  Chunk Storage partial, backpressure telemetry, access-pattern
  readahead; log: inline-data, split-buffer cutover; operational:
  graceful retire, drain refused/re-issued, drain cancellation).
- **2 new production modules**: `telemetry_bus.rs` (kiseki-advisory),
  `node_lifecycle.rs` (kiseki-control). Both compile clean and ship
  unit tests; coverage of spec'd invariants is partial — see §
  Production-module coverage.
- **4 ADRs touched**: 021 advisory, 030 inline, 034 merge/split,
  035 drain. Per-ADR enforcement summarised below.
- **14 newly-green scenarios is real progress** from the previous
  167/181, with the genuinely-integrated paths being inline offload
  (ADR-030), split-buffer cutover (ADR-034), and drain orchestration
  state machine (ADR-035).

### Top-3 most concerning gaps

1. **HIGH — `then_lock_local` constructs a fresh `LockManager` whose
   count is asserted = 0** (gateway.rs:511-525). That assertion holds
   for `LockManager::default()` regardless of any system-under-test
   behaviour — proves an axiom about the constructor, not "lock state
   is gateway-local". A real check would spawn a second `NfsContext`
   sharing nothing with the first, replay the OPEN+LOCK against it,
   and assert that the second context's table sees zero locks while
   the original still sees one. As written, the test is structurally
   indistinguishable from `assert!(true)`.

2. **HIGH — drain "progress per shard" runs Raft membership change on
   a disconnected demonstration cluster** (operational.rs:2295-2329).
   `w.drain_raft.add_learner(4)` and `change_membership({2,3,4})` run
   on a cluster created in `ensure_drain_raft` that has no link to
   `w.drain_orch`'s `voter_in_shards: vec![100+7]` accounting. The
   audit trail (`record_voter_replaced`) is added independently. A
   real wiring would have `DrainOrchestrator` invoke the Raft handles
   it owns and update its progress as a side effect. As written, the
   Raft calls are a tracer-bullet test of `RaftTestCluster::add_learner`,
   not the orchestrator.

3. **HIGH — `then_caller_queue_only` and `then_no_neighbour_headroom`
   assert against fresh `BudgetEnforcer` instances** (gateway.rs:1241-1254
   and 1455-1468). `BudgetEnforcer::new(...).hints_used() == 0` is a
   property of the constructor, not of the running gateway. To verify
   I-WA5 ("neighbour callers do not leak through this channel") the
   test must show that emitting backpressure for "alice" leaves
   "bob"'s subscription empty — that *is* what
   `telemetry_bus.rs::backpressure_subscriber_receives_only_own_events`
   tests, but the @integration step does not exercise that path.

### Per-ADR enforcement table

| ADR | Decision | Status | Test that fails on violation |
|---|---|---|---|
| 021 | Per-caller telemetry (I-WA5) — bucketed retry hint, no neighbour leak | **ENFORCED (in unit), DOCUMENTED (in BDD)** | `telemetry_bus.rs:151,173` (unit) — would fail if Bob received Alice's event. BDD `then_backpressure_event` (gateway.rs:1219) verifies bucketing but `then_caller_queue_only` is non-falsifiable (see §1). |
| 030 | Inline payloads ≤ threshold offload to `objects.redb` on apply (I-SF5) | **ENFORCED** | `store.rs:1024 append_delta_offloads_inline_payload_on_apply` (unit) and `then_payload_offloaded` (log.rs:67-79, BDD) — both fail if `inline.put` is not called with the right key+payload. |
| 034 | Split-cutover writes to a Splitting source's out-of-range key are buffered, then drained to the target | **ENFORCED** | `store.rs:961 split_buffer_holds_out_of_range_writes_then_drains_to_target` (unit) and `then_buffered`/`then_committed_to`/`then_no_delta_lost` (log.rs:984-1037, BDD). Would fail if the buffered write didn't land on the target. |
| 034 | Merge in-progress shards reject Split (F-O6 ordering rule) | **ENFORCED** | `then_split_rejected` (log.rs:1390-1399) checks the error string for the rejection reason. |
| 035 | I-N4 capacity pre-check refuses drain that would break RF=3 | **ENFORCED** | `node_lifecycle.rs:402 drain_refused_when_only_three_active_nodes` (unit) + `then_request_refused_with` (operational.rs:2370). |
| 035 | I-N7 cancellation: Draining → Active is reversible, no rollback of completed replacements | **ENFORCED (state), DOCUMENTED (no-rollback)** | `node_lifecycle.rs:439 cancel_drain_returns_node_to_active` (unit) + `then_transitions_active`/`then_cancellation_audited` (operational.rs:2456-2476). The "no rollback" half is doc-only; no test fails if a future change added rollback. |
| 035 | I-N6 audit trail (DrainRequested / DrainRefused / DrainCancelled / VoterReplaced / Evicted) | **ENFORCED** | All five audit events tested via the `audit().iter().any(matches!)` pattern in operational.rs and node_lifecycle.rs unit tests. |
| 035 | Drain orchestration drives real Raft membership changes | **DOCUMENTED only** | The orchestrator never calls `RaftTestCluster::change_membership`. The BDD step does, but on an unrelated cluster (see §2). No test fails if the orchestrator never touches Raft. |

## Per-feature depth table

Severity legend: **HIGH** (claims integration, asserts an isolated
fixture or constructor axiom), **MEDIUM** (assertion is weak / partial
but not false), **LOW** (cosmetic / acceptable simplification).

### protocol-gateway.feature

| Scenario | Step | File:line | Depth | Gap | Severity |
|---|---|---|---|---|---|
| NFSv4.1 state management | `given_nfs_open` | gateway.rs:451-456 | THOROUGH | Calls `sessions.open_file(fh)` on the real `SessionManager` — fh derived from path, matches lock step. | LOW |
| NFSv4.1 state management | `given_nfs_lock` | gateway.rs:458-472 | THOROUGH | Real `LockManager::lock` call. | LOW |
| NFSv4.1 state management | `when_another_lock` | gateway.rs:474-491 | THOROUGH | Conflicting lock, real `Denied(_,_)` path, mapped to `NFS4ERR_DENIED`. | LOW |
| NFSv4.1 state management | `then_lock_denied` | gateway.rs:493-500 | THOROUGH | Asserts the mapped error string. | LOW |
| NFSv4.1 state management | `then_lock_state_maintained` | gateway.rs:502-509 | THOROUGH | `nfs_ctx.locks.lock_count() >= 1` — real query against real LockManager. | LOW |
| NFSv4.1 state management | `then_lock_local` | gateway.rs:511-525 | **SHALLOW** | Constructs a fresh `LockManager::default()` whose count is necessarily 0 by construction. Does not exercise multi-gateway isolation. | **HIGH** |
| NFS gateway over TCP | `given_transport_tcp` | gateway.rs:576-615 | THOROUGH | Spawns a real `nfs_server::run_nfs_server` thread on an ephemeral port; binds NfsGateway over the real World gateway. | LOW |
| NFS gateway over TCP | `when_client_connects` | gateway.rs:617-632 | THOROUGH | Real `TcpStream::connect_timeout` to the bound port. | LOW |
| NFS gateway over TCP | `then_nfs_tcp_tls` | gateway.rs:634-649 | **SHALLOW** | Asserts the listener has a non-zero port. The "TLS" half of the requirement is explicitly noted as out of scope ("TLS termination handled upstream"). | **MEDIUM** (TLS is a real ADR-022 requirement; in-process TCP-only test does not falsify a missing TLS path) |
| NFS gateway over TCP | `then_nfs_rpc_framing` | gateway.rs:651-668 | THOROUGH | Sends a real ONC-RPC record-marker prefix (`0x80000000`) over the TCP stream — server is exercised. | LOW |
| S3 gateway over TCP | `then_s3_https` | gateway.rs:670-683 | **SHALLOW** | Same TLS gap as NFS — TCP connect succeeds, "HTTPS" is asserted only by comment. | **MEDIUM** |
| S3 gateway over TCP | `then_s3_rest_semantics` | gateway.rs:685-697 | THOROUGH | Calls `gateway_write` through real pipeline. | LOW |
| Gateway cannot reach KMS | `given_tenant_kms_unreachable_gw` | gateway.rs:793-808 | THOROUGH | Real `MemKeyStore::inject_unavailable()` after pre-write. | LOW |
| Gateway cannot reach KMS | `given_cached_kek_expired` | gateway.rs:810-815 | STUB | No-op — relies on World starting fresh. Acceptable per ADR-030's cache-empty default but documented as "no explicit mutation". | LOW |
| Gateway cannot reach KMS | `when_write_arrives` | gateway.rs:817-830 | THOROUGH | Real `fetch_master_key` call returns `KeyManagerError::Unavailable` after injection. | LOW |
| Gateway cannot reach KMS | `then_cannot_encrypt` | gateway.rs:832-839 | THOROUGH | Asserts the captured error type. | LOW |
| Gateway cannot reach KMS | `then_write_rejected_retriable` | gateway.rs:841-855 | THOROUGH | Verifies the `KeyManagerError → KisekiError::Retriable` mapping with a real conversion. | LOW |
| Gateway cannot reach KMS | `then_cached_reads_work` | gateway.rs:858-875 | THOROUGH | Re-reads the pre-outage composition through real `gateway_read`. | LOW |
| Gateway cannot reach KMS | `then_tenant_admin_alerted` | gateway.rs:877-947 | MOCK | Fabricates an `AuditEvent` in-step and asserts it appears in the audit log. The system-under-test (gateway/keystore) does not produce the event itself; the step does. | **MEDIUM** (test would still pass if the gateway never alerted — only asserts that `audit_log.append` then `audit_log.query` are consistent) |
| Chunk Storage partial | `given_chunk_storage_partial` | gateway.rs:948-980 | THOROUGH | Builds a real EC 4+2 pool with 6 devices, writes EC-encoded chunks, takes one device offline. | LOW |
| Chunk Storage partial | `when_read_unavailable_device` | gateway.rs:981-989 | THOROUGH | Calls real `read_chunk_ec` which traverses the EC decode path with 1 device offline. | LOW |
| Chunk Storage partial | `then_ec_repair_attempted` | gateway.rs:991-1001 | THOROUGH | Asserts the 4+2 read with d3 offline succeeded — that is the EC repair path. | LOW |
| Chunk Storage partial | `then_repair_completes` | gateway.rs:1003-1011 | THOROUGH | Verifies reconstructed payload size. | LOW |
| Chunk Storage partial | `then_repair_fails_error` | gateway.rs:1013-1029 | THOROUGH | Takes 3 devices offline (exceeds parity=2), asserts error. | LOW |
| Chunk Storage partial | `then_protocol_error` | gateway.rs:1031-1047 | THOROUGH | `ChunkError::ChunkLost` stringifies as "chunk lost: insufficient fragments for reconstruction" — matches both `"insufficient"` and `"reconstruction"` substrings. **Verified — assertion is real and falsifiable.** | LOW |
| Backpressure telemetry | `given_backpressure_sub` | gateway.rs:1199-1203 | THOROUGH | Real `subscribe_backpressure` on `TelemetryBus`. | LOW |
| Backpressure telemetry | `when_queue_crosses_threshold` | gateway.rs:1205-1214 | MOCK | The "queue depth crossing" is fabricated — the step constructs the event and emits it directly. The gateway code path that *would* emit on real saturation is not exercised. | **MEDIUM** (asserts the bus delivery, not the trigger) |
| Backpressure telemetry | `then_backpressure_event` | gateway.rs:1216-1238 | THOROUGH | Real `try_recv` on the per-workload channel + bucket whitelist check. Falsifiable. | LOW |
| Backpressure telemetry | `then_caller_queue_only` | gateway.rs:1240-1254 | **SHALLOW** | `BudgetEnforcer::new(...).hints_used() == 0` — asserts a constructor invariant, not isolation. | **HIGH** |
| Backpressure telemetry | `then_data_path_accepts` | gateway.rs:1256-1265 | THOROUGH | Real write through the gateway pipeline. | LOW |
| Access-pattern hint | `then_may_readahead` | gateway.rs:1294-1320 | MOCK | Drives `PrefetchAdvisor` directly with synthetic block reads — not the integration path that goes "NFS io_advise → advisory hint → view materialization". The advisor is exercised faithfully but in isolation from the gateway. | **MEDIUM** |
| QoS-headroom (gateway side) | `when_gw_computes_headroom` | gateway.rs:1437-1443 | SHALLOW | Reads `hints_used()` and resets `last_error`. No actual headroom computation. | MEDIUM |
| QoS-headroom (gateway side) | `then_bucketed_fraction` | gateway.rs:1445-1452 | SHALLOW | `assert!(used < 100)` where `used == 0` (fresh enforcer). Tautological. | **MEDIUM** |
| QoS-headroom (gateway side) | `then_no_neighbour_headroom` | gateway.rs:1454-1468 | **SHALLOW** | Same construction-axiom problem as `then_caller_queue_only`. | **HIGH** |

### log.feature

| Scenario | Step | File:line | Depth | Gap | Severity |
|---|---|---|---|---|---|
| Inline data delta | `given_inline_threshold` | log.rs:46-53 | THOROUGH | Real `set_shard_config` with the spec'd byte count. | LOW |
| Inline data delta | `when_append_table` | log.rs:107-152 | THOROUGH | Real `append_delta` with `has_inline_data: true`; `last_inline_key` and `last_delta` captured for downstream Then. | LOW |
| Inline data delta | `then_inline_committed` | log.rs:55-65 | THOROUGH | Asserts both `header.has_inline_data` and `payload.ciphertext` non-empty. | LOW |
| Inline data delta | `then_payload_offloaded` | log.rs:67-79 | THOROUGH | Trait-disambiguated call into `<SmallObjectStore as InlineStore>::get(&w.inline_store, &key)` — same key the apply path used (wired via `mem_shard_store.set_inline_store(inline_store)` in `World::new`, acceptance.rs:247-249). **Verified — key matches.** | LOW |
| Inline data delta | `then_no_chunk_write` | log.rs:81-89 | THOROUGH | Asserts `delta.header.chunk_refs.is_empty()`. | LOW |
| Delta append to splitting shard | `given_mid_split` | log.rs:937-948 | THOROUGH | Calls real `set_split_target` on the concrete `MemShardStore` after creating both shards. | LOW |
| Delta append to splitting shard | `given_split_boundary` | log.rs:951-968 | THOROUGH | Real range update + state transition to Splitting. | LOW |
| Delta append to splitting shard | `when_append_at_key` | log.rs:971-981 | THOROUGH | Real `append_delta` with hashed_key 0x90 (out of source range 0x00..0x80). | LOW |
| Delta append to splitting shard | `then_buffered` | log.rs:984-995 | THOROUGH | Asserts `split_buffer_len(source) == 1` — real query on the live store. | LOW |
| Delta append to splitting shard | `then_latency_bump` | log.rs:998-1003 | STUB | No-op with comment. The "latency bump" is asserted only by reference to the previous step. Acceptable: behavioural implication. | LOW |
| Delta append to splitting shard | `then_committed_to` | log.rs:1006-1023 | THOROUGH | Real `drain_split_buffer(source)` then asserts `target_health.delta_count == 1`. | LOW |
| Delta append to splitting shard | `then_no_delta_lost` | log.rs:1025-1037 | THOROUGH | Real assertion: source buffer empty, target has exactly one delta. | LOW |
| Merge in progress | `given_merge_in_progress` | log.rs:1259-1286 | THOROUGH | Real state transitions; pre-registers the merged shard via `derive_merged_name` (log.rs:1288-1300) — common-prefix algorithm. **Verified for "shard-c1"+"shard-c2" → "shard-c12".** | LOW |
| QoS-headroom telemetry (Given side) | `given_qos_sub` | log.rs:1194-1198 | THOROUGH | Real `subscribe_qos_headroom` on `TelemetryBus`. | LOW |

### operational.feature

| Scenario | Step | File:line | Depth | Gap | Severity |
|---|---|---|---|---|---|
| Graceful retire | `given_admin_needs_retire` | operational.rs:2222-2226 | THOROUGH | Spins up a real 3-node `RaftTestCluster` for the demo path. | LOW |
| Graceful retire | `given_five_active_including` | operational.rs:2229-2237 | THOROUGH | Registers all 6 nodes (n1..n5 + n7) in the orchestrator's registry. n7 gets `voter_in_shards: vec![107]` — a single voter slot. | LOW |
| Graceful retire | `when_operator_drains` | operational.rs:2240-2246 | THOROUGH | Real `request_drain` call; error captured. | LOW |
| Graceful retire | `then_capacity_validated` | operational.rs:2249-2260 | MOCK | Asserts the captured error is `None` — proves I-N4 *did not refuse*, not that I-N4 *ran*. (Equivalent for this scenario, but the assertion would still pass if the pre-check were a no-op.) | **MEDIUM** |
| Graceful retire | `then_transitions_draining` | operational.rs:2263-2271 | THOROUGH | Real `state(id) == Draining` query. | LOW |
| Graceful retire | `then_progress_per_shard` | operational.rs:2274-2305 | **SHALLOW** | Runs `add_learner(4)` and `change_membership({2,3,4})` on `w.drain_raft` — a separate cluster with no relationship to `w.drain_orch`'s registry. The `record_voter_replaced` call on the orchestrator is independent of the Raft work. | **HIGH** (claims "real Raft membership change drives drain progress" but the two are not wired) |
| Graceful retire | `then_transitions_evicted` | operational.rs:2308-2316 | THOROUGH | Real `state(id) == Evicted` after `record_voter_replaced` completes the only voter slot. | LOW |
| Graceful retire | `then_operator_signalled` | operational.rs:2319-2336 | THOROUGH | Real audit-trail scan for `Evicted` + `VoterReplaced` events. | LOW |
| Graceful retire | `then_state_transitions_audited` | operational.rs:2339-2353 | THOROUGH | Real audit-trail scan for `DrainRequested` + `Evicted`. | LOW |
| Drain refused / re-issued | `given_exactly_three_active` | operational.rs:2358-2363 | THOROUGH | Registers exactly 3 nodes — `precheck` will refuse. | LOW |
| Drain refused / re-issued | `then_request_refused_with` | operational.rs:2366-2375 | THOROUGH | Asserts captured error contains the spec'd phrase. Falsifiable. | LOW |
| Drain refused / re-issued | `then_replacement_n4` | operational.rs:2379-2382 | THOROUGH | Real `register_node(n4, [])`. | LOW |
| Drain refused / re-issued | `then_operator_reruns` | operational.rs:2385-2391 | THOROUGH | Real second `request_drain`. | LOW |
| Drain refused / re-issued | `then_drain_accepted` | operational.rs:2407-2415 | THOROUGH | Asserts `state == Draining` and no error. | LOW |
| Drain refused / re-issued | `then_audit_refusal_and_success` | operational.rs:2418-2429 | THOROUGH | Real audit scan for both `DrainRefused` and `DrainRequested`. | LOW |
| Drain cancellation | `given_node_draining` | operational.rs:2434-2442 | MOCK | Uses `set_state(id, Draining)` to fast-forward — bypasses `request_drain` (and therefore the `DrainRequested` audit event). Acceptable for setting up a precondition; documented as "set state directly". | LOW |
| Drain cancellation | `when_operator_drain_cancel` | operational.rs:2396-2402 | THOROUGH | Real `cancel_drain` call. | LOW |
| Drain cancellation | `then_transitions_active` | operational.rs:2445-2453 | THOROUGH | Real state query. | LOW |
| Drain cancellation | `then_cancellation_audited` | operational.rs:2456-2464 | THOROUGH | Real audit scan for `DrainCancelled`. | LOW |
| Drain cancellation | `then_subsequent_ops_succeed` | operational.rs:2467-2476 | MOCK | Asserts `snapshot()` shows the node `Active` with no `drain_progress`. Does *not* re-issue any operation against the node — "subsequent operations succeed" is interpreted as "registry shows healthy". | MEDIUM |

## Production-module test coverage

### `crates/kiseki-advisory/src/telemetry_bus.rs`

| Spec invariant claimed | Tests covering | Tests missing |
|---|---|---|
| **I-WA5 per-caller scoping** (subscriber only sees own events) | `backpressure_subscriber_receives_only_own_events` (line 151-170), `qos_headroom_per_workload_isolation` (line 173-183) | None — the unit tests cover both happy path and the cross-workload isolation negative case. |
| Bucketed retry-after (only fixed values exposed) | `retry_after_buckets_to_fixed_set` (line 186-192) | None — tests boundaries (0, 50, 75, 150, 10_000). |
| Bounded subscriber queue / drop-on-overflow | None | **MISSING**: no test fills the 64-slot channel and asserts that the oldest event is dropped. The `try_send` swallow is exercised only implicitly. |
| Subscription replacement (re-subscribe replaces tx) | None | **MISSING**: docstring claims "previous subscription is replaced" but no test verifies the prior receiver sees no further events after a re-subscribe. |
| Slow-subscriber non-blocking (advisory must never block data path) | None (implicit in `try_send`) | **MISSING**: no test demonstrates that emit returns immediately under back-pressure. |

### `crates/kiseki-control/src/node_lifecycle.rs`

| Spec invariant claimed | Tests covering | Tests missing |
|---|---|---|
| **I-N4 capacity pre-check** | `drain_succeeds_with_replacement_capacity` (385), `drain_refused_when_only_three_active_nodes` (402), `drain_re_issued_after_replacement_node_added` (420) | None — covers accept, refuse, and recovery. |
| **I-N6 audit trail** | All five events (`DrainRequested`, `DrainRefused`, `DrainCancelled`, `VoterReplaced`, `Evicted`) tested via `audit().iter().any(matches!)`. | **PARTIAL**: order of events in audit array is asserted only in `drain_re_issued_after_replacement_node_added` (lines 432-436); other tests use `.last()` or `.any()`, so an event interleaving regression would not be caught. |
| **I-N7 cancellation** | `cancel_drain_returns_node_to_active` (439-452) | **MISSING**: the "no rollback of completed replacements" guarantee from ADR-035 §4 is doc-only. A test that runs `record_voter_replaced` for shard 0, then `cancel_drain`, then asserts shard 0's replacement is *still* there would falsify any future change that added rollback. |
| **Failed → Draining transition** (drain a Failed node) | None | **MISSING**: `request_drain` checks `matches!(from, Active|Degraded|Failed)` (line 268) but no test exercises the `Failed` branch. |
| Concurrency cap (`max_concurrent_migrations = max(1, n/10)`) | None | **MISSING**: ADR-035 §3 specifies an I-SF4 concurrency bound; the orchestrator does not implement migration scheduling at all (it tracks state only). This is a documented carve-out — production migration runs elsewhere. Worth flagging in the gap log. |
| State-machine completeness (5 states) | All 5 states defined; `Draining → Evicted` driven by `record_voter_replaced` (line 332-348) | **MISSING**: `Active → Degraded`, `Degraded → Active`, `Active → Failed`, `Failed → Active` automatic transitions are not implemented in this module. ADR-035 says these are "automatic" — currently no code runs them. |

## Specific recommendations (for the implementer)

1. **Replace `then_lock_local`** (gateway.rs:511-525). Spawn a second
   `NfsContext` (or at minimum a second `NfsGateway` against the same
   gateway) in the World, replay the OPEN+LOCK sequence against it,
   and assert the second context shows zero locks while the first
   still shows ≥1. The current fresh-`LockManager` check is non-falsifiable.

2. **Replace `then_caller_queue_only`** (gateway.rs:1241-1254) and
   **`then_no_neighbour_headroom`** (gateway.rs:1454-1468). Both
   should subscribe a *second* workload ("neighbour") in the Given,
   emit telemetry only for "training-run-42", and assert the
   neighbour's `try_recv` returns `Err(TryRecvError::Empty)`. That is
   exactly what the unit test
   `backpressure_subscriber_receives_only_own_events` does — promote
   that pattern into the BDD step.

3. **Wire `DrainOrchestrator` to `RaftTestCluster`** or remove the
   pretence. Either: (a) add a `RaftHandle` field to `NodeRecord` and
   have `record_voter_replaced` accept a callback that runs the
   actual `add_learner`/`change_membership` calls before bumping
   `completed_shards`; or (b) drop the demonstration cluster from the
   step and document explicitly that the @integration scenario only
   verifies the orchestrator's state machine, not Raft. Today's code
   does (b) silently while looking like (a).

4. **Strengthen `then_capacity_validated`** (operational.rs:2249-2260).
   Before the When step, call `precheck` (or expose a `last_precheck_result`
   on the orchestrator) and assert it ran. The current `last_drain_error.is_none()`
   would still hold if `request_drain` skipped the pre-check entirely.

5. **`then_bucketed_fraction`** (gateway.rs:1445-1452): replace
   `assert!(used < 100)` (where `used == 0` always) with a real
   bucket lookup. After incrementing the budget enforcer to known
   values (e.g., 0/25/75/100 hints), assert the bucket name —
   `Ample`/`Moderate`/`Tight`/`Exhausted`. The bucketing function
   should live in `kiseki-advisory` (it currently does not appear to
   exist; `BudgetEnforcer` only exposes counts).

6. **Add `kiseki-advisory::telemetry_bus` overflow test**: subscribe
   one workload, emit 100 events, assert `try_recv` succeeds 64 times
   then returns Empty (or Disconnected). This proves the bounded-queue
   guarantee in the docstring at line 117.

7. **Add a `Failed → Draining` test** in `node_lifecycle.rs::tests`
   to exercise the third branch of the `request_drain` state guard
   (line 268). Currently untested — operator-drain-of-failed-node is
   spec'd as a supported transition (ADR-035 §3.1).

8. **Document the TLS gap explicitly** in protocol-gateway.feature
   §"NFS gateway over TCP" / §"S3 gateway over TCP (HTTPS)" or
   downgrade those scenarios to `@integration-no-tls` until the real
   TLS termination test exists. The current step assertions
   ("listener bound on a non-zero port") satisfy the *TCP* half but
   not the *TLS* half of the scenario name.

9. **Audit `then_tenant_admin_alerted`** (gateway.rs:877-947) — the
   step appends an `AuditEvent` from inside the test, then queries
   for it. The system-under-test (gateway/keystore) does not actually
   emit the event. Either wire the alert through `MemKeyStore::inject_unavailable`
   → audit emission, or downgrade this step's claim ("test verifies
   the audit log can record an alert" rather than "the gateway alerts
   the admin").

10. **Promote `derive_merged_name`** out of the test file (log.rs:1288-1300)
    if it's the canonical spec for merged-shard naming — otherwise
    the test convention will drift from production's naming.
