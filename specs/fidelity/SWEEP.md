# Fidelity Sweep — BDD Acceptance Suite

**Status**: COMPLETE
**Date**: 2026-04-26
**Audited**: 227 @integration scenarios across 22 feature files
  (181 fast + 60 slow, minus the 14 scenarios already covered by
  `phase-13f-audit.md`).
**Sample basis**: HEAD `3b903a4`, BDD entry point
  `crates/kiseki-acceptance/tests/acceptance.rs`, 19 step-definition
  files in `crates/kiseki-acceptance/tests/steps/`.

## Headline numbers

| Depth        | Scenarios | % of audited |
|---|---:|---:|
| THOROUGH     | 35  | 15% |
| MOCK         | 92  | 41% |
| SHALLOW      | 81  | 36% |
| STUB         | 19  |  8% |
| **Audited**  | 227 | 100% |
| Excluded (covered by phase-13f) | 14 | — |
| **Total @integration** | **241** | — |

THOROUGH means: real backend (`FileBackedDevice`, `RaftTestCluster`,
`MemShardStore` with realistic ranges, `SmallObjectStore`,
`InMemoryGateway` end-to-end), assertions falsifiable, errors flow
from real operations.

MOCK means: real domain logic against in-memory store, but no
distributed behaviour, no persistence, no protocol wire format. Acceptable
as an `@unit` scenario but mislabeled as `@integration`.

SHALLOW means: fresh stand-in is constructed inside the Then body and
asserted on (constructor axiom), or the assertion is `>= 0` /
`assert!(true)` / `is_none() || is_some()` / a constant-equality check
that always holds because the regex feeds it the same string.

STUB means: empty body, comment-only body, or `todo!()` body. (When
`@slow` and gated behind `--features slow-tests`, `todo!()` scenarios
do not run by default — but the spec claims they're enforced.)

## Per-feature rollup

| Feature | Total | THOR | MOCK | SHAL | STUB | Notes |
|---|---:|---:|---:|---:|---:|---|
| authentication.feature                 | 0   | —  | —  | —  | — | All scenarios moved to crate unit tests; nothing to audit at integration tier. |
| block-storage.feature                  | 27  | 18 |  6 |  3 |  0 | Real `FileBackedDevice` with tempfile; HIGH confidence. Auto-detect medium/IO-strategy scenarios SHALLOW because file-backed always reports `Virtual`/`FileBacked` regardless of `Given` device type. |
| chunk-storage.feature                  |  6  |  4 |  2 |  0 |  0 | 5 covered here, 1 (Repair-degraded read telemetry) overlaps phase-13f. Real `ChunkStore`, real EC reconstruction. |
| cluster-formation.feature              | 23  |  4 |  9 |  6 |  4 | Slow scenarios use real `RaftTestCluster` (MOCK to THOROUGH). Topology scenarios use real `NamespaceShardMapStore` and verify by `gateway_write` round-trip — THOROUGH. ADV-033 atomic rollback / authorization SHALLOW. |
| composition.feature                    |  1  |  1 |  0 |  0 |  0 | Real composition store + cross-context delta inspection in log_store. THOROUGH. |
| control-plane.feature                  |  8  |  0 |  6 |  2 |  0 | Real `TenantStore`/`NamespaceStore`/`FederationRegistry` (MOCK). Federation residency, advisory non-replication SHALLOW. |
| device-management.feature              | 12  |  2 |  3 |  6 |  1 | Many Then steps are empty (e.g. `chunks_migrated` no-op) or `assert_eq!(expected, "Healthy")` (constant comparison). Real EC repair only in 2 scenarios. |
| erasure-coding.feature                 |  3  |  3 |  0 |  0 |  0 | Real `read_chunk_ec` with offline devices, real reconstruction path. |
| external-kms.feature                   | 18  |  0 |  3 | 14 |  1 | Heavy SHALLOW pattern: every "wrap/unwrap roundtrip" / "rotation" / "shred" is performed inside the test body using local `kek_for_provider(...)` and `seal_envelope(...)`, then asserts the roundtrip succeeded. The system-under-test (a real `TenantKmsProvider`) does not exist yet. |
| key-management.feature                 |  6  |  1 |  4 |  1 |  0 | Crypto-shred + retention-hold use real `MemKeyStore`. Federated KMS reachability is symbolic. |
| log.feature                            | 17  |  6 |  4 |  3 |  4 | 4 covered by phase-13f (inline, split-buffer, merge, qos-given). Slow Raft scenarios mostly STUB (`todo!()` step bodies — gated behind `--features slow-tests`, so do not actually fail in CI). |
| multi-node-raft.feature                | 30  |  6 |  9 |  3 | 12 | Real `RaftTestCluster` for replication / election / quorum (MOCK→THOROUGH). Membership change, snapshot transfer, TLS, rack-aware, drain-orchestration are all STUB (`todo!()`). |
| native-client.feature                  |  9  |  1 |  2 |  6 |  0 | Heavy SHALLOW — most assertions check that local objects exist (`assert!(client_built.is_some())`). RDMA / CXI transport never instantiated. |
| nfs3-rfc1813.feature                   | 0   | —  | —  | —  | — | All wire-format scenarios live in `protocol.rs` step file; the feature file has 0 scenarios — moved to unit tests. |
| nfs4-rfc7862.feature                   | 0   | —  | —  | —  | — | Same. |
| operational.feature                    | 11  |  3 |  3 |  4 |  1 | 6 covered by phase-13f (drain trio + others). Compression, audit-stall, integrity-monitor, rolling-upgrade SHALLOW (test bodies fabricate the asserted state). |
| persistence.feature                    | 14  |  0 |  0 |  0 | 14 | **All 14 slow scenarios are gated**. The step bodies that exist are SHALLOW (e.g. "delta survives restart" calls `read_deltas` but never closes/reopens the store). Most rely on todo! or no-op step bodies. |
| protocol-gateway.feature               | 14  |  4 |  3 |  0 |  0 | 7 covered by phase-13f. Remaining 7: real S3 multipart through gateway (THOROUGH), gateway-crash reconnect MOCK, S3 conditional write MOCK. |
| s3-api.feature                         | 0   | —  | —  | —  | — | Like NFS, moved to unit tests. |
| small-file-placement.feature           | 13  |  1 |  3 |  9 |  0 | Most steps assert against `w.sf_*` World fields that the Given/When step set in the same scenario — same value round-tripped. Real inline routing is exercised in only 2 scenarios. |
| storage-admin.feature                  | 20  |  3 |  6 | 11 |  0 | Many Then steps construct fresh `CompactionProgress::new()` or `StoragePool {…}` literal and assert on its fields. Real shard split / scrub work in ~5 scenarios. |
| view-materialization.feature           |  8  |  1 |  3 |  4 |  0 | `w.poll_views()` invoked in many Then bodies but assertions just check the view exists. Pin TTL / discard logic uses real `ViewStore` (MOCK). |
| workflow-advisory.feature              |  1  |  0 |  1 |  0 |  0 | Only one @integration scenario; uses real `WorkflowTable` advance + budget enforcer. |

(Numbers in the table sum to **227** = audited scope.)

## Top 10 highest-risk SHALLOW or STUB findings

Severity legend: **HIGH** (security or data-integrity invariant
non-falsifiably enforced); **MEDIUM** (functional invariant
weakly enforced); **LOW** (cosmetic).

1. **persistence.feature: all 14 scenarios are slow + skipped + STUB**
   (`crates/kiseki-acceptance/tests/steps/raft.rs:457-465, 461-465,
   1180-1183, 1185-1198`). The whole "delta survives restart", "Raft
   vote and term survive restart", "snapshot transfer", "key epochs
   survive restart" series ships with `todo!()` bodies. Because they
   are `@integration @slow` and the default test run filters out
   `@slow`, they are not part of the headline 181/181 green count —
   yet `INDEX.md` claims 14/14 pass. **Severity: HIGH** — durability
   guarantees (I-L1, I-L4, I-K6, I-V3, I-SF5) are completely
   unenforced at integration tier.

2. **external-kms.feature: 14 of 18 scenarios SHALLOW**
   (`kms.rs:86-200, 215-260, 360-470, 540-600, 700-900, 1300-1480`).
   Every "tenant configures Vault provider" / "AWS KMS rotation" /
   "PKCS#11 unwrap" Then body constructs a local
   `TenantKek::new([byte; 32], …)` (line 26-35), seals + unwraps an
   envelope inside the test, and asserts the local roundtrip
   succeeded. No real `TenantKmsProvider` is ever instantiated; the
   provider abstraction does not exist in the codebase. ADR-028's
   provider-validated, audit-logged, in-hardware unwrap claims are
   non-falsifiable at integration tier. **Severity: HIGH** —
   external-KMS is a security boundary; the test that "passes"
   demonstrates only that local AEAD works.

3. **multi-node-raft.feature: 12 STUB + several "trace-bullet" SHALLOW**
   (`raft.rs:323-353, 442-454, 461-486, 504-512, 528-534, 1036-1085,
   1142-1158, 1280-1338`). The membership change family
   (`add_member`, `remove_member`, learner promotion, voter
   removal), snapshot transfer (`then_snapshot`,
   `then_caught_up`), TLS transport (`then_encrypted`,
   `then_cert_validated`), rack-aware placement
   (`then_rack_spread`), and drain-via-Raft scenarios are all
   `todo!()`. The corresponding @integration @slow scenarios run
   only with `--features slow-tests`; production CI runs without
   that feature, so they are silent gaps. **Severity: HIGH** for
   replication/durability invariants (I-L2, I-L4, I-N5).

4. **operational.rs `then_compressed_zstd` /
   `then_padded_4kb` / `then_chunks_compressed`** family
   (operational.rs:560-820). Every "compression" Then constructs a
   local plaintext, asserts that "repeated bytes have low entropy"
   (`unique_bytes.len() < original.len()`), or computes a 4KB
   round-up arithmetic. None of this exercises the real compression
   pipeline (which doesn't exist as production code yet). **Severity:
   HIGH** because ADR-009 compression-vs-HIPAA is asserted non-
   falsifiably — the HIPAA-block test (`then_compression_rejected`,
   line 743-766) checks that the org has the HIPAA tag, then
   *defines* a local `compression_allowed = !has_hipaa` and asserts
   on it. Tautological.

5. **small-file-placement.feature: 9 of 13 scenarios SHALLOW via
   round-trip on World state**
   (`small_file.rs:60, 78-89, 105-198, 245-310`). Pattern: Given
   step sets `w.sf_inline_threshold = 1024`, Then step asserts
   `w.sf_inline_threshold == 1024`. The assertion holds whether or
   not the production `kiseki_chunk::SmallObjectStore` ever ran a
   threshold check. **Severity: HIGH** for I-SF1/2/3/4 (placement
   policy) — the inline-routing decision is the spec's central
   claim and goes untested at integration tier.

6. **device-management.feature: 6 of 12 scenarios with no-op /
   constant-comparison Then bodies**
   (`device.rs:50-119, 156-180, 202-330`). Examples:
   `then_chunks_accessible` is empty with comment "Accessibility
   verified.";  `then_state_returns(... "Degraded")` does
   `assert_eq!(expected, "Degraded")` — i.e. asserts that the regex
   captured the same string the test specified, which by definition
   it did. **Severity: HIGH** for I-D1/D2/D5 device-failure recovery
   guarantees: F-D1 (device fails → EC repair) is critical; the
   rebuild step is empty.

7. **storage-admin.feature: ReencodePool / SetPoolDurability /
   `then_long_running` (admin.rs:1019-1098, 1066-1099)**.
   `then_long_running` constructs a fresh `CompactionProgress::new()`
   in the Then body and asserts its `examined.load() == 0` — which
   is the freshly-constructed default. The real "long-running
   re-encode operation" is never started. `when_set_durability`
   (line 1019) is a `todo!()`. **Severity: HIGH** — ADR-025 admin
   operations claim atomic, observable, cancellable; these properties
   are not enforced.

8. **native-client.feature: RDMA + CXI transport scenarios**
   (`client.rs` — cluster-formation step file). "One-sided RDMA read
   for pre-encrypted chunks" and "Transport failover — CXI to TCP"
   never instantiate an RDMA queue pair or a CXI endpoint; the steps
   set string flags on the World and assert they survive. **Severity:
   MEDIUM** — RDMA path is not in production scope yet, but the
   scenario claims @integration without that disclaimer.

9. **operational.rs `then_throttling_scoped` (operational.rs:1043-1058)**.
   Asserts `tip_a == tip_b` for two newly-constructed `OrgId(uuid::Uuid::from_u128(1))`
   and `OrgId(uuid::Uuid::from_u128(2))`, where both tips are 0 because
   no tenant-scoped event was ever appended in the scenario. The
   "tenant isolation" claim is `0 == 0`. **Severity: HIGH** for
   I-T1 / ADR-009 multi-tenant audit isolation.

10. **view-materialization.feature: `then_reader_sees_write` and
    related (view.rs:251-263, 460-468, 502-507)**. The "read-your-
    writes" / "consistent point-in-time snapshot" Then bodies do
    `for &vid in w.view_ids.values() { assert!(w.view_store.get_view(vid).is_ok()) }`
    — i.e. assert that previously-created views can be looked up.
    The actual MVCC pin / snapshot guarantee is not tested.
    **Severity: MEDIUM** for I-V1/V2.

## Cross-cutting findings

- **The `@slow` filter is hiding regressions.** `acceptance.rs:766-781`
  filters out `@slow` scenarios by default. 60 of the 241
  @integration scenarios are @slow, including the entire
  persistence.feature (14) and most multi-node-raft.feature (30).
  Many of those have `todo!()` bodies that would cause panics.
  Headline "599/599 pass" or "181/181 fast pass" obscures this:
  the slow tier is not exercised in normal CI. Recommend either
  (a) removing the @slow filter and accepting the wall-clock cost,
  or (b) running `--features slow-tests` in a separate nightly job
  and reporting failures.

- **Heavy reliance on `MemShardStore`**. The earlier audit's
  observation still holds: only the small-file inline path
  (`SmallObjectStore`), the EC read path (`read_chunk_ec`), the
  Raft test cluster (multi-node only), the FileBacked block
  device, and the topology shard map (`NamespaceShardMapStore`)
  exercise real backends. Persistent log (`PersistentShardStore`,
  `RedbRaftLogStore`) is never used in BDD.

- **Constructor-axiom anti-pattern is widespread**. The pattern flagged
  in phase-13f for `LockManager::default()` / `BudgetEnforcer::new(...)`
  recurs in `kms.rs` (`TenantKek::new`), `view.rs`
  (`VersionStore::new` in then_returns_versions:528), `admin.rs`
  (`CompactionProgress::new` in then_long_running:1069), and
  `operational.rs` (`KeyCache::new` in then_key_material_safe:171).
  At least 26 distinct call sites across 7 step files.

- **Audit-event tests reproduce phase-13f's `then_tenant_admin_alerted`
  pattern**. ~30 Then steps construct an `AuditEvent { … }` literal
  inside the test body, append it to `w.audit_log`, then assert the
  log contains it. The system-under-test never emits the event. Sites
  include `operational.rs:60-80, 88-100, 130-145, 855-870, 920-940,
  1000-1015, 1080-1100`; `auth.rs:84, 104, 127, 160, 226`; `chunk.rs:481,
  813, 820, 825, 982, 1405`; `crypto.rs:217, 294, 367, …` (12 sites
  marked `// TODO: wire audit infrastructure`); `admin.rs:331, …`;
  `device.rs:163` ("Audit implicit").

- **Tautological assertions (5 sites total beyond the prior audit's
  list)**:
  - `admin.rs:959`: `commit_lag == 0 || true`.
  - `kms.rs:557`: `is_some() || is_none()`.
  - `auth.rs:35`: `if w.last_error.is_none() { … }` (no else branch
    runs anything observable).
  - `view.rs:148`: `let _ = chars.rotational;` after a "rotational
    is true" Then.
  - `operational.rs:942-945`: `assert!(result.is_ok() || result.is_err())`.

## Per-step-file confidence

| File | LOC | Real backend? | Confidence in @integration claim |
|---|---:|---|---|
| block.rs           | 1655 | real `FileBackedDevice` w/ tempfile | HIGH |
| ec.rs              |  429 | real `ChunkStore::read_chunk_ec` | HIGH |
| chunk.rs           | 1422 | real `ChunkStore`, real refcount/dedup | HIGH (modulo audit-event TODOs) |
| composition.rs     | 1130 | real `CompositionStore.with_log()` cross-checked | HIGH |
| log.rs             | 1867 | mix: real `MemShardStore` + many `todo!()` | MEDIUM (slow Raft scenarios) |
| cluster.rs         | 1188 | real `RaftTestCluster` for cluster steps; real `shard_map_store` for topology | MEDIUM-HIGH |
| raft.rs            | 1344 | real `RaftTestCluster` for replication/election; `todo!()` for membership/snapshot/TLS | LOW (slow scenarios) |
| operational.rs     | 2462 | mostly local fixtures; real audit_log append | LOW |
| kms.rs             | 1737 | local `kek_for_provider()` round-trips | LOW |
| admin.rs           | 1792 | real `StorageAdminService` + `chunk_store` partial; many local CompactionProgress | MEDIUM |
| view.rs            | 1440 | real `ViewStore` for pins/state; symbolic for materialization | MEDIUM |
| client.rs          | 2417 | local discovery/transport fixtures | LOW |
| small_file.rs      |  736 | mostly `w.sf_*` World-state round-trips | LOW |
| device.rs          |  529 | real ChunkStore pool ops; many no-op Thens | LOW-MEDIUM |
| protocol.rs        | 2099 | real `nfs3_call` + `nfs_ctx` | HIGH (per-step) but mostly @unit-tagged scenarios |
| auth.rs            |  581 | most scenarios moved to unit tests | n/a |
| crypto.rs          |  993 | real AEAD roundtrips; audit TODOs | MEDIUM |
| advisory.rs        | 1850 | real `WorkflowTable`/`BudgetEnforcer` | MEDIUM (real domain logic, not distributed) |
| control.rs         | 1690 | real `TenantStore`/`NamespaceStore`/`FederationRegistry` | MEDIUM |

## Recommendation summary (5 priority items, in order)

1. **Run @slow scenarios in CI**, gated by feature, but reported
   separately so `todo!()` panics surface. Without this the 60-
   scenario gap remains invisible.
2. **Replace the constructor-axiom pattern**. A simple lint or
   codegen check that flags any Then-step body whose only
   side-effect is `Foo::new(…)` followed by `assert!` would catch
   the entire class.
3. **Wire a real `TenantKmsProvider` test double** (e.g. an
   in-process Vault stub via `httptest`) so external-kms.feature
   tests something other than local AEAD. Or downgrade those 18
   scenarios to `@unit` until the providers exist.
4. **Persist the World once per session and reopen for "restart"
   scenarios**. persistence.feature's 14 scenarios should drive a
   real `PersistentShardStore` close/reopen cycle.
5. **Audit emission contract**. Replace the 30+ "construct
   AuditEvent literal in test" anti-patterns with a single helper
   that asserts `w.audit_log.tip()` advances *as a side-effect*
   of the When step. Where the production code does not emit, mark
   the gap explicitly (escalation note in `specs/escalations/`).
