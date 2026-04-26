# Fidelity Index — Kiseki

**Checkpoint**: 2026-04-26 (post-Phase 13f + full BDD depth sweep)
**Previous**: 2026-04-24 (post Phase 12 ADR-032 async GatewayOps)

This index reflects today's reality of the BDD acceptance suite. The
prior checkpoint counted scenarios as "passing" without rating
test depth. The 2026-04-26 sweep
(`specs/fidelity/SWEEP.md` + `bdd-depth-audit.md`) adds the depth
classification that was missing.

---

## Per-crate status (production code)

Confidence reflects: (a) unit + integration test coverage, (b) BDD
depth at the @integration tier, (c) presence of real backend exercises.

| Crate | Status | Unit+Integ | BDD coverage | Confidence | Notes |
|---|---|---:|---|---|---|
| kiseki-common         | DONE   | 18 | n/a (types only) | HIGH | HLC, ids, InlineStore trait, ChunkId/KeyEpoch serde |
| kiseki-crypto         | DONE   | 32 | THOROUGH (in scope) | HIGH | AEAD, HKDF, envelope, shred, mlock — covered by real round-trips |
| kiseki-proto          | DONE   |  6 | n/a | HIGH | Protobuf round-trip tests |
| kiseki-raft           | DONE   | 16 | MOCK→THOROUGH (slow only) | MEDIUM | TCP transport + RedbRaftLogStore exercised in unit tests; @slow BDD scenarios use RaftTestCluster but membership-change/snapshot transfer steps are `todo!()` |
| kiseki-transport      | DONE   | 21 | n/a (TLS in BDD is symbolic) | HIGH | Real X.509 + SPIFFE SAN + CRL revocation tests |
| kiseki-keymanager     | DONE   | 35 | MOCK | HIGH | Epochs, rotation, persistent Raft log unit-tested. BDD scenarios use MemKeyStore; external-KMS feature is SHALLOW |
| kiseki-log            | DONE   | 50 | THOROUGH (split/merge/inline), STUB (slow Raft) | MEDIUM | Real auto-split, merge, inline offload, throughput guard, compaction. Slow Raft scenarios mostly `todo!()` |
| kiseki-audit          | DONE   | 19 | SHALLOW (audit-event tests fabricate the event) | LOW-MEDIUM | Real append-only + Raft store; ~30 BDD Then steps construct AuditEvent in test body |
| kiseki-chunk          | DONE   | 34 | THOROUGH (EC + small-file + dedup) | HIGH | Real EC reconstruction, real refcount, real SmallObjectStore. Device-management Thens weak |
| kiseki-block          | DONE   | 26 | THOROUGH (FileBackedDevice round-trip) | HIGH | 18 of 27 BDD scenarios THOROUGH; auto-detect Thens are SHALLOW because file-backed always reports Virtual |
| kiseki-composition    | DONE   | 12 | THOROUGH (cross-context delta inspection) | HIGH | Real CRUD + log bridge + multipart + versioning |
| kiseki-view           | DONE   | 13 | MOCK→SHALLOW | MEDIUM | Real ViewStore + pin lifecycle; materialization assertions weak |
| kiseki-advisory       | DONE   | 13 | MOCK (in BDD), THOROUGH (in unit) | MEDIUM | Adds: TelemetryBus (per-caller scoping unit-tested by phase-13f); BDD step doesn't always invoke the bus |
| kiseki-gateway        | DONE   | 39 | THOROUGH (S3 + NFS3 + NFS4 + locks) | HIGH | Real InMemoryGateway end-to-end; some Thens still construct fresh BudgetEnforcer/LockManager (phase-13f §1, §3) |
| kiseki-client         | DONE   | 23 | SHALLOW | LOW-MEDIUM | FUSE ops + discovery work; RDMA/CXI transport scenarios set string flags only |
| kiseki-control        | DONE   | 22 | MOCK→THOROUGH | HIGH | Adds: node_lifecycle::DrainOrchestrator (phase-13f). Topology, tenant CRUD, IAM, federation use real stores |
| kiseki-server         | WIRED  |  2 | (composed) | MEDIUM | Composes everything; no own BDD |

Total unit + integration tests: **374** pass, 0 fail.

### Confidence rollup

| Level | Crates | What it means |
|---|---|---|
| HIGH    | 9 | common, crypto, proto, transport, keymanager, log (modulo slow Raft), chunk, block, composition, gateway, control — real backends exercised, falsifiable assertions |
| MEDIUM  | 5 | raft (slow gaps), audit, view, advisory, server |
| LOW-MED | 2 | client (RDMA/CXI symbolic), audit (test-fabricated events) |

(No LOW — every crate has at least one real-backend test. The LOW-MED
distinction matters: a crate with mostly correct code but BDD
scenarios that don't really test it is functionally OK at unit tier
but mis-labeled at integration tier.)

---

## BDD coverage (sweep result, 2026-04-26)

| Metric | Value |
|---|---|
| Feature files                                          | 22  |
| @integration scenarios (total)                         | 241 |
| @integration @slow (gated behind `--features slow-tests`) |  60 |
| @integration fast (run by default)                     | 181 |
| Step-definition files                                  |  19 |
| Step functions                                         | ~2,700 |
| `todo!()` step bodies                                  |   9 (in log.rs) + several in raft.rs |
| Default test result                                    | 181/181 fast pass; 0 fail |

### Depth distribution (per BDD-depth-audit.md, 2026-04-26)

| Depth | @integration count | % |
|---|---:|---:|
| THOROUGH (real backend, falsifiable) | 35 + 10 (phase-13f) = 45  | 19% of 241 |
| MOCK (real domain logic, in-memory)  | 92 + 4  (phase-13f) = 96  | 40% of 241 |
| SHALLOW (constructor axiom / vacuous) | 81 + 0 = 81               | 34% of 241 |
| STUB (`todo!()` / empty / no impl)    | 19 + 0 = 19               |  8% of 241 |

(Phase-13f rated 4 SHALLOW + 4 MOCK + 6 THOROUGH within its 14
scenarios; aggregated above.)

### Per-feature distribution (with phase-13f scenarios merged in)

| Feature | Total | THOR | MOCK | SHAL | STUB |
|---|---:|---:|---:|---:|---:|
| authentication              | 0   | —  | —  | —  | — |
| block-storage               | 27  | 18 |  6 |  3 |  0 |
| chunk-storage               | 6   |  4 +1 (phase-13f) = 5 |  2 |  0 |  0 |
| cluster-formation           | 23  |  4 |  9 |  6 |  4 |
| composition                 | 1   |  1 |  0 |  0 |  0 |
| control-plane               | 8   |  0 |  6 |  2 |  0 |
| device-management           | 12  |  2 |  3 |  6 |  1 |
| erasure-coding              | 3   |  3 |  0 |  0 |  0 |
| external-kms                | 18  |  0 |  3 | 14 |  1 |
| key-management              | 6   |  1 |  4 |  1 |  0 |
| log                         | 17  | 6 + 4 (p13f-T) = 10 |  4 |  3 |  4 |
| multi-node-raft             | 30  |  6 |  9 |  3 | 12 |
| native-client               | 9   |  1 |  2 |  6 |  0 |
| operational                 | 11  | 3 + 4 (p13f-T) = 7 |  3 +2 (p13f) =5 |  4 |  1 |
| persistence                 | 14  |  0 |  0 |  0 | 14 |
| protocol-gateway            | 14  | 4 + 6 (p13f-T) =10 |  3 |  0 +4 (p13f) = 4 (4 are M) |  0 |
| small-file-placement        | 13  |  1 |  3 |  9 |  0 |
| storage-admin               | 20  |  3 |  6 | 11 |  0 |
| view-materialization        | 8   |  1 |  3 |  4 |  0 |
| workflow-advisory           | 1   |  0 |  1 |  0 |  0 |

---

## Invariants — enforcement summary

| Category | Count | Enforced (real test) | Documented (no falsifying test) |
|---|---:|---:|---:|
| Log (I-L1..L15)                  | 15 | I-L1, L2 (in unit), L6, L11, L12, L13, L14, L15 (8) | L3, L4, L5, L7, L8, L9 (some unit-tested), L10 (7) |
| Chunk (I-C1..C8)                 | 8  | I-C1, C2, C3, C4 (4) | I-C5, C6, C7, C8 (4) |
| Key (I-K1..K14)                  | 14 | I-K1, K2, K3, K4, K6, K7, K8 (7) | I-K5, K9..K14 (7) |
| Tenant (I-T1..T7)                | 7  | I-T1 partially, I-T2, T3 (3) | I-T4..T7 (4) |
| View (I-V1..V4)                  | 4  | I-V1 partially | I-V2..V4 (3) |
| Auth (I-Auth1..4)                | 4  | I-Auth1..4 in unit | (BDD scenarios moved out) |
| Audit (I-A1..A5)                 | 5  | I-A1, A2 in unit | I-A3..A5 (3) — test-fabricated events |
| Operational (I-O1..O6)           | 6  | I-O1, O2, O3 | I-O4, O5, O6 |
| Advisory (I-WA1..WA19)           | 19 | I-WA1..7 (unit), I-WA5 (phase-13f unit) | I-WA8..WA19 (mostly doc-only) |
| Small-File (I-SF1..SF7)          | 7  | I-SF1, SF5, SF6 (in unit) | I-SF2, SF3, SF4, SF7 (4) |
| Node lifecycle (I-N1..N7, ADR-035) | 7 | I-N1, N4, N6, N7 (phase-13f) | I-N2, N3, N5 (3) |
| **Total**                        | **96** | ~46 enforced | ~50 documented-only |

(96 = the 63 in the prior INDEX + 7 from ADR-035 + 19 (Advisory) +
7 small-file. Prior INDEX combined some categories; this is the
finer-grained current count from `enforcement-map.md` after
phase-13f.)

---

## ADRs (32 accepted)

ADRs 001-032 all accepted. Phase-13f-audit's per-ADR enforcement
table covers ADRs 021, 030, 034, 035 — copy that for those.

For other ADRs:

| ADR | Decision | Status |
|---|---|---|
| 001-019 | Foundational decisions | Mostly ENFORCED via unit tests, structurally enforced by code organization |
| 020 | Workflow Advisory | ENFORCED in unit (BudgetEnforcer + WorkflowTable); BDD MOCK |
| 022 | Transport (TLS) | ENFORCED in unit (kiseki-transport); BDD scenarios SHALLOW (TLS termination handled upstream) |
| 023 | Native client (FUSE/SDK) | DOCUMENTED in BDD (RDMA/CXI symbolic) |
| 025 | Storage admin gRPC | ENFORCED in unit; BDD MOCK→SHALLOW |
| 026 | Multi-node Raft | ENFORCED in unit; BDD MOCK in slow tier (when run); STUB for membership change |
| 027 | Control plane migration | ENFORCED in unit + BDD MOCK |
| 028 | External KMS providers | DOCUMENTED only — provider abstraction does not exist; BDD SHALLOW |
| 029 | Block storage (raw devices) | ENFORCED via FileBackedDevice in BDD |
| 031 | Client-side cache | ENFORCED in unit + BDD MOCK |
| 032 | Async GatewayOps | ENFORCED in unit; BDD coverage limited |

---

## Highest-priority gaps (top 5)

Listed in descending impact order. Each cites a fix scope.

### 1. persistence.feature is fully unimplemented — fix or downgrade

**File**: `specs/features/persistence.feature` (14 scenarios) +
`crates/kiseki-acceptance/tests/steps/log.rs:752-1228` (9 `todo!()`).

All 14 scenarios are `@integration @slow`. Step bodies range from
`todo!()` to "no matching step". They do not run by default
(`@slow` filter). Yet `INDEX.md` (prior) and the spec INDEX claim
14/14 pass. Durability invariants I-L1, I-L4, I-K6, I-V3, I-SF5 are
unenforced at integration tier.

**Implementer action**:
(a) Promote `PersistentShardStore` and `RedbRaftLogStore` into the
World as an alternative to `MemShardStore`, gated by a tag like
`@persistent`.
(b) Implement the 14 scenarios against the real persistent store
with a real close + reopen cycle.
(c) Until (a)+(b) land, downgrade these scenarios to `@unit` or
mark with a new `@unimplemented` tag and exclude from the headline
count.

### 2. external-kms.feature is local-AEAD round-trips, not provider tests

**File**: `crates/kiseki-acceptance/tests/steps/kms.rs` (1737 lines).

14 of 18 scenarios construct `TenantKek::new([byte; 32], …)` inside
the test body, seal/unwrap an envelope, and assert the round-trip.
No `TenantKmsProvider` trait or implementation exists. ADR-028's
provider-validated, audit-logged, in-hardware unwrap claims are
non-falsifiable.

**Implementer action**:
(a) Implement at least a stub `TenantKmsProvider` trait with one
in-process backend (an "internal" provider that wraps `MemKeyStore`).
(b) Add `httptest`-based Vault stub for the Vault scenarios.
(c) Replace each Then-body local roundtrip with a call through the
provider; assert the provider observed the operation (e.g. via a
mock-recorded call list).

### 3. multi-node-raft membership/snapshot/TLS scenarios are `todo!()`

**File**: `crates/kiseki-acceptance/tests/steps/raft.rs:323-353,
442-454, 461-486, 504-512, 528-534, 1036-1085, 1142-1158, 1233-1338`.

12 of 30 multi-node-raft scenarios have `todo!()` bodies. They are
@slow and gated; ADR-026 invariants I-L2, I-L4, ADR-030 §7 (learner
read accelerator), ADR-035 drain → real Raft mechanics are
unenforced.

**Implementer action**:
(a) Add `RaftTestCluster::add_learner` + `change_membership` API
(the action item phase-13f recommended for drain orchestration).
(b) Add snapshot transfer (`TestNetwork::full_snapshot`).
(c) Implement the 12 scenarios against (a)+(b).
(d) Either remove the @slow filter from CI or run it nightly with
failures surfaced.

### 4. The "construct a fixture in the Then body and assert on it"
   anti-pattern recurs in 7 step files.

Sites: `kms.rs` (TenantKek), `view.rs:528` (VersionStore),
`admin.rs:1069` (CompactionProgress), `operational.rs:171` (KeyCache),
`operational.rs:1700` (KeyCache TTL), `client.rs` (transport flags),
`small_file.rs:175-220` (World-state round-trips). Plus the 4 sites
phase-13f flagged (LockManager × 1, BudgetEnforcer × 3).

**Implementer action**:
(a) Add a clippy-style lint or test-time check that flags any
@integration Then body whose only side-effect is `Foo::new(…)` +
`assert!(…)`. Such a body is a constructor axiom, not a system
property.
(b) For each flagged site, either (i) ensure the Given/When step
exercises a *shared* fixture that the Then can query, or
(ii) downgrade the scenario to `@unit`.

### 5. Audit-event Then steps reproduce an "I am the producer"
   pattern in 30+ sites.

Sites grep'd: 12 `// TODO: wire audit infrastructure` in crypto.rs,
6 in chunk.rs, 5 in auth.rs, plus uncounted "construct AuditEvent
literal" sites in operational.rs (lines 60-100, 130-145, 855-870,
920-940, 1000-1015, 1080-1100), admin.rs:331 etc.

**Implementer action**:
(a) Add a helper `assert_audit_event_emitted_for(world, predicate)`
that captures `world.audit_log.tip()` *before* the When and asserts
it advanced *as a side-effect*.
(b) For each site, replace `world.audit_log.append(AuditEvent{…})`
inside the Then with a call to the helper.
(c) Where the production code does not emit, file an escalation in
`specs/escalations/audit-emission-gap.md` so the implementer either
wires emission or marks the spec invariant as DOCUMENTED only.
