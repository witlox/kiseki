# Phase 14 — Comprehensive Plan

**Status**: ACCEPTED — execution begins 2026-04-26.
**Date**: 2026-04-26
**Predecessor**: Phase 13f (`phase-13f-final-11.md`) — fast suite 167 → 181/181, all
11 plan items closed, integrator + auditor + audit-sweep follow-ups landed in
`378e8d3`.

## Decisions locked in (2026-04-26)

Architect/analyst sign-off captured before implementer work begins:

| Decision | Choice |
|---|---|
| **14a** TenantKmsProvider trait shape (5 async methods + cfg-gated `inject_failure`) | confirmed as proposed |
| **14d.1** ObjectBackupBackend location | `kiseki-server` |
| **14d.2** Backend dependency | minimal in-house trait (no `object_store` crate) |
| **14d.3** Snapshot format | single tarball per snapshot (not per-shard JSON files) |
| **14d.4** Restore semantics | snapshot-only (per ADR-016 — full reconstruction from one tarball) |
| **14d analyst** | implementer authors the 6 backup-and-restore Gherkin scenarios in-band; no separate seed |
| **14e** Raft key-store at-rest encryption | per-node identity → HKDF; **default = mTLS-derived** (uses the existing `KISEKI_CERT_PATH/KEY_PATH` already required for the data fabric — A-T-2/I-Auth1); SPIFFE-derived **when `KISEKI_SPIFFE_SOCKET` is set** (assumption A-T-2's "alternative"); file-based `$DATA_DIR/node-identity.key` fallback for dev/single-node (no mTLS configured); HKDF info string `"kiseki/at-rest/v1"` domain-separates derived bytes from any reuse of the source key. Test impl takes raw bytes — no SPIRE socket needed in BDD. |
| **14f** `KisekiNode` topology metadata | `Topology` enum (`Rack(String)` / `Zone{rack, zone}` / `Custom(HashMap<String,String>)`) |

## Why this plan exists

After Phase 13f the fast suite hit 181/181, but two follow-up reviews surfaced
deeper gaps the headline number was hiding:

- `specs/integration/phase-13f-integration-review.md` (integrator pass) — 4
  HIGH seam findings beyond the 11 plan items, all addressed in `378e8d3`.
- `specs/fidelity/phase-13f-audit.md` (auditor pass over Phase 13f's 14
  scenarios) — 3 HIGH constructor-axiom findings, all addressed in `378e8d3`.
- `specs/fidelity/bdd-depth-audit.md` (auditor sweep over the remaining 227
  scenarios) — **39 HIGH findings** still open, organised below.

The "181/181 fast pass" headline remains accurate but obscures that ~44 % of
@integration scenarios are SHALLOW or STUB by depth. Phase 14 closes the gap.

**Source-of-truth artefacts** (read these before working on a section):

- `specs/fidelity/SWEEP.md` — per-feature depth rollup, top-10 risks
- `specs/fidelity/bdd-depth-audit.md` — per-scenario depth + severity
- `specs/fidelity/INDEX.md` — refreshed checkpoint + 5 priority gaps
- `specs/architecture/adr/016-backup-and-dr.md` — backup/DR design
- `specs/architecture/adr/028-external-tenant-kms-providers.md` — KMS providers

## Methodology — red-write-green

Every step in this plan follows the same discipline. Two independent loops:

**BDD (integration tier)**

1. Run the target scenario — must FAIL with a real assertion failure
   (`todo!()`-induced panic counts as a STUB; rewrite first).
2. Wire production code through the integrated path (gateway → composition →
   log → view → backend).
3. Run again — GREEN.
4. Re-run the full fast suite — no regressions.

**TDD (unit tier)**

1. For new production types, write a unit test that exercises the spec
   invariant.
2. Run — RED.
3. Minimal implementation → GREEN.
4. Repeat for each branch / failure mode.

**Per-step gate** before committing each phase increment:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings` (the local
  `make rust-clippy` target now mirrors CI exactly — see commit `844f5aa`)
- `cargo test --workspace --exclude kiseki-acceptance --locked`
- `cargo test -p kiseki-acceptance --locked` — 181/181, no regressions
- For phases that touch slow scenarios:
  `cargo test -p kiseki-acceptance --features slow-tests --locked`

Commit messages follow Conventional Commits + cite the spec section being
enforced (e.g. `feat(kms): TenantKmsProvider trait + Vault impl (ADR-028 §3)`).

## Phases

| # | Phase | Risk closed | Scope size |
|---|---|---|---|
| 14a | TenantKmsProvider trait + production impls (ADR-028) | external-kms.feature: 14 SHALLOW → THOROUGH | medium |
| 14b | Persistence feature wiring | persistence.feature: 14 STUB → THOROUGH | medium |
| 14c | Constructor-axiom + audit-event sweep | 7 step files; 30+ Then steps | medium |
| 14d | Backup/restore via object-store backends (ADR-016) | New trait + S3 impl + new BDD feature | medium |
| 14e | Security hardening (Zeroizing, mlock) | 3 production-marker sites | small |
| 14f | Slow-suite Raft membership/snapshot/TLS | multi-node-raft.feature: 12 STUB + 41 from prior plan | large |

Each phase is self-contained — execute in numerical order; later phases assume
earlier ones landed.

---

## Phase 14a — TenantKmsProvider production trait (ADR-028)

**Findings closed** (auditor sweep cluster B): 16 HIGH in `external-kms.feature`.
Today every Then step constructs a local `TenantKek::new([byte; 32], …)` inside
`crates/kiseki-acceptance/tests/steps/kms.rs:25-35`, seals/unwraps inside the
test, and asserts the local roundtrip. **No `TenantKmsProvider` trait exists in
production code.** ADR-028 is non-falsifiable end-to-end.

### Step 1 — production trait in `kiseki-keymanager`

- New module `crates/kiseki-keymanager/src/tenant_provider.rs`.
- Trait `TenantKmsProvider: Send + Sync` with the operations ADR-028 specifies:
  - `wrap_dek(tenant_id, plaintext_dek) -> WrappedKey`
  - `unwrap_dek(tenant_id, wrapped) -> PlaintextDek` (returns `Zeroizing`)
  - `rotate_kek(tenant_id) -> KeyEpoch`
  - `circuit_state(tenant_id) -> CircuitState` for monitoring
  - `inject_failure(tenant_id, mode)` (test/diagnostic surface — ADR-028 §6)
- Typed errors via `thiserror`, mapped to `KisekiError::{Retriable, Permanent}`
  per the established pattern.

**TDD**: unit tests for the trait's contract using the in-memory impl
(below). Cover wrap/unwrap roundtrip, rotation epoch monotonicity, fault
injection produces retriable errors, circuit-breaker transitions.

### Step 2 — `InMemoryTenantKmsProvider` reference impl

- `crates/kiseki-keymanager/src/tenant_provider/in_memory.rs`.
- Backed by `HashMap<OrgId, Vec<TenantKek>>` (epoch history).
- `inject_failure` flips a per-tenant `Mutex<FailureMode>`.
- This replaces the test-only `TenantKek::new(…)` smell — production code now
  owns the type.

### Step 3 — provider stubs for ADR-028 §4 backends

The four feature-gated backends listed in ADR-028:

- `vault` — HashiCorp Vault Transit
- `aws` — AWS KMS
- `azure` — Azure Key Vault
- `gcp` — Google Cloud KMS

Each ships as a feature-gated module (`crates/kiseki-keymanager/src/tenant_provider/{vault,aws,azure,gcp}.rs`)
with the trait impl returning `KeyManagerError::ProviderNotConfigured` until
the credential plumbing is wired. **The point of Phase 14a is the trait + the
in-memory impl; the four cloud provider data paths are scaffolded but
not exercised at integration tier here** — that's a separate ticket per
ADR-028 §7.

**TDD**: each backend's module ships with one `#[cfg(feature = "kms-…")]` smoke
test that checks construction + the stubbed-out error path.

### Step 4 — re-wire `kms.rs` step definitions

Replace every site that constructs `TenantKek::new([byte; 32], …)` inside a
Then step with a call through `w.tenant_kms` (a new `Arc<dyn TenantKmsProvider>`
field on `KisekiWorld`, defaulting to `InMemoryTenantKmsProvider`).

For each of the 14 SHALLOW scenarios in `external-kms.feature`:

- **RED**: scenario currently passes vacuously (test creates the kek, seals,
  unwraps, asserts). Run with the new wiring **before** the production code is
  changed — assertion now sees no provider state and fails.
- **WRITE**: route the scenario's Given/When through the provider trait
  (`provider.wrap_dek(tenant, …)`, `provider.rotate_kek(…)`, etc.).
- **GREEN**: assertion passes against the live provider state.

### Step 5 — fault-injection scenarios use the trait

The "Gateway cannot reach tenant KMS" scenario already uses
`MemKeyStore::inject_unavailable` (system-key path). Add the per-tenant
analogue: `provider.inject_failure(tenant, Unreachable)` driving a real
retriable error through the gateway write path.

### Definition of done — Phase 14a

- [ ] `TenantKmsProvider` trait + `InMemoryTenantKmsProvider` impl land with
      unit-test coverage for every trait method's contract.
- [ ] Four cloud-provider stubs compile under their respective features.
- [ ] All 14 SHALLOW scenarios in `external-kms.feature` rated THOROUGH in a
      refreshed `bdd-depth-audit.md`.
- [ ] No remaining `TenantKek::new(…)` constructions in `kms.rs`.
- [ ] Fast suite still 181/181; full keymanager unit suite still green.
- [ ] Commit summary: `feat(kms): TenantKmsProvider trait + ADR-028 §3 enforcement`.

---

## Phase 14b — Persistence feature wiring

**Findings closed** (auditor sweep cluster A): 14 HIGH in `persistence.feature`.
All 14 `@slow @integration` scenarios are STUB (`todo!()` bodies in
`crates/kiseki-acceptance/tests/steps/log.rs:752-1228`). Hidden by the default
`@slow` filter at `crates/kiseki-acceptance/tests/acceptance.rs:773-775`.

The relevant production crates already exist:

- `kiseki-log::PersistentShardStore` (wraps MemShardStore + redb)
- `kiseki-raft::RedbRaftLogStore` (Raft log persistence)
- `kiseki-keymanager` persistent epoch storage

The BDD just never calls them. Phase 14b wires the calls.

### Step 1 — `PersistentShardStore` available to BDD

- Add `pub persistent_shard_store: Arc<PersistentShardStore>` to `KisekiWorld`,
  initialized in `KisekiWorld::new()` against a per-scenario `tempfile::tempdir()`
  redb path.
- Tear-down: `KisekiWorld::drop` already exists (commit `378e8d3`); extend it
  to flush + close the redb db cleanly.

### Step 2 — wire each STUB scenario through the persistent store

For every scenario in `persistence.feature`, the loop is:

1. **Inventory**: read the scenario's intent (durable write, restart, recover).
2. **RED**: replace `todo!()` with a call that `cargo test` will run; the
   assertion must fail because nothing is wired yet.
3. **WRITE**: route the Given/When through `w.persistent_shard_store`. For
   "restart" steps, drop the existing instance and re-open from the same
   tempdir — proves the redb commit landed.
4. **GREEN**: assertion sees the recovered state.

The 14 scenarios fall into these clusters (group commits per cluster):

- Append + restart roundtrip (3 scenarios)
- GC boundary survives restart (2 scenarios)
- Inline payloads survive restart (2 scenarios)
- Audit log persistence (2 scenarios)
- Key epoch persistence (2 scenarios)
- Raft log persistence (3 scenarios)

### Step 3 — promote `persistence.feature` out of the silent `@slow` skip

Currently, `acceptance.rs:773-775` filters all `@slow` scenarios from the
default `cargo test -p kiseki-acceptance` invocation. After Phase 14b the
persistence scenarios MUST run in the fast suite — they're disk-backed but
fast (single-digit-second). Concretely:

- Either remove `@slow` from `persistence.feature` (preferred — these
  scenarios are not Raft-cluster-slow), or
- Add a tag like `@persistence` and special-case it through the filter.

### Definition of done — Phase 14b

- [ ] All 14 persistence scenarios rated THOROUGH (real redb roundtrip,
      assertions falsifiable).
- [ ] Fast suite count grows from 181 → ~195 (the 14 promoted scenarios run
      by default).
- [ ] No remaining `todo!()` in any persistence step body.
- [ ] Commit summary: `feat(persistence): wire 14 BDD scenarios through real redb`.

---

## Phase 14c — Constructor-axiom + audit-event sweep

**Findings closed** (auditor sweep cross-cutting): the constructor-axiom
anti-pattern recurs in 7 step files beyond the 4 already fixed in Phase 13f.
And 30+ Then steps assert against an `AuditEvent` literal they constructed
themselves.

### Step 1 — constructor-axiom rewrites

For each of the 7 sites (file:line cited in `bdd-depth-audit.md`), apply the
Tier-2 template (subscribe-second-instance, compare to first):

| File | Smell | Fix template |
|---|---|---|
| `kms.rs` (14 sites) | local `TenantKek::new(…)` | swap to `w.tenant_kms` from Phase 14a |
| `view.rs:528` | fresh `VersionStore` | use the World's `view_store` |
| `admin.rs:1069` | fresh `CompactionProgress` | use the live admin service's progress |
| `operational.rs:171, 1700` | fresh `KeyCache::new(0)` | use the World's `key_store` cache |
| `client.rs` transport flags | local atomics | use the gateway's metrics |
| `small_file.rs:175-220` | round-trip on `w.sf_*` fields | drive through `mem_shard_store` |

Each site becomes a "RED → WRITE → GREEN" three-step within the existing
scenario.

### Step 2 — audit-event "I am the producer" rewires (30+ Then steps)

The pattern: test constructs an `AuditEvent { … }` literal, appends it to
`w.audit_log`, then asserts it's there. Both the production and the assertion
are in the test, so the test would pass even if the system-under-test never
emitted a single audit event.

Fix: extend the relevant production code paths (key rotation, key destruction,
data write, advisory hint, drain transition, etc.) to actually emit through
`AuditOps::append`, and rewrite the Then step to consume from the live log
without first appending.

The 12 `// TODO: wire audit infrastructure` markers in `crypto.rs` are the
biggest single batch — they correspond to key-lifecycle audit events. Wire
them in `kiseki-keymanager`'s rotation / destruction / access paths first.

### Definition of done — Phase 14c

- [ ] Zero `TenantKek::new(…)` / `VersionStore::new(…)` / `KeyCache::new(0)` /
      `BudgetEnforcer::new(…)` / `LockManager::default()` / `CompactionProgress::default()`
      constructions inside Then-step bodies (a `grep` audit will catch
      regressions).
- [ ] Zero Then steps that construct an `AuditEvent` literal and append it
      themselves.
- [ ] 12 `// TODO: wire audit infrastructure` markers in `crypto.rs` retired.
- [ ] Refreshed `bdd-depth-audit.md` shows the depth distribution shifted at
      least 30 scenarios from SHALLOW → MOCK or THOROUGH.
- [ ] Commit summary: `chore(bdd): close 30+ constructor-axiom assertions`.

---

## Phase 14d — Backup/restore via object-store backends (ADR-016)

**Findings closed**: ADR-016 spec exists; `crates/kiseki-server/src/backup.rs`
exists; the implementation is filesystem-only (`backup_dir: PathBuf`) and
marked `#![allow(dead_code)] // Module not yet wired into the running server`.
No BDD coverage at all.

### Step 1 — `ObjectBackupBackend` trait in `kiseki-server`

- New trait, two methods: `put_blob(key, bytes) -> io::Result<()>` and
  `list_keys(prefix) -> io::Result<Vec<String>>`, plus
  `get_blob(key) -> io::Result<Option<Vec<u8>>>` and `delete_blob(key)`.
- `FileSystemBackupBackend` — moves the existing `std::fs` writes behind the
  trait. Existing tests stay green.
- `S3BackupBackend` — uses `object_store` crate (arrow-rs ecosystem) so the
  workspace doesn't take on a heavy AWS-SDK dependency. Construction takes
  endpoint + bucket + credentials.

**TDD**: each backend gets a unit test using `tempfile::tempdir()` (FS) or a
`mockall`-generated mock (S3) covering put/list/get/delete + delete-nonexistent
+ list-empty-prefix.

### Step 2 — wire `BackupManager` into `kiseki-server` runtime

- Drop the `#![allow(dead_code)]` attribute.
- Add `BackupManager` construction to `kiseki-server::runtime::Runtime`.
- Periodic backup task (configurable cadence, default off).

### Step 3 — admin gRPC surface

Add three RPCs to the `Admin` service in `specs/architecture/proto/`:

- `CreateSnapshot(request)` — returns a `BackupSnapshot`.
- `RestoreSnapshot(snapshot_id)` — returns `RestoreReport`.
- `ListSnapshots()` — returns `Vec<BackupSnapshot>`.

Wire them through `kiseki-control` (admin role check) → `kiseki-server`
(backup manager). Existing `StorageAdminService::require_admin` pattern.

### Step 4 — new `backup-and-restore.feature`

Gherkin scenarios anchored to ADR-016 §"Recovery scenarios":

- `Snapshot to local filesystem succeeds and is restorable`
- `Snapshot to S3-compatible store succeeds and is restorable`
- `Restore reads encrypted ciphertext only — no plaintext leaks`
- `Concurrent backup is rejected`
- `Cleanup deletes snapshots older than retention_days`
- `Snapshot includes log + chunk-store ciphertext + control-plane state`
  (verifies ADR-016's "what is replicated" table)

Each scenario follows red-write-green at the BDD tier, with an in-memory
`MockS3Backend` for the S3 cases (no real AWS in CI).

### Definition of done — Phase 14d

- [ ] `ObjectBackupBackend` trait + 2 impls live in `kiseki-server`.
- [ ] `BackupManager` is no longer dead code; runtime constructs it.
- [ ] 3 admin RPCs wired through the trait.
- [ ] New `backup-and-restore.feature` with ≥ 6 THOROUGH scenarios.
- [ ] Existing 181 fast scenarios + new 6 = 187+ green.
- [ ] Commit summary: `feat(backup): ObjectBackupBackend trait + S3 impl + ADR-016 BDD`.

---

## Phase 14e — Security hardening (production markers)

**Findings closed**: three security-relevant `// for production` markers
discovered during the recent grep:

- `crates/kiseki-keymanager/src/cache.rs:16` — *"Raw key material (would be
  Zeroizing in production)"* — caches plaintext key bytes that survive a Drop.
- `crates/kiseki-keymanager/src/raft_store.rs:31` — *"in production: encrypted
  with node-local key"* — keys are at rest in the Raft log unencrypted.
- `crates/kiseki-server/src/integrity.rs:4` — *"prevent memory extraction of
  key material in production"* — `mlock` is documented but not enabled.

### Step 1 — `Zeroizing<T>` on cached plaintext key material

- Wrap the cached key bytes in `cache.rs` in `zeroize::Zeroizing<[u8; 32]>`
  so they're zeroed on Drop.
- TDD: add a unit test that fetches a key, drops the cache, and asserts the
  underlying memory has been zeroed (use a `#[allow(unsafe_code)]` raw-pointer
  read with a `// SAFETY:` comment, gated to a debug-only test).

### Step 2 — encrypt-at-rest for the Raft key store

- `raft_store.rs` currently serialises plaintext key material. Wrap in an
  AES-GCM envelope using a node-local key at construction time.
- Node-local key sourcing (per the locked-in 14e decision row above):
  introduce a small `NodeIdentitySource` trait with selection precedence
  1. `SpiffeIdentitySource` — when `KISEKI_SPIFFE_SOCKET` is set
  2. **`MtlsIdentitySource` — default**, derives from the node's existing
     mTLS private key (the cert/key already loaded for the data fabric)
  3. `FileIdentitySource` — `$DATA_DIR/node-identity.key` (mode 0600,
     auto-generated on first boot) when neither SPIFFE nor mTLS is set
  4. `TestIdentitySource(Vec<u8>)` — raw-bytes impl for unit/BDD tests
  All four feed `HKDF-SHA256(secret, salt=node_id, info="kiseki/at-rest/v1")`
  so derived bytes are domain-separated from any reuse of the source.
- No legacy migration: Phase 14e ships before any production deployment,
  so no real data exists in the old plaintext format. If a developer
  reopens a pre-14e redb, deserialization fails with `InvalidData` and
  they wipe their data dir — far cleaner than carrying a one-shot
  migration code path forever.
- TDD: roundtrip test (encrypt → persist → reload → decrypt), reject test
  (wrong node identity → AuthenticationFailed), precedence test
  (SPIFFE > mTLS > file, already done in step 2a).

### Step 3 — `mlock` the integrity checker's key pages

- `integrity.rs` already mentions mlock; finish the wiring by calling
  `aws_lc_rs::mlock_secure_pages()` (or the `region` crate) at construction.
- Unit test: probe `/proc/self/status` (Linux) for `VmLck` after construction.

### Definition of done — Phase 14e

- [ ] Zero `// would be Zeroizing in production` / `// in production:` security
      markers in `kiseki-keymanager` and `kiseki-server`.
- [ ] All three production hardening steps land with unit-test coverage.
- [ ] Commit summary: `security: enable Zeroizing + at-rest encryption + mlock`.

---

## Phase 14f — Slow-suite Raft membership/snapshot/TLS (41 scenarios)

**Findings closed**: 12 HIGH STUB scenarios in `multi-node-raft.feature`
(`raft.rs:323, 442, 461, 504, 528, 1036-1085, 1142-1158, 1233-1338`), plus
the remaining 29 from `phase-13f-final-11.md` slow-suite section. Total: **41
scenarios**.

The test infrastructure already partially exists:

- `RaftTestCluster::add_learner` + `change_membership` (Phase 13f, commit
  `64f7380`).
- `RaftMembershipAdapter` trait + impl (Phase 14 prior commit `378e8d3`).
- `MemLogStore` + redb-backed log persistence in `kiseki-raft`.

What's missing: snapshot transfer, persistent log in test cluster, TLS hooks,
rack/topology metadata, perf instrumentation.

### Step 1 — snapshot transfer in `TestNetwork::full_snapshot` (5 scenarios)

Currently returns `Err(Unreachable("snapshot not implemented"))` at
`crates/kiseki-log/src/raft/test_cluster.rs:148-164`. Implement using
openraft's snapshot transfer protocol.

### Step 2 — persistent log in `RaftTestCluster` (3 scenarios)

Wire `RedbRaftLogStore` as an alternative to `MemLogStore` in the test
cluster. Add `RaftTestCluster::with_persistent_log(path)` constructor.

### Step 3 — TLS transport inspection hooks (3 scenarios)

Add a `MessageTap` trait that the test cluster's transport calls on every
RPC; tests subscribe to verify TLS framing without parsing actual TLS records.

### Step 4 — rack-aware placement metadata (3 scenarios)

Extend `KisekiNode` with a `rack: Option<String>` field; placement scenarios
assert the per-rack distribution.

### Step 5 — drain orchestration scenarios (8)

Phase 14a–14c laid the groundwork (`DrainOrchestrator` + `RaftMembershipAdapter`).
Wire the 8 slow-suite drain scenarios through `execute_drain` against a
larger `RaftTestCluster` (5+ nodes, multiple shards).

### Step 6 — performance measurement (2 scenarios)

Add latency/throughput instrumentation to `RaftTestCluster` (per-write
duration, bytes/sec) and assert against a coarse threshold.

### Step 7 — remaining slow scenarios (17)

Cover the rest from the prior plan's slow-suite section:

- Concurrent elections (multi-shard cluster, 30 shards)
- Follower reads
- Partition minority asymmetric
- Network partition resilience
- Node recovery (persistent log + network recovery)
- Learner support
- SSD migration

Each is a red-write-green scenario against the existing infrastructure plus
the additions from steps 1–6.

### Step 8 — promote slow tests to release CI (already done)

`release.yml` already runs `cargo test --features slow-tests` per Phase 13f
(commit `305cdaf`); nothing to do here, just verify after Phase 14f lands
that the release workflow passes 60/60.

### Definition of done — Phase 14f

- [ ] All 41 slow-suite scenarios rated MOCK or THOROUGH (no STUB, no
      SHALLOW).
- [ ] `cargo test -p kiseki-acceptance --features slow-tests` reports 60/60
      passed (was 19/60).
- [ ] Release workflow CI green on the slow-tests step.
- [ ] Commit summary: `feat(raft): close 41 slow-suite scenarios`.

---

## Definition of "Phase 14 complete"

All of:

- [ ] Refreshed `bdd-depth-audit.md` shows: zero STUB, zero SHALLOW. Every
      @integration scenario rated MOCK or THOROUGH.
- [ ] Fast suite still ≥ 181/181 (likely 195+ after Phase 14b promotion).
- [ ] Slow suite 60/60.
- [ ] ADR-016 + ADR-028 enforced by falsifiable BDD.
- [ ] Three security hardening sites closed.
- [ ] Backup/restore works against a real (or mocked) S3 endpoint.
- [ ] No `// would be …` / `// in production:` security markers anywhere in
      `crates/`.
- [ ] CI green on every commit; release workflow green.

After Phase 14, the "181/181 fast pass" headline finally means what the
@integration tag promises: every scenario exercises the real integrated path
through real backends with assertions that fail when the system-under-test
misbehaves.
