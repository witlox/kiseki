# ADR-025 Storage Administration API — Implementation Plan

**Created:** 2026-05-03
**Owner:** implementer (diamond workflow)
**Tracks:** `kiseki-server::admin_grpc`, `kiseki-proto::v1::storage_admin`,
`kiseki-server::bin::kiseki_admin`, `kiseki-chunk` / `kiseki-chunk-cluster`
mutation surfaces.

## Context

[ADR-025](../architecture/adr/025-storage-admin-api.md) proposes a 25-RPC
`StorageAdminService` covering device management, pool management,
performance tuning, observability, shard management, and repair / scrub.
Status today is **Proposed** — the proto schema doesn't exist and only
the snapshot RPCs from ADR-016 (`AdminService`) ship in
`kiseki-server::admin_grpc`. The user wants ADR-025 fully closed
(option **B** from the loop where this plan was scoped).

Sibling ADRs already shipped this plan must respect:

- ADR-016 — `AdminService` (snapshots) keeps its own gRPC service. New
  storage-admin endpoints live on a separate `StorageAdminService` so
  permissions can split.
- ADR-024 — Device management + capacity thresholds. `AddDevice` /
  `RemoveDevice` / `EvacuateDevice` mutate state ADR-024 already defines.
- ADR-026 — Per-shard Raft groups. Every cluster-wide mutation
  (AddDevice, CreatePool, SplitShard, SetTuningParams) goes through
  Raft on the cluster control shard — never bare in-memory state.
- ADR-029 / ADR-030 — Block allocator + small-file placement own the
  device + pool data model.
- ADR-033 — Initial shard topology. `SplitShard` is a non-trivial
  operation already defined here.
- ADR-034 — Shard merge mechanism. Surfaced as `MergeShards`
  (extension below).
- ADR-035 — Drain protocol. `EvacuateDevice` plumbs into the existing
  drain orchestrator.
- ADR-040 — Persistent metadata. New tuning-param state goes through
  the persistent CompositionStore meta table, not a fresh redb.

## Existing surfaces to build on

| Where | What's there | Reuse for |
|---|---|---|
| `kiseki-server/src/admin.rs` | pure `cluster_status() -> ClusterStatus` returning zeros + `to_table()` | `ClusterStatus` RPC body — needs real wiring |
| `kiseki-server/src/admin_grpc.rs` | tonic service impl pattern (3 ADR-016 RPCs) | template for `StorageAdminService` impl |
| `kiseki-server/src/cli.rs` | `parse_admin_args` recognises `status / pool list / device list / shard list / maintenance {on,off}` | wire the parser's stub branches to actual gRPC calls |
| `kiseki-server/src/bin/kiseki_admin.rs` | remote CLI binary (HTTP today, ~700 LOC — uses `--endpoint http://...`) | extend with `--grpc-endpoint` for the new service |
| `specs/architecture/proto/kiseki/v1/admin.proto` | snapshot RPCs only | leave as-is; `storage_admin.proto` is a new file |
| `kiseki-chunk/src/store.rs` | `ChunkStore::add_pool / pool / pool_mut` | back the read-only pool RPCs immediately |
| `kiseki-chunk-cluster/src/scrub*.rs` | scrub scheduler + `OrphanScrub` + `UnderReplicationPolicy` | back `TriggerScrub` / `ListRepairs` |
| `kiseki-chunk-cluster/src/placement.rs` | placement engine | back `RebalancePool` |
| `kiseki-control/src/grpc.rs` | tonic Server::builder pattern + interceptor wiring | template for the Server::builder bind |
| `kiseki-server/src/runtime.rs:1098` | data-path Server already lists existing services | adds `StorageAdminService` to the same builder |

## Scope

**In scope** (the full 25 RPCs from ADR-025 §"Admin API surface"):

| Group | RPCs |
|---|---|
| Device | `ListDevices` `GetDevice` `AddDevice` `RemoveDevice` `EvacuateDevice` `CancelEvacuation` |
| Pool | `ListPools` `GetPool` `CreatePool` `SetPoolDurability` `SetPoolThresholds` `RebalancePool` |
| Tuning | `GetTuningParams` `SetTuningParams` |
| Cluster obs | `ClusterStatus` `PoolStatus` `DeviceHealth` (stream) `IOStats` (stream) |
| Shard | `ListShards` `GetShard` `SplitShard` `SetShardMaintenance` |
| Repair | `TriggerScrub` `RepairChunk` `ListRepairs` |

**Extension** (from ADR-034 sibling decisions — adding so we don't have
to revisit later): `MergeShards`.

**Out of scope** (deferred to their own ADRs):
- Authentication / RBAC for admin RPCs — ADR-025 §"Audit + auth" lists
  these and there's already an mTLS interceptor pattern from
  `fabric_san_interceptor`. Reuse but don't widen.
- Web UI surface — separate, lives in `docs/admin/dashboard`.

## Non-goals

1. Re-implementing snapshot RPCs — `AdminService` keeps them.
2. Changing the existing `cluster_status` table format — `to_table()` stays.
3. Backwards-incompatible CLI changes — every existing `kiseki-admin`
   subcommand keeps working; new ones are additive.

## Workstreams (TDD-ordered, dependency-respecting)

Each workstream lands as its own commit with the failing test FIRST,
then the implementation, then the CLI wiring.

### W1 — Proto schema + service scaffolding

**Failing test:** `crates/kiseki-proto/build.rs` includes
`kiseki/v1/storage_admin.proto`; `cargo build -p kiseki-proto` regen
produces `kiseki::v1::storage_admin_service_server::StorageAdminService`.
Compile-time test asserts the trait exists.

**Implementation:**
- New `specs/architecture/proto/kiseki/v1/storage_admin.proto` —
  every message + service definition from ADR-025 §"Admin API surface"
  copy-pasted into protobuf. Include the `MergeShards` extension.
- Register the proto in `crates/kiseki-proto/build.rs` (audit ADR's CI
  Proto-Coverage check passes).
- Skeleton `crates/kiseki-server/src/storage_admin.rs` — wires a
  `StorageAdminGrpc` struct that holds `Arc` handles to:
  `chunk_store`, `cluster_chunk_store`, `view_store`, `audit_store`,
  `tuning_state`, `raft_handle`. Every RPC method `Status::unimplemented`.
- Service registered on the data-path `Server::builder` in
  `kiseki-server::runtime::run_main` next to the existing
  `AdminServiceServer`.

**Done when:** `grpcurl <node>:9000 list kiseki.v1.StorageAdminService`
shows all 26 RPCs (25 + MergeShards). Existing snapshot RPCs still work.

**Effort:** ~1 day. Pure mechanical.

---

### W2 — Read-only observability RPCs ✅ DONE (2026-05-03)

These don't mutate state — they project existing in-memory or
persistent data. They land first because they're independent of W4/W5.

| RPC | Source | Notes |
|---|---|---|
| `ClusterStatus` | aggregate from `cluster_chunk_store.cluster_nodes()` + `chunk_store.pools()` + raft membership | replaces the stub `cluster_status()`; preserve the `to_table()` consumer |
| `PoolStatus` | `chunk_store.pool(name)` + per-pool capacity from device manager | adds `used_bytes`, `device_count`, `target_fill_pct` |
| `ListPools` | `chunk_store.pools()` (need to add `pub fn pools()` getter) | currently only `pool / pool_mut(name)` exist |
| `GetPool` | `chunk_store.pool(name)` | exists |
| `ListDevices` | `chunk_store.pool(name)` → iterate `pool.devices` | flatten across all pools when no filter |
| `GetDevice` | scan pools for matching device id | needs new `chunk_store.find_device(id)` helper |
| `ListShards` | `cluster_chunk_store.cluster_shards()` (already used elsewhere) | maps `Vec<ShardId>` → `Vec<ShardInfo>` with leader + member counts |
| `GetShard` | as above + raft state for the shard | per-shard leader_id, last_applied, voters |
| `GetTuningParams` | new `TuningState` (W3 lays the foundation) | reads-only; W3 introduces the type — kept UNIMPLEMENTED in W2 |
| `ListRepairs` | `RepairTracker` ring buffer (new) | scrub_scheduler write-side lands in W4; W2 returns honest empty list |

**Status:** 9 of 10 read-only RPCs implemented (`GetTuningParams`
deferred to W3 since it depends on the `TuningState` model).

**Landed:**
- `crates/kiseki-server/src/storage_admin.rs` — `StorageAdminGrpc`
  builder pattern with `with_chunk_store` / `with_cluster` /
  `with_bootstrap_shard` / `with_repair_tracker` / `with_metrics`
- `crates/kiseki-chunk-cluster/src/repair_tracker.rs` — `RepairTracker`
  ring buffer (4096 cap), wire-format string contracts pinned
- `crates/kiseki-chunk/src/store.rs` — `pools()` + `find_device()`
- `crates/kiseki-chunk/src/async_ops.rs` — async trait surface
- `crates/kiseki-server/src/runtime.rs` — full wiring incl. counter

**Tracing + metrics (added during W2):**
- Every implemented handler wraps its body in `with_obs(rpc_name, ...)`
  which emits an OTEL span via `kiseki_tracing::span()` and bumps
  `kiseki_storage_admin_calls_total{rpc, outcome}` (Prometheus
  IntCounterVec, registered in `KisekiMetrics`)
- Outcome buckets: `ok`, `client_error` (4xx-equivalent codes),
  `server_error` (5xx-equivalent), `unimplemented` (W3-W7 stubs)
- The 17 unimplemented RPCs route through `self.unimpl(...)` which
  also emits a span + bumps the counter — operators see traffic to
  not-yet-implemented endpoints on `/metrics` without surprises
- A mechanical guard test (`every_implemented_rpc_uses_with_obs`)
  walks the source and asserts every implemented handler uses the
  helper — prevents regressions in W3-W7

**Tests landed (48 in `mod tests`):** 40 per-RPC behavioural tests
(real-impl assertions for the 9 implemented + `_unimplemented_until_w*`
for the 17 stubs); 6 metrics outcome-bucket tests; 2 cardinality
guards (`proto_declares_exactly_26_rpcs`, `rpc_test_coverage_is_complete`)
keeping the proto / impl / test counts in lockstep.

**CLI wiring (deferred):** the `parse_admin_args("status")` / etc.
wiring lives in W6. The CLI today still uses the legacy HTTP admin
endpoints — switching to the new gRPC service is a single workstream
once W3-W5 land their mutating counterparts.

**Effort actual:** ~1 day (vs ~1.5 day estimate; the chunk_store
helper landed cleanly, RepairTracker was straightforward).

---

### W3 — Tuning parameter state + management ✅ DONE (2026-05-03)

**Failing test:** `tests/storage_admin_tuning.rs` — `SetTuningParams`
with `compaction_rate_mb_s = 200` then `GetTuningParams` returns 200.
After a server restart (`KISEKI_DATA_DIR` set), `GetTuningParams`
still returns 200.

**Status:** the 2 RPCs (`GetTuningParams` / `SetTuningParams`) and
their backing state model landed; persistence rehydrates across
restart. Raft replication and most subsystem hooks are deferred
(see "Deferred" below).

**Landed:**
- `crates/kiseki-server/src/tuning.rs` — `TuningParams` (8 cluster
  parameters from ADR-025 §"Cluster-wide tuning"), per-field bounds
  via `validate()`, proto round-trip helpers, postcard
  Serialize/Deserialize.
- `TuningStore` — `tokio::sync::RwLock` snapshot + `watch::Sender`
  for live subscribers. `set()` validates → persists → swaps →
  broadcasts. Validation failure leaves backing state intact (no
  partial updates possible because the snapshot is replaced
  atomically).
- `TuningPersistence` trait + `InMemoryTuningPersistence` (default)
  + `RedbTuningPersistence` (own `<dir>/tuning.redb` file, single
  postcard row keyed by `"current"`).
- `with_persistence()` rehydrates on construction; out-of-range
  loaded values fall back to defaults with a warning log so a
  schema-tightening release boots cleanly across version skew.
- `crates/kiseki-server/src/storage_admin.rs` — `with_tuning_store()`
  builder; `get_tuning_params` / `set_tuning_params` go through
  `with_obs` (tracing + metrics); missing dep returns
  `FailedPrecondition` (not `Unimplemented`).
- `crates/kiseki-server/src/runtime.rs` — wires the redb-backed
  store under `cfg.data_dir/tuning/` when `KISEKI_DATA_DIR` is set,
  falls back to in-memory otherwise. Spawns a `subscribe()` observer
  task that logs every applied SetTuningParams at `tracing::info`
  (operator audit trail; W4/W5 hooks reuse the same channel).

**Tests landed (17 in `tuning::tests` + 7 in `storage_admin::tests`):**
- `defaults_are_in_range`, `defaults_match_adr_025_table`
- `proto_round_trip_preserves_all_fields`
- `validate_rejects_under_minimum`, `_over_maximum`,
  `_zero_for_non_zero_lower_bound_fields`,
  `_accepts_exact_boundaries`
- `store_get_returns_default_when_empty`,
  `_set_then_get_round_trips`,
  `_set_rejects_out_of_range_without_persisting`,
  `_subscribe_receives_change_notifications`
- `redb_persistence_round_trips_across_open`,
  `_load_empty_returns_none`
- `store_with_redb_persistence_rehydrates_on_restart`
  (the failing-test from the plan)
- `store_with_corrupted_persistence_falls_back_to_defaults`
- 7 RPC-level tests covering happy-path round-trip, missing dep,
  out-of-range rejection, missing `params` field, zero-init reject,
  `every_implemented_rpc_uses_with_obs` updated to require both
  RPCs use `with_obs`.

**Deferred to W5 / future workstreams (with rationale):**
- **Raft replication.** ADR-025 calls SetTuningParams "Raft-coordinated"
  but the necessary delta type (TuningParamsSet) sits naturally
  alongside the other cluster-control mutations in W5. The
  TuningStore API is shaped to support it without churn —
  followers will call the same `set()` from their hydrator step.
  Today `committed_at_log_index` returns 0 (single-node
  semantics) per the proto field's W5 note.
- **Per-subsystem hooks** (compaction throttle, scrub interval,
  etc.). The `subscribe()` channel + observer log lands in W3 so
  the wire-up surface exists; each subsystem's actual hook lands
  with that subsystem's W4/W5 work (e.g. scrub_interval_h hook
  lands alongside W4's TriggerScrub which already wires the scrub
  scheduler).

**Effort actual:** ~0.6 days (state model + persistence + 2 RPCs +
24 tests). The deferred Raft + hooks bundle into ~1 day of W5 work
when those subsystems land.

---

### W4 — Simple mutating RPCs (single-node, no Raft) ✅ DONE (2026-05-03)

These mutate node-local state only — no cluster coordination needed.

| RPC | Mechanism | Notes |
|---|---|---|
| `SetShardMaintenance` | atomic flag in `MaintenanceMode` shared with `ClusterChunkServer` | gates writes to the shard; reads stay served |
| `CancelEvacuation` | `EvacuationRegistry::cancel(id)` flips the registered `EvacuationProgress.cancelled` atomic | the drain orchestrator (W5 `EvacuateDevice`) is the producer; W4 ships the registry |
| `RepairChunk` | new `ScrubScheduler::repair_one_chunk(chunk_id)` reuses `UnderReplicationScrub` for the single-chunk case | RepairTracker entry on start + finish so `ListRepairs` shows progress |
| `TriggerScrub` | new `ScrubScheduler::trigger_now()` spawns `run_once()` | returns immediately with a `scrub_id`; report-on-completion lands in `ListRepairs` |

**Landed:**
- `crates/kiseki-chunk-cluster/src/maintenance.rs` — `MaintenanceMode`
  with per-shard `AtomicBool` registry. 5 inline tests.
- `crates/kiseki-chunk-cluster/src/server.rs` — `with_maintenance()`
  builder + `is_in_maintenance(shard)` check at the top of
  `put_fragment()` returning `FailedPrecondition` when set.
- `crates/kiseki-chunk/src/evacuation.rs` — `EvacuationRegistry`
  (HashMap<id, Arc<EvacuationProgress>>) with register/cancel/
  unregister/ids. `EvacuationProgress` derives Debug. 4 new tests.
- `crates/kiseki-chunk-cluster/src/scrub_scheduler.rs` —
  `trigger_now()` + `repair_one_chunk()`. The latter walks
  `cluster_chunk_state` for the requested id, dispatches to the
  configured `UnderReplicationScrub` (EC or replication N), and
  reports `already_healthy` when nothing needed repair.
- `crates/kiseki-server/src/storage_admin.rs` — 4 RPCs implemented
  via `with_obs`. Helpers: `parse_chunk_id_hex`, `parse_shard_id`,
  `now_ms`. RepairTracker write-path now real (Manual + Scrub
  trigger entries land + transition to terminal state).
- `crates/kiseki-server/src/runtime.rs` — `MaintenanceMode` and
  `EvacuationRegistry` constructed early; same `Arc`s shared with
  data-path server + admin handler. `ScrubScheduler` hoisted out
  of the if-block so it can flow into `with_scrub`.

**Tests landed (15 in storage_admin::tests + 9 across the new
modules):**
- `set_shard_maintenance_flips_flag_in_shared_store` — proves the
  admin RPC writes the same atomic the data path consults.
- `set_shard_maintenance_disable_clears_flag`
- `set_shard_maintenance_unknown_shard_returns_not_found`
- `set_shard_maintenance_invalid_uuid_returns_invalid_argument`
- `set_shard_maintenance_empty_id_returns_invalid_argument`
- `set_shard_maintenance_without_dep_returns_failed_precondition`
- `cancel_evacuation_cancels_registered_progress` — proves the
  admin RPC flips `EvacuationProgress.cancelled` for the worker.
- `cancel_evacuation_unknown_id_returns_not_found`
- `cancel_evacuation_empty_id_returns_invalid_argument`
- `cancel_evacuation_without_dep_returns_failed_precondition`
- `trigger_scrub_without_dep_returns_failed_precondition`
- `repair_chunk_without_dep_returns_failed_precondition`
- `repair_chunk_invalid_hex_returns_invalid_argument`
- `parse_chunk_id_hex_round_trips_with_hex_encode`
- `parse_chunk_id_hex_rejects_non_hex_chars`
- `parse_shard_id_round_trips_with_uuid`
- `every_implemented_rpc_uses_with_obs` extended to cover all 4 W4
  RPCs.
- 5 maintenance + 4 evacuation registry inline tests covering
  per-shard isolation, idempotence, register/cancel/unregister.

**Tests deferred to integration land:**
- Full `TriggerScrub → ScrubScheduler::trigger_now() → ListRepairs`
  end-to-end. The unit tests cover trigger validation; integration
  needs a fabric peer for `ScrubScheduler::new()` to be useful, so
  it lands in `kiseki-acceptance` BDD scenarios under
  `storage-admin.feature`.
- `RepairChunk` happy-path with a real ClusterChunkStore +
  multi-node fabric — same dependency as TriggerScrub. The
  scheduler-side `repair_one_chunk` is unit-tested by extending the
  existing scrub_scheduler tests (FakeRepairer + FakePeerOracle
  already mocked there).

**Cluster scope (W4 vs W5):**
- All 4 W4 RPCs are *node-local mutations*. `committed_at_log_index`
  returns 0. W5 will optionally lift `SetShardMaintenance` and
  `CancelEvacuation` to Raft-coordinated deltas if cluster-wide
  consistency turns out to matter operationally. `RepairChunk` and
  `TriggerScrub` stay node-local by design (they're operator-driven
  one-shots, not cluster state).

**Effort actual:** ~0.8 days (the maintenance module landed
cleanly; the scrub_scheduler `repair_one_chunk` reused the existing
under-replication pipeline with minimal new code).

---

### W5 — Raft-coordinated mutating RPCs

The hard ones. Each needs a delta type, a leader-side validation,
follower apply, audit emission, and recovery semantics.

| RPC | Delta | Coordination |
|---|---|---|
| `AddDevice` | `DeviceAdded { pool, device }` | leader validates capacity range; followers add to local `ChunkStore::pool_mut(pool).devices`. ADR-024 / ADR-029 already define the device shape. |
| `RemoveDevice` | `DeviceRemoved { pool, device_id }` | requires device empty-check (no chunks placed). Returns `FailedPrecondition` with chunk count if not. |
| `EvacuateDevice` | `EvacuationStarted { pool, device_id }` | hands off to existing drain orchestrator (ADR-035). Returns immediately with `evacuation_id`. Progress polled via `GetDevice`. |
| `CreatePool` | `PoolCreated { pool: AffinityPool }` | followers add via `add_pool`. Validates name uniqueness. |
| `SetPoolDurability` | `PoolDurabilityChanged { pool, strategy }` | requires pool empty OR a one-time chunk migration plan. v1 rejects with `FailedPrecondition` when pool has chunks; migration is a separate ADR. |
| `SetPoolThresholds` | `PoolThresholdsChanged { pool, warning_pct, critical_pct, readonly_pct }` | followers update; Capacity engine watches. |
| `RebalancePool` | leader-only; spawns a long-running task | returns `rebalance_id`; status via `PoolStatus`. |
| `SplitShard` | reuses ADR-033 split machinery | RPC just triggers it; the heavy lifting already exists. |
| `MergeShards` | reuses ADR-034 merge | same shape as Split. |

**Failing test:** for each, `tests/storage_admin_raft_mutations.rs`
spawns a 3-node `ClusterHarness`, calls the RPC on the leader,
asserts both the leader AND a follower see the mutation via the
matching read RPC after raft commit.

**Implementation pattern (per RPC):**
1. Add the delta variant to the cluster control-shard delta enum
2. Leader-side validation in the RPC handler before proposing
3. Follower apply in the hydrator
4. Audit event (ADR-009 / ADR-015 contract — admin-action mutations
   are auditable)
5. Recovery: idempotent on duplicate apply (Raft replay safe)

**Effort:** ~3-4 days for the 9 RPCs at this tier. The infrastructure
(W1-W4) does most of the wiring; this is per-RPC body work.

---

### W6 — kiseki-admin CLI parity + UX polish

**Failing test:** `tests/cli_admin_subcommands.rs` walks every
ADR-025 RPC via the CLI binary against a live cluster and confirms
the response is rendered.

**Implementation:**
- New subcommands: `pool create / set-durability / set-thresholds /
  rebalance`, `device add / remove / evacuate / cancel-evac`,
  `shard split / merge / maintenance {on,off}`, `tuning {get,set}`,
  `scrub trigger`, `repair {chunk,list}`.
- Output formatting: tabular by default, `--format=json` for
  scripting, `--watch` for the streaming RPCs (`DeviceHealth`,
  `IOStats`).

**Effort:** ~1 day.

---

### W7 — Streaming RPCs

`DeviceHealth` and `IOStats` are server-streaming. Pattern:

- Server: spin a `tokio::sync::broadcast` channel inside the relevant
  subsystem (chunk-cluster for IOStats, chunk for DeviceHealth);
  RPC handler subscribes and forwards into a tonic `Streaming`.
- Client: reads frames in a loop, prints each one.

**Failing test:** `tests/storage_admin_streams.rs` — start the stream,
trigger an event (e.g. write a chunk so IOStats produces output),
assert the client sees at least one frame within 1 s.

**Effort:** ~1 day.

---

## Audit gates

Per the diamond workflow:

- **Adversary review** before W3 (tuning param Raft delta is the most
  risky piece — get adversary sign-off on bounds/range/coordination).
- **Auditor review** between W4 and W5 (verify the simple mutating
  RPCs aren't smuggling cluster-state through node-local paths).
- **Auditor review** after W5 (verify Raft delta safety + replay
  idempotence + audit emission for each mutating RPC).
- **Integrator** at the end (cross-cuts: CLI / docs / metrics).

## Risks + open questions

1. **`SetPoolDurability` on a non-empty pool** — ADR-025 says
   "tunable", ADR-005 + ADR-024 say "static at creation". W5 picks
   `FailedPrecondition` for v1. Architect to confirm before we ship
   migration as a separate ADR.
2. **`EvacuateDevice` resume** — drain orchestrator (ADR-035) doesn't
   currently survive restart. If the leader restarts mid-evacuate,
   the operation aborts. v1 returns the partial state via `GetDevice`;
   user re-issues.
3. **`MergeShards` cross-tenant** — ADR-034 only covers single-tenant
   merges. Reject merges across tenants with `InvalidArgument`.
4. **Streaming RPC backpressure** — slow consumer can stall the
   subsystem if we use unbounded channels. Plan: bounded
   `broadcast(1024)` with `BroadcastStream::new` and a `dropped`
   counter exposed via Prometheus.
5. **Auth / RBAC** — out of scope here per Non-goals. Today the
   AdminService relies on the data-path TLS interceptor; same applies
   to StorageAdminService. Production deployments should add an
   admin-cert SAN check before exposing this.

## Estimated effort summary

| Workstream | Days | Cumulative |
|---|---|---|
| W1 — proto + scaffolding | 1.0 | 1.0 |
| W2 — 10 read-only RPCs | 1.5 | 2.5 |
| W3 — tuning state + 2 RPCs | 2.0 | 4.5 |
| W4 — 4 simple mutating RPCs | 1.0 | 5.5 |
| W5 — 9 Raft-coordinated RPCs | 3.5 | 9.0 |
| W6 — CLI parity | 1.0 | 10.0 |
| W7 — 2 streaming RPCs | 1.0 | 11.0 |

**Total: ~11 dev-days end to end** for Accepted (fully implemented).

After completion, ADR-025 status flips:
`Proposed` → `Accepted (CompositionStore landed in commit 9e55e64;
StorageAdminService landed across W1-W7 commits, see this plan)`
in both `specs/architecture/adr/` and `docs/decisions/adr/`.

## Tracking

Each workstream lands as one or more commits referenced by header
in this file as it's done. Tasks (TaskCreate ids) tracked in the
session task list and crossed off when the corresponding workstream
ships.
