# Implementation Plan — Kiseki

**Status**: Active. **Last updated**: 2026-04-19 (Phase 0 committed).
**Owner**: implementer role (`.claude/roles/implementer.md`).

Execution discipline for turning the design tree
(`specs/architecture/*`, `specs/features/*`) into working code. The
architect owns the **phase decomposition** in
`specs/architecture/build-phases.md`; this document layers **how** the
implementer executes each phase — TDD/BDD cycle, gate structure,
language boundaries, definition-of-done.

This is a behavioural spec, not a redesign. It does not override any
architecture artifact; when something here conflicts with an ADR or an
invariant, the ADR/invariant wins and this plan is amended.

---

## Principles (non-negotiable)

1. **BDD-first, TDD-inside.** Every Gherkin scenario in
   `specs/features/` gets a step definition and a failing test before
   any production code is written. Unit tests live alongside the code
   they cover.
2. **One scenario at a time.** No batching. Red → green → refactor →
   commit. Exactly one scenario is in `in_progress` at any moment.
3. **Feature completeness over speed.** If a scenario can't be
   implemented faithfully, escalate (`specs/escalations/`) — do not
   implement a degraded variant.
4. **No shortcuts on invariants.** Every invariant an invariant map
   maps to *this* phase must have an enforcement point in the code of
   *this* phase, not "TODO next phase".
5. **Gate discipline.** No phase advances without adversary gate-1
   (pre-code), auditor sign-off (post-code HIGH fidelity), adversary
   gate-2 (findings closed), and — where relevant — integrator review.
6. **Architectural boundaries are law.** Data-path crates never depend
   on `kiseki-advisory`. Go never touches a data-path byte. Python is
   bindings + tests only. Cross-language flow is gRPC/protobuf or PyO3.

---

## Language boundaries

| Layer | Language | Scope |
|---|---|---|
| Data path, hot paths, crypto | **Rust** | 14 crates under `crates/` |
| Control plane, IAM, policy, federation, CLI | **Go** | `control/` module |
| Rust ↔ Go wire boundary | **protobuf only** | `specs/architecture/proto/kiseki/v1/*.proto` |
| Client bindings | Rust native + **PyO3 (Python)** + C FFI + C++ wrapper | `bindings/` |
| E2E orchestration + validation | **Python** | `tests/e2e/` |

Hard rules:

- Go never reads or writes a tenant byte on the data fabric.
- Rust never implements tenant-policy semantics (those live in
  `control/pkg/`).
- Cross-language communication is only via protobuf/gRPC or PyO3. No
  CGO, no unix pipes, no shared files for coordination.
- Python has no business logic — thin wrappers and test scripts only.

---

## Per-phase execution cycle

Applies to every phase `N` in `build-phases.md`. Steps run in order:

### 1. Orient

- Read `specs/architecture/module-graph.md` for crate boundaries.
- Read `specs/architecture/enforcement-map.md` for the invariants owned
  by this phase.
- Read the phase's feature file(s) in `specs/features/`.
- Read relevant ADRs and `specs/failure-modes.md` entries.
- One-line summary: `I am implementing [phase]. Boundaries: [X].
  Dependencies: [Y]. Scenarios: [N]. Fidelity: [level or unaudited].`

### 2. Scaffold

- Crate manifest(s), workspace wiring, feature flags per
  `module-graph.md` §"Crate feature flags".
- CI skeleton wired (fmt + clippy + test + deny). Every new crate is
  in the workspace before any code runs.
- No production code yet. No half-tests.

### 3. BDD harness (red)

- `cucumber-rs` in Rust, `godog` in Go, `pytest-bdd` in Python.
- Step definitions in `tests/acceptance/` — one file per feature file.
- Every scenario gets at least a stub with `unimplemented!()` or
  `pending` — all red.
- Feature file ↔ step file coverage is a verify-script check, not
  optional.

### 4. TDD loop (per scenario)

Strictly in this order:

1. Pick one Gherkin scenario (top of the feature file, or
   dependency-ordered).
2. Write a failing unit test for the smallest concrete behaviour the
   scenario needs.
3. Write the minimum production code to make it pass.
4. Run the **full** test suite for the crate. Any regression blocks
   progress.
5. Refactor, re-run everything.
6. Mark the scenario's step as green; commit; next scenario.

No batching. No parallel scenarios. No "I'll come back to that step".

### 5. Pre-commit gate

Before every commit: `make` locally, `/project:verify` if the run set
changed. Lefthook enforces fmt + lint + test + vet. No `--no-verify`.

### 6. Adversary gate-2

After the last scenario in the phase is green: adversary runs
structured review against attack vectors
(`.claude/roles/adversary.md`). Findings go to
`specs/findings/<phase>-gate2.md`. Blocking findings are resolved
before gate-3.

### 7. Auditor sweep

Auditor runs fidelity sweep. HIGH confidence required — no
self-certification by the implementer. Output to
`specs/fidelity/<phase>.md` + `INDEX.md`.

### 8. Adversary gate-3 (findings closure)

Every gate-2 finding closed with status `resolved`, `accepted with
mitigation`, or escalated via `specs/escalations/`.

### 9. Integrator (when cross-feature)

When the phase touches multiple bounded contexts, integrator reviews
the cross-context surface (interactions, failure cascades, invariant
ownership).

### 10. Spec-check + phase close

`/project:spec-check` confirms no drift between specs and code.
Implementer marks phase complete in this document and in
`build-phases.md` (append a "Status: completed at commit X" line).

---

## Per-phase implementer checklist

Each phase below lists its unique inputs on top of the generic cycle.
**Does not repeat** items already in `build-phases.md` — read that file
for architectural detail.

### Phase 0 — Foundation (COMPLETED at `726351c`)

**Crates**: `kiseki-common`, `kiseki-proto`.

- Workspace scaffold, clippy pedantic deny, `unsafe_code = "deny"`.
- Domain types from `ubiquitous-language.md` (exact names).
- Typed errors per `error-taxonomy.md`, categorised Retriable /
  Permanent / Security.
- HLC `tick` + Lamport `merge` + `Ord`; proptest suite.
- Protobuf: 9 `.proto` files; `build.rs` compiles to Rust; Go target
  in `Makefile`.
- CI wired (`.github/workflows/ci.yml`).
- No Gherkin scenarios — Phase 0 has no feature assignments.

### Phase 1 — Cryptography (NEXT)

**Crate**: `kiseki-crypto`. **Depends on**: Phase 0.
**Feature files**: `key-management.feature`.
**Gate-1 findings**: `specs/findings/crypto-gate1.md` (in progress).

- FIPS AEAD (AES-256-GCM via `aws-lc-rs`), `fips` feature flag.
- Envelope encrypt/decrypt round-trip.
- HKDF-SHA256 DEK derivation per ADR-003 (local, not RPC).
- Tenant KEK wrap/unwrap; crypto-shred cache TTL semantics (ADR-011,
  I-K15).
- Chunk ID derivation — sha256 default, HMAC-SHA256 tenant-isolated
  (I-K10, I-X2).
- Compress-then-encrypt + padding; pad scheme behind a tenant opt-in
  flag (I-K14).
- `Zeroizing<T>` + `mlock` (RLIMIT_MEMLOCK handling explicit).
- Property tests: nonce never reused across encrypts with same
  (master_key, chunk_id, epoch).
- BDD harness: `key-management.feature` via `cucumber-rs`.

**Gate-1 blockers to resolve before code**: see
`specs/findings/crypto-gate1.md`.

### Phase 2 — Transport

**Crate**: `kiseki-transport`. **Depends on**: Phase 0.
**Feature files**: none directly; authentication handshake touched
from `authentication.feature` and `operational.feature`.

- `Transport` trait. TCP+TLS reference impl; Cluster CA validation.
- `cxi` (libfabric-sys FFI; every `unsafe` block has `// SAFETY:`),
  `verbs` features.
- Fallback chain: CXI → verbs → TCP verified by integration test.
- Connection pool + keepalive + timeout semantics.

### Phase 3 — Log

**Crate**: `kiseki-log`. **Depends on**: 0, 1.
**Feature file**: `log.feature`.

- Delta envelope: structurally separated header (plaintext) + payload
  (opaque) (I-L7).
- openraft integration; leader election, replication, snapshotting.
- Shard lifecycle: create / split-under-load / merge / maintenance
  (I-L6, I-O1, I-O6).
- SSTable on RocksDB; compaction merges by `(hashed_key, sequence)` —
  payloads never decrypted (I-O2).
- Consumer watermarks + GC (I-L4, I-A5).

### Phase 4 — System Key Manager

**Crate**: `kiseki-keymanager`. **Binary**: `kiseki-keyserver`.
**Depends on**: 0, 1.
**Feature file**: `key-management.feature` (manager surface).

- Raft-replicated master keys (ADR-007); epoch create/rotate/retain.
- HKDF derivation is NOT here (Phase 1 library does it locally).
- System KEK rotation without downtime; health gRPC endpoint.

### Phase 5 — Audit

**Crate**: `kiseki-audit`. **Depends on**: 0, 1, 3.
**Feature file**: `operational.feature` (audit subset) +
audit-event scenarios scattered across other features.

- Per-tenant audit shards (ADR-009).
- Append-only event log; watermark tracking integrated with
  `kiseki-log` GC (I-L4, I-A4, I-A5 safety valve).
- Event types from every prior context + advisory events (I-WA8 batch
  guarantees).

### Phase 6 — Chunk Storage

**Crate**: `kiseki-chunk`. **Depends on**: 0, 1, 4.
**Feature file**: `chunk-storage.feature`.

- Idempotent write = dedup → refcount++ (I-C2).
- Affinity pools, EC encode/decode, placement (I-C3, I-C4).
- Retention holds gate GC (I-C2b).
- Repair via EC rebuild; integrity check path.

### Phase 7 — Composition

**Crate**: `kiseki-composition`. **Depends on**: 0, 1, 3, 6.
**Feature file**: `composition.feature`.

- Composition CRUD + namespace management + refcount deltas.
- Multipart upload FSM (I-L5 — visibility gated on finalize).
- Inline data below `INLINE_DATA_THRESHOLD` (ADR-006).
- Versioning; cross-shard rename → EXDEV (I-L8).

### Phase 8 — View Materialization

**Crate**: `kiseki-view`. **Depends on**: 0, 1, 3, 6.
Note: **not** Phase 7 (see ADV-ARCH-08).
**Feature file**: `view-materialization.feature`.

- Stream processor: consume deltas, decrypt payload, materialize view.
- Lifecycle: create / discard / rebuild from log (I-V1).
- MVCC pins with TTL (I-V4).
- Staleness tracking + alerting; read-your-writes / bounded-staleness
  enforcement (I-V3, I-K9).

### Phase 9 — Protocol Gateways

**Crates**: `kiseki-gateway-nfs`, `kiseki-gateway-s3`.
**Depends on**: 0, 1, 7, 8.
**Feature file**: `protocol-gateway.feature`.

- NFSv4.1 server (ADR-013 scope), lock state.
- S3 subset (ADR-014), multipart + versioning.
- Gateway-side encryption: TLS in → encrypt → write. No plaintext
  past this boundary (I-K1, I-K2).
- Protocol error mapping to typed errors.
- Conformance test harnesses (pynfs subset, s3-tests subset).

### Phase 10 — Native Client

**Crate**: `kiseki-client`. **Binary**: `kiseki-client-fuse`.
**Depends on**: 0, 1, 2, 6, 7, 8.
**Feature file**: `native-client.feature`.

- FUSE via `fuser`.
- Seed-based discovery (ADR-008) — no control-plane dependency.
- Transport selection CXI → verbs → TCP.
- Client-side encryption: **plaintext never leaves the process**.
- Prefetch + access-pattern detection + client cache + invalidation.

### Phase 10.5 — Client bindings

**Non-crate**: `bindings/python/`, `bindings/c/`, `bindings/cpp/`.

- PyO3 wrapper (`maturin` build); `.pyi` stubs.
- C FFI via `cbindgen`; C++ RAII wrapper on top of the C header.
- Per-binding tests prove the boundary: the non-Rust side carries
  zero business logic.

### Phase 11 — Control Plane (Go)

**Module**: `control/`. **Binaries**: `kiseki-control`, `kiseki-cli`.
**Feature file**: `control-plane.feature`.
May run in parallel with Rust phases 3 – 10.

- Tenancy, IAM (Cluster CA, mTLS cert issuance, access requests),
  policy (quotas, compliance tags, holds, placement).
- Flavor best-fit matching; federation async replication.
- Audit export: tenant-scoped filtering to SIEM.
- `control/pkg/advisory`: profile allow-lists, hint budgets, opt-out
  FSM (ADR-021 §6; invariants I-WA7, I-WA18).
- BDD via `godog`.

### Phase 11.5 — Workflow Advisory runtime

**Crate**: `kiseki-advisory`. **Depends on**: 0, 5, 11.
**Feature file**: `workflow-advisory.feature`.

- Isolated tokio runtime + separate gRPC listener (ADR-021 §1).
- Workflow / effective-hints / prefetch-ring tables (ADR-021 §4).
- Token-bucket budget enforcer; advisory audit emitter with
  drop-and-audit (I-WA8).
- k-anonymity telemetry bucketing (I-WA5).
- Covert-channel hardening: bucketed timing + size padding
  (I-WA15).
- `AdvisoryLookup` hot-path surface with ≤ 500 µs deadline; returns
  `None` on overload (I-WA2).
- Mandatory property tests for I-WA1 (data-path equivalence), I-WA2
  (deadline), I-WA6/I-WA15 (`ScopeNotFound` indistinguishability).

### Phase 12 — Integration

**Binary**: `kiseki-server`. **Depends on**: all Rust phases + 11.5.
**Feature files**: cross-context scenarios; full Python e2e suite.

- Compose every Rust crate into the single server binary.
- Per-tenant stream-processor process management (ADR-012).
- Discovery responder, node health reporting, maintenance mode.
- Advisory runtime wiring: second tokio runtime, separate listener,
  `AdvisoryLookup` injected into every data-path context.
- Full Python e2e: write/read round-trip across all protocols,
  encryption invariants, crypto-shred, multi-protocol cross-view,
  failover, federation.
- Fault injection: node crash, partition, clock skew, advisory
  overload (F-ADV-1). Soak test 48 h.

**Exit gate**: `/project:e2e` green; integrator signs off on
cross-context interactions; adversary gate-3 with findings closed;
auditor fidelity HIGH across all contexts.

---

## Cross-cutting discipline

- **Commit cadence**: one scenario per commit; conventional commit
  messages; `/project:verify` before each.
- **CI from Phase 0**: `cargo fmt --check && cargo clippy -D warnings
  && cargo deny check && cargo test` + `go fmt && go vet &&
  golangci-lint && go test -race` + `ruff && mypy && pytest`. Lefthook
  pre-commit hooks.
- **No phase advances** without adversary + auditor sign-off. Findings
  are escalations, not TODOs in code.
- **Spec drift**: `/project:spec-check` after every phase. Any drift
  escalates to architect or analyst.
- **Feature completeness over speed**: scenarios that can't be faithfully
  implemented are escalated, not trimmed.

---

## Tracking

| Phase | Status | Commit | Gate-1 | Gate-2 | Auditor | Notes |
|---|---|---|---|---|---|---|
| 0 | done | `726351c` | n/a | `phase0-gate2.md` | n/a | Foundation; adversarial pass resolved 4 findings |
| 1 | done | — | — | `phase1-gate2.md` | — | kiseki-crypto: 2 High resolved, 1 Medium resolved, 1 Low resolved |
| 2 | done | — | — | `phase2-gate2.md` | — | kiseki-transport: 0 blocking, 3 Medium + 2 Low deferred |
| 3 | done | — | — | `phase3-gate2.md` | — | kiseki-log: 1 blocking resolved (compaction), Raft deferred |
| 4 | done | — | — | `phase4-gate2.md` | — | kiseki-keymanager: Raft deferred, 0 blocking |
| 5 | done | — | — | `phase5-gate2.md` | — | kiseki-audit: per-tenant shards, event types, 0 blocking |
| 6 | done | — | — | `phase6-gate2.md` | — | kiseki-chunk: dedup, refcount, GC, holds, pools |
| 7 | done | — | — | — | — | kiseki-composition: CRUD, namespace, multipart, EXDEV |
| 8 | pending | — | — | — | — | |
| 9 | pending | — | — | — | — | |
| 10 | pending | — | — | — | — | |
| 10.5 | pending | — | — | — | — | bindings |
| 11 | pending | — | — | — | — | Go control plane |
| 11.5 | pending | — | — | — | — | advisory |
| 12 | pending | — | — | — | — | integration |

Update this table at the close of each phase.
