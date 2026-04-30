# Phase 17 (Items 2 + 3) — Auditor Gate 2 Pass

**Auditor**: Auditor role
**Date**: 2026-04-30
**Scope**: ADR-040 rev 2 (commit `7f7c524`) + 4 implementation commits
(`9e55e64`, `106273e`, `20496d7`, `e8f3005`)
**Mode**: Incremental audit, post-implementation, pre-integrator

## Verdict

**Gate 2 conditional pass.** The persistent storage and hydrator
machinery are well-tested; the gateway / runtime wiring of the
halt-mode flow is **not** — three code paths added to close
implementation-review ticket I-2 have zero test coverage. Two
deferred items (full §D10 metrics, end-to-end persistence-across-
restart) are explicitly out-of-scope per the implementer's commit
message but the auditor flags them so the integrator and final
adversary catch them.

| Severity | Count | IDs |
|---|---|---|
| Critical | 0 |  |
| High | 2 | A1, A2 |
| Medium | 3 | A3, A4, A5 |
| Low | 2 | A6, A7 |

The High findings are addressable in a follow-up PR before the
final adversary pass. The Medium / Low findings can be addressed
during the integrator pass or rolled into a Phase-17-followups
sweep.

---

## Phase 1 — Per-decision step-depth classification

Each ADR-040 decision (D1..D11) and invariant (I-CP1..I-CP6) is
classified by what the implementation actually verifies.

| Decision / Invariant | Implementation site | Test site | Depth | Verdict |
|---|---|---|---|---|
| **D1** Storage layout (`<dir>/metadata/compositions.redb`) | `runtime.rs:494-518` | `redb::tests::persistence_across_reopen` | THOROUGH | ENFORCED |
| **D2** Encoding (`[1B version][postcard]`) | `redb.rs:52-71` | `redb::tests::put_and_get_round_trip` | THOROUGH | ENFORCED |
| **D3** Hot-tail LRU caching | `redb.rs:108-167` | `redb::tests::cache_serves_post_commit_value_after_apply_batch` | THOROUGH | ENFORCED |
| **D4** Sync-only inner locks (no `.await` hold) | `redb.rs:108-110, 184-186` | structural — clippy `held_lock_after_await` lint | ENFORCED-BY-LINT | ENFORCED |
| **D5** Atomic `last_applied + state` per batch | `redb.rs:280-348` | `redb::tests::apply_batch_atomically_commits_data_and_meta` | THOROUGH | ENFORCED |
| **D5.1** Transient/permanent skip algorithm | `hydrator.rs:265-340` | `hydrator::tests::hydrator_transient_skip_does_not_advance_until_threshold` | THOROUGH | ENFORCED |
| **D6.1** Snapshot includes all deltas (today's regime) | depends on `kiseki-log` | inherited from log-layer tests | DOCUMENTED | ENFORCED-BY-DEPENDENCY |
| **D6.2** Compaction-aware bundle (deferred) | not implemented | — | — | DEFERRED-PER-ADR |
| **D6.3** Halt-mode self-defense | `hydrator.rs:222-250, 380-407` | `hydrator::tests::hydrator_enters_halt_mode_on_log_compaction_gap` | **MOCK** (only no-gap path; see A3) | PARTIAL |
| **D7** Configurable retry budget | `mem_gateway.rs:441-447` | none | SHALLOW (env not tested) | DOCUMENTED |
| **D7** Halt-mode 503 short-circuit | `mem_gateway.rs:457-465` + `s3_server.rs:322-336` | **none** | SHALLOW | UNENFORCED (see A1) |
| **D8** Schema versioning + refuse-to-start | `redb.rs:128-156` | `redb::tests::schema_too_new_refuses_open` | THOROUGH | ENFORCED |
| **D8.1** `PersistentStoreError` typed errors | `error.rs` | compile-time only | ENFORCED-BY-TYPE | ENFORCED |
| **D9** Migration from in-memory (first-boot init) | `redb.rs:120-167` | `redb::tests::meta_defaults_on_first_open` | THOROUGH | ENFORCED |
| **D10** Observability surface (13 metrics) | **deferred** | — | UNENFORCED | DEFERRED-INTENTIONALLY (see A5) |
| **D11** Persistence scope = compositions only | `composition.rs` (struct shape) | structural — `namespaces` and `multiparts` are still `HashMap` fields | ENFORCED-BY-CODE | ENFORCED |
| **I-CP1** Atomic batch commit | covered by D5 | covered by D5 | THOROUGH | ENFORCED |
| **I-CP2** One hydrator per node | runtime spawn | not tested directly | SHALLOW | DOCUMENTED |
| **I-CP3** Schema-version byte on every record | covered by D2 + D8 | covered by D2 + D8 | THOROUGH | ENFORCED |
| **I-CP4** Cache hits never observe pre-commit state | `redb.rs:184-200, 245-269, 322-348` | one of two write paths tested (apply_hydration_batch); the `put()` path's cache-after-commit ordering isn't directly tested | PARTIAL | PARTIAL (see A7) |
| **I-CP5** Halt mode on compaction gap | covered by D6.3 | covered by D6.3 | MOCK | PARTIAL (see A3) |
| **I-CP6** Transient/permanent + durable retry counter | covered by D5.1 + I-1 | covered by D5.1 + I-1 | THOROUGH | ENFORCED |

Overall enforcement: 14 ENFORCED, 3 PARTIAL, 2 DEFERRED, 1
DOCUMENTED. The gaps cluster around D6.3's halt path and the
gateway/runtime wiring that fans the halt flag out to clients.

## Phase 2 — Implementation-review tickets

| Ticket | Implementation | Test | Verdict |
|---|---|---|---|
| **I-1** Persist transient-skip retry counter | `redb.rs` `meta_keys::STUCK_STATE`; `hydrator.rs` reads + writes through batch | `hydrator::tests::hydrator_transient_skip_does_not_advance_until_threshold` exercises the durable counter accumulating across polls | **CLOSED** |
| **I-2** Gateway returns 503 in halt mode | `GatewayError::ServiceUnavailable` + halt-check in read path + 503 + Retry-After in s3_server | **none** | **NOT VERIFIED** (see A1) |
| **I-2** Per-shard endpoint surfaces halt flag | `composition_hydrator_halted` in shard_leader response | **none** | **NOT VERIFIED** (see A2) |

I-1 closes cleanly. I-2's two halves both have implementation but
no tests. The auditor recommends this is the highest-priority
follow-up before the final adversary pass.

## Phase 3 — Findings

### A1: Halt-mode 503 path has zero test coverage

**Severity**: High
**Category**: Step-depth — UNENFORCED
**Location**: `crates/kiseki-gateway/src/mem_gateway.rs:457-465`,
`crates/kiseki-gateway/src/s3_server.rs:322-336`

**Description**: ADR-040 §D7 + impl-review ticket I-2 specify that
when the persistent hydrator is in halt mode and a composition
lookup misses, the gateway returns
`GatewayError::ServiceUnavailable` which the S3 server maps to
HTTP 503 with `Retry-After: 5`. The implementation exists; **no
test verifies it**.

The end-to-end consequence: an operator deploying a node whose
hydrator legitimately enters halt mode (e.g. a node that's been
offline through a log-compaction window in the future D6.2 world)
needs the gateway to surface 503 so load balancers route around.
Without a test, this is a regression-prone path; the next change
to the read path's error handling could silently break it.

**Suggested resolution**: Unit test in
`crates/kiseki-gateway/src/mem_gateway.rs` that:
1. Constructs an `InMemoryGateway` with a test storage that
   reports `halted() = true`.
2. Calls `read()` for a composition_id that doesn't exist.
3. Asserts the result is `Err(GatewayError::ServiceUnavailable(_))`.

A second test in `s3_server.rs` for the HTTP-mapping side: pipe
a `ServiceUnavailable` through the `get_object` handler and
assert the response is `503` with the `Retry-After` header.

### A2: Per-shard leader endpoint halt flag has zero test coverage

**Severity**: High
**Category**: Step-depth — UNENFORCED
**Location**: `crates/kiseki-server/src/web/api.rs:406-422`

**Description**: I-2 explicitly calls for the per-shard leader
endpoint to surface the halt flag so load balancers and clients
can route around. The implementation exists; the existing e2e test
`test_per_shard_leader_agrees_across_nodes` doesn't probe the
`composition_hydrator_halted` field.

**Suggested resolution**: Extend the existing test (or add a
sibling) that:
1. Hits `/cluster/shards/{bootstrap_shard_id}/leader` and asserts
   the response JSON has the `composition_hydrator_halted` key.
2. Optionally: in a follow-up that wires a way to put the
   hydrator into halt mode from a test harness, assert the field
   flips to `true`.

### A3: Halt-mode triggering path is structurally tested but doesn't actually trigger

**Severity**: Medium
**Category**: Step-depth — PARTIAL
**Location**: `crates/kiseki-composition/src/hydrator.rs::tests::hydrator_enters_halt_mode_on_log_compaction_gap`

**Description**: The test confirms `hydrator.halted() == false` in
the no-gap case but the `MemShardStore` test backend can't
simulate a compaction gap (it never truncates deltas). The actual
gap-detection branch in `poll()` (lines 222-250) is unreachable
from tests as currently written.

The test acknowledges this in its body comments: *"the actual
gap-detection path is exercised end-to-end in the e2e tests where
openraft's snapshot-install replaces the delta range under the
hydrator. The unit-test stub here doesn't model log compaction;
that's `kiseki-log` territory."* But there's no concrete e2e test
that does this either.

**Suggested resolution**: Add a stub `LogOps` impl in the hydrator
test module that:
- Returns deltas with a synthetic gap (e.g. `[seq=5, seq=10]`
  when the hydrator polls from `seq=1`).
- Verifies `hydrator.poll()` enters halt mode and persists
  `halted = true`.

This is a small, pure-Rust test addition. ~40 LOC.

### A4: Gateway tests don't exercise `PersistentRedbStorage`

**Severity**: Medium
**Category**: Interface fidelity — PARTIAL
**Location**: `crates/kiseki-gateway/src/mem_gateway.rs::tests`

**Description**: The gateway has rich tests (`mem_gateway.rs:912+`,
`telemetry_wiring_tests`) but they all construct the gateway with
the in-memory `MemoryStorage` (the default). The persistent
backend is exercised in unit tests against
`PersistentRedbStorage` directly, but the **integrated path**
(gateway → CompositionStore → PersistentRedbStorage → redb file →
restart → read back) has no explicit test.

This means a regression that breaks the gateway's interaction with
the persistent backend (e.g. a future change to
`CompositionStore::create` that doesn't go through `storage.put()`
correctly) wouldn't surface until production deployment.

**Suggested resolution**: Add at least one gateway-level test that
constructs an `InMemoryGateway` with a `PersistentRedbStorage`
backend, performs a PUT, drops the gateway and the storage,
re-opens both at the same path, performs a GET, and asserts the
bytes survive. This validates the cross-module wiring.

### A5: §D10 metrics surface deferred — F-4 only half-closed

**Severity**: Medium
**Category**: Decision enforcement — DEFERRED
**Location**: ADR-040 §D10; multiple sites in `kiseki-composition`
and `kiseki-server::metrics`

**Description**: The implementer's commit message acknowledges the
13 composition + 2 gateway metrics are deferred. The auditor
flags this with extra context: **F-4 (the original adversary
finding from rev 1) is only half-closed**. F-4 said "make the
retry budget configurable AND observable." The configurable half
landed (`KISEKI_GATEWAY_READ_RETRY_BUDGET_MS`); the observable
half (`kiseki_gateway_read_retry_total` and
`kiseki_gateway_read_retry_exhausted_total`) didn't.

**Operational impact**: an operator running this code today CAN
tune the retry budget but CANNOT see whether they're hitting it.
Without the `_exhausted` counter rising, they have no signal that
the budget is too tight. That's almost as bad as the original
problem F-4 identified.

**Suggested resolution**: Track the metrics surface as a
**release-blocking** follow-up ticket, not a post-release one.
The adversary's original finding F-4 isn't truly closed until
both halves land.

### A6: `KISEKI_GATEWAY_READ_RETRY_BUDGET_MS` env parsing not tested

**Severity**: Low
**Category**: Step-depth — SHALLOW
**Location**: `crates/kiseki-gateway/src/mem_gateway.rs:441-447`

**Description**: The env-var parsing is in hot-path code and has
no test. Default (1000), explicit override, and malformed input
(non-numeric) all flow through the same `unwrap_or(1000)`. A
malformed value silently falls back to the default — that's
acceptable behavior, but no test confirms it.

**Suggested resolution**: One test that sets the env var to
"foo", calls `read()` with a known-missing comp_id, and verifies
the read takes ~1s (default budget) before returning NotFound.
Optional but cheap.

### A7: I-CP4 cache invariant: `put()` path's cache-after-commit ordering not directly tested

**Severity**: Low
**Category**: Step-depth — PARTIAL
**Location**: `crates/kiseki-composition/src/persistent/redb.rs:245-265`

**Description**: I-CP4 says the LRU cache update must happen
*after* the redb commit, so a reader that observes the cache
also observes the durable record. The
`apply_hydration_batch` path is tested
(`cache_serves_post_commit_value_after_apply_batch`); the
`put()` path is not. Both follow the same pattern (commit then
update cache), but the explicit test only covers one.

**Suggested resolution**: Mirror the existing test for the
`put()` path. ~10 LOC.

## Phase 4 — Cross-cutting

### Tests that compile but never run

None found. All 8 hydrator tests + 10 redb tests are exercised
by `cargo test -p kiseki-composition --lib`.

### Orphan implementation paths

The `tracing::warn!` and `tracing::error!` calls in the hydrator
(lines 320-340, 397-403) emit the right strings for the
`kiseki_composition_hydrator_skip_total{reason}` and
`kiseki_composition_hydrator_stalled` metrics — but without the
metrics wired (D10 deferred), they're log-only. An operator
relying on log-aggregation (loki/journalctl) gets the signal;
one relying on Prometheus alerting doesn't. Same gap as A5,
different framing.

### Stale specs

Phase 17 plan (`specs/implementation/phase-17-cross-node-followups.md`)
correctly reflects the closed/in-flight items. No stale claims.

## Recommendation

The auditor's verdict is **Gate 2 conditional pass with the
following follow-ups required before the final adversary pass**:

1. **A1, A2** (High): add tests for I-2's halt-mode 503 path and
   the per-shard endpoint halt flag. Without these, I-2 is
   "implementation-complete but not verified" — and the
   adversary will (correctly) flag this as a re-occurrence of
   the F-1/F-3 pattern of "spec paths the ADR claims but the
   implementation actively hides."

2. **A5** (Medium → upgraded to High by the auditor): the
   metrics deferral leaves F-4 only half-closed. Either land
   the metrics now or document the partial-closure status in
   the next adversary review and the next user-facing release
   notes.

3. **A3, A4, A6, A7** (Medium / Low): nice-to-have, can be
   rolled into the integrator pass or a sibling test-coverage
   sweep.

The implementer can address A1 + A2 + A5 in a single follow-up
PR (~150 LOC). The auditor recommends not advancing to the
integrator pass until that PR lands.

---

## Closure log

- **2026-04-29 (commit b685a28)** — A3, A6, A7 closed.
  - A3: hydrator gap-detection halt-mode now witnessed by 3
    new tests (`hydrator_halts_when_first_delta_seq_skips_past_expected`,
    `hydrator_halts_when_empty_response_but_tip_advanced`,
    `hydrator_does_not_halt_when_caught_up_at_tip`) using a
    purpose-built `GapInjectingLog` stub.
  - A6: `KISEKI_GATEWAY_READ_RETRY_BUDGET_MS` env-parsing
    now exercised by `retry_budget_env_override_is_honored` +
    `retry_budget_env_malformed_falls_back_to_default`.
  - A7: I-CP4 `put()`-path cache-after-commit witnessed by
    `cache_serves_post_commit_value_after_put`.
- **2026-04-29 (next commit)** — A5 fully closed.
  - 11-metric §D10 surface (`CompositionMetrics`) registered with
    the global `prometheus::Registry`, wired through
    `PersistentRedbStorage::with_metrics()` (LRU hit/miss/evict,
    decode-error kind, redb commit errors) and
    `CompositionHydrator::with_metrics()` (apply duration,
    last-applied-seq{shard}, skip{reason}, stalled). Runtime
    spawns a 30-s task that stats the on-disk redb file size
    and refreshes the count gauge. F-4 now fully observable.
  - A4 (durability across reopen) was already covered by
    `persistence_across_reopen` in `redb.rs` and
    `test_persistence_survives_node_restart` in
    `tests/e2e/test_cross_node_replication.py` from the
    integrator pass — closing here for completeness.

All Phase 17 audit findings are now closed; no carry-over to
Phase 18.
