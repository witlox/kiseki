# Phase 17 — Integrator Pass

**Integrator**: Integrator role
**Date**: 2026-04-30
**Subject**: Phase 17 items 2 + 3 (commits `9e55e64` → `77ac00f`) plus
the auditor follow-up commit (`77ac00f`)
**Verdict**: **Ready for final adversary pass.** No cross-context
issues found; two integration tests added to close the auditor's
A4 / A5 seams that didn't have e2e coverage.

## Cross-context seams examined

| Seam | Verified by | Verdict |
|---|---|---|
| runtime → `CompositionStore::with_storage(PersistentRedbStorage)` → redb file under `KISEKI_DATA_DIR/metadata/compositions.redb` | `test_persistence_survives_node_restart` (new) + the existing 9-test cross-node suite running against `docker-compose.3node.yml` (which sets `KISEKI_DATA_DIR=/data` on every node) | OK |
| runtime → hydrator → `CompositionStore::storage_mut().apply_hydration_batch(...)` → atomic redb txn (data + meta in one fsync) | unit tests in `kiseki-composition::persistent::redb::tests` + integration via the cross-node e2e tests where the hydrator on node-2/node-3 has to apply leader-emitted Create deltas before the gateway reads them | OK |
| gateway read path → `storage().halted()` → `GatewayError::ServiceUnavailable` → `s3_server::get_object` 503 + `Retry-After` | `mem_gateway::halt_mode_tests::*` (3 unit tests) + `s3_server::tests::get_object_returns_503_when_hydrator_halted` (1 unit test) | OK |
| runtime → `KisekiMetrics::gateway_retry: Arc<GatewayRetryMetrics>` → Prometheus registration → `/metrics` exposition | **`test_phase_17_metrics_surface_includes_gateway_retry_counters` (new, e2e)** — proves all 3 nodes surface both `kiseki_gateway_read_retry_total` and `kiseki_gateway_read_retry_exhausted_total` on `/metrics` | OK |
| runtime → `gw.compositions_handle()` → `UiState::compositions` → `/cluster/shards/{shard_id}/leader` `composition_hydrator_halted` field | extended `test_per_shard_leader_agrees_across_nodes` asserts the field is present + value is `false` in steady-state | OK |
| docker-compose volume `node2-data:/data` → `docker compose stop` preserves volume → `docker compose start` reopens the same redb → composition survives | `test_persistence_survives_node_restart` (new, e2e) — PUT, restart node-2, GET on node-2 still returns the bytes | OK |
| release.yml multi-node phase → `pytest test_cross_node_replication.py` (now 9 tests including the 2 new integration tests) | release workflow runs the same suite; CI's docker build will pick up the latest commit | OK |

## Integration smells scanned for

- **Dual write** (gateway delete: emit Delete delta + local delete): both happen under the same `compositions.lock().await` per Phase 17 item 1 — no inconsistency window for concurrent readers on the same node. Cross-node followers converge through the hydrator on the next poll. **OK.**
- **Assumed ordering** (hydrator → namespace lookup): the bootstrap namespace is installed on every node before the hydrator spawns (Phase 16f §D6.3 fix at `runtime.rs`). The order is preserved in commit `77ac00f`. **OK.**
- **Error swallowing** (hydrator `apply_hydration_batch` failure): the hydrator logs at warn and returns 0 from `poll`. The next poll retries. Persistence semantics are preserved because nothing was committed. **OK.**
- **Schema evolution** (postcard struct field add): no field added in this phase. ADR-040 §D8 + I-CP3 specify the upgrade path; first triggers when a schema_version=2 record exists. Future concern, not a current gap.
- **Phantom dependency** (gateway depends on hydrator's last_applied_seq being durable): made explicit by the auditor's A4 closure — the new test exercises the seam end-to-end. **OK.**
- **Metric registration race** (two nodes registering the same metric in the same Prometheus registry): each node owns its own registry, so no race. **OK.**

## Tests added

```
tests/e2e/test_cross_node_replication.py
├── test_phase_17_metrics_surface_includes_gateway_retry_counters
│   └── verifies all 3 nodes surface kiseki_gateway_read_retry_total
│       + _exhausted_total on /metrics — closes A5's "is the wiring
│       actually in the response?" question.
└── test_persistence_survives_node_restart
    └── PUT on node-1, hydrate to node-2, stop+start node-2, GET on
        node-2 still works — closes auditor finding A4 (gateway-
        level test exercising PersistentRedbStorage end-to-end).
```

## End-to-end suite status

```
$ ./.venv/bin/pytest -ra --tb=short test_cross_node_replication.py
============================= 9 passed in 92.45s ===============================
```

Test runtime grew from 80 s → 92 s (the new persistence test takes
~12 s for the stop/start dance). Acceptable.

## Build verification

- `cargo build --release -p kiseki-server` clean (1m44s).
- `cargo test --workspace --exclude kiseki-acceptance` clean.
- `cargo clippy --workspace --no-deps -- -D warnings` clean.
- 3-node docker image builds clean from `Dockerfile.server`.

## Remaining items (deferred)

These are not integration concerns; they're tracked for either the
final adversary pass or a sibling cleanup PR.

| Item | Source | Status |
|---|---|---|
| A3 — hydrator gap-detection trigger needs a stub `LogOps` | auditor | unit-test scope; ~40 LOC follow-up |
| A6 — `KISEKI_GATEWAY_READ_RETRY_BUDGET_MS` env-parsing test | auditor | low-impact; can be inline in a future PR |
| A7 — `PersistentRedbStorage::put()` cache-after-commit ordering | auditor | parallel test of the existing apply_hydration_batch coverage |
| 11 composition-side metrics from §D10 | architect's deferral | tracked under "Phase 17 follow-ups" — landing as the F-4 closure was the priority |
| D6.2 compaction-aware snapshot bundle | architect's deferral | sibling ADR when openraft log compaction is enabled |

## Readiness recommendation

**Ready for final adversary pass.** The cross-context seams all
exercise correctly under the e2e suite + workspace tests. The
two new integration tests act as defense in depth for the `/metrics`
registration path and the persistence-across-restart contract,
both of which would otherwise rely entirely on unit-level coverage
that doesn't see the runtime → registry → HTTP exposition flow.

The deferred items in the table above are reasonable to address
either in a sibling PR before release or after release as part of
ongoing observability + test-coverage work. Nothing in the
deferred set is a release blocker.
