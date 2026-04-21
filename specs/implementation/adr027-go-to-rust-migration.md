# ADR-027 Migration: Go Control Plane to Rust

## Context

ADR-027 (accepted with fixes) eliminates the Go control plane in favor of
Rust-only. Current Go codebase: 1,490 lines business logic, 32/32 BDD
scenarios (godog, Strict:true), 2,079 lines step definitions, Docker infra.

Adversarial findings require: BDD coverage never drops to zero (ADV-2),
crate-graph enforcement (ADV-3), sync handlers (ADV-4), kiseki-common
visibility rules (ADV-13).

**Goal**: Port all 32 control-plane BDD scenarios to cucumber-rs, backed by
a new `kiseki-control` crate. Delete Go code only after 32/32 GREEN in Rust.

---

## New Crate: `crates/kiseki-control/`

```
src/
  lib.rs                -- crate root, re-exports
  error.rs              -- ControlError enum -> tonic::Status
  tenant.rs             -- Organization, Project, Workload, Store, ValidateQuota
  iam.rs                -- AccessRequest state machine (pending->approved/denied->expired)
  policy.rs             -- EffectiveStaleness, compliance floor
  flavor.rs             -- Flavor, MatchBestFit, DefaultFlavors
  federation.rs         -- Peer, Registry
  namespace.rs          -- Namespace, Store (shard assignment, read-only mode)
  retention.rs          -- Hold, Store
  maintenance.rs        -- State (enabled/disabled)
  advisory_policy.rs    -- ScopePolicy, HintBudget, ProfilePolicy, OptOutState, validation
  grpc/
    mod.rs
    control_service.rs  -- impl ControlService (tonic)
    audit_service.rs    -- impl AuditExportService (tonic)
```

**Dependency firewall** (ADV-3): `Cargo.toml` depends ONLY on:
- `kiseki-common` (types: ComplianceTag, DedupPolicy, Quota, OrgId, etc.)
- `kiseki-proto` (generated gRPC)
- `tonic`, `tokio`, `uuid`, `thiserror`, `serde`

**No data-path crates allowed.** CI check via Makefile `arch-check` target.

**Concurrency** (ADV-4): All stores use `std::sync::RwLock` (not tokio).
gRPC handlers wrap sync logic -- control plane is cold path.

---

## Phases (BDD-first, one scenario at a time)

### Phase A: Tenant Lifecycle (3 scenarios)

| # | Scenario | Requires |
|---|----------|----------|
| 1 | Create a new organization | `tenant.rs`: Organization, Store |
| 2 | Create project within organization | `tenant.rs`: Project, ValidateQuota, EffectiveComplianceTags |
| 3 | Create workload within tenant | `tenant.rs`: Workload |

**New files**: `crates/kiseki-control/src/{lib,error,tenant}.rs`,
`crates/kiseki-acceptance/tests/steps/control.rs` (step defs),
extend `KisekiWorld` with control-plane state.

**Exit**: 3/32 GREEN in cucumber-rs. Go tests still passing.

### Phase B: Namespace + Maintenance + CP Outage (4 scenarios)

| # | Scenario | Requires |
|---|----------|----------|
| 4 | Create namespace triggers shard | `namespace.rs` |
| 5 | Cluster-wide maintenance mode | `maintenance.rs` |
| 6 | CP unavailable -- data path continues | World flags only |
| 7 | Quota enforcement during CP outage | World flags only |

**New files**: `namespace.rs`, `maintenance.rs`

**Exit**: 7/32 GREEN.

### Phase C: IAM + Tenant Isolation (4 scenarios)

| # | Scenario | Requires |
|---|----------|----------|
| 8 | Access request -- requires approval | `iam.rs`: AccessRequest |
| 9 | Access approved -- scoped, time-limited | `iam.rs`: Approve() |
| 10 | Access denied | `iam.rs`: Deny() |
| 11 | Cross-tenant isolation | `iam.rs`: isolation check |

**New files**: `iam.rs`

**Exit**: 11/32 GREEN.

### Phase D: Quota Enforcement (3 scenarios)

| # | Scenario | Requires |
|---|----------|----------|
| 12 | Write rejected -- quota exceeded | Quota math on World |
| 13 | Workload quota within org ceiling | ValidateQuota |
| 14 | Quota adjustment by admin | Quota update logic |

**No new files** -- logic already in tenant.rs, steps only.

**Exit**: 14/32 GREEN.

### Phase E: Flavor + Compliance + Retention (6 scenarios)

| # | Scenario | Requires |
|---|----------|----------|
| 15 | Flavor best-fit matching | `flavor.rs` |
| 16 | Flavor unavailable | `flavor.rs` |
| 17 | Compliance tag inheritance | `policy.rs` |
| 18 | Tag removal rejected if data exists | `policy.rs` |
| 19 | Set retention hold | `retention.rs` |
| 20 | Release retention hold | `retention.rs` |

**New files**: `flavor.rs`, `policy.rs`, `retention.rs`

**Exit**: 20/32 GREEN.

### Phase F: Federation (3 scenarios)

| # | Scenario | Requires |
|---|----------|----------|
| 21 | Register federation peer | `federation.rs` |
| 22 | Data residency enforcement | `federation.rs` + compliance |
| 23 | Config sync across sites | `federation.rs` |

**New files**: `federation.rs`

**Exit**: 23/32 GREEN.

### Phase G: Advisory Policy (9 scenarios)

| # | Scenario | Requires |
|---|----------|----------|
| 24 | Cluster-wide hint-budget ceilings | `advisory_policy.rs` |
| 25 | Profile allow-list narrows per scope | `advisory_policy.rs` |
| 26 | Workload budget cannot exceed ceiling | `advisory_policy.rs` |
| 27 | Three-state opt-out transition | `advisory_policy.rs` |
| 28 | Cluster-wide emergency disable | `advisory_policy.rs` |
| 29 | Prospective policy changes | `advisory_policy.rs` |
| 30 | Audit export includes advisory events | World + audit list |
| 31 | Federation does NOT replicate advisory | federation + advisory |
| 32 | Pool authorization -- opaque labels | `advisory_policy.rs` |

**New files**: `advisory_policy.rs`

**Exit**: 32/32 GREEN. Migration complete.

---

## Phase H: Infrastructure + Go Removal

After 32/32 GREEN in cucumber-rs:

1. Add `kiseki-control` binary entry point (`src/main.rs`)
2. Wire into `kiseki-server/runtime.rs` (register ControlService on gRPC)
3. Update `Dockerfile.control` to Rust builder (reuse Dockerfile.server pattern)
4. Verify `docker compose up --build` starts control service
5. Git tag `pre-go-removal` for rollback safety (ADV-10)
6. Delete `control/` directory entirely
7. Remove Go from: `Makefile`, `lefthook.yml`, `.github/workflows/ci.yml.disabled`
8. Update ADR-027: fix stale premise, mark Accepted
9. Update `specs/architecture/module-graph.md`: remove Go section
10. Update `specs/architecture/build-phases.md`: Phase 11 is now Rust

---

## Crate-Graph Enforcement (ADV-3)

Makefile target:
```
arch-check:
  @! grep -E 'kiseki-(log|chunk|composition|view|gateway|client|keymanager|crypto|raft|transport|server|audit|advisory)' \
      crates/kiseki-control/Cargo.toml \
      || { echo "VIOLATION: kiseki-control depends on data-path crate"; exit 1; }
```

Added to `verify` target. Pre-commit hook catches violations.

---

## Key Reference Files

| Purpose | File |
|---------|------|
| Types to reuse | `crates/kiseki-common/src/tenancy.rs` |
| gRPC pattern | `crates/kiseki-log/src/grpc.rs` |
| cucumber-rs World | `crates/kiseki-acceptance/tests/acceptance.rs` |
| Go business logic | `control/pkg/grpc/control_service.go` |
| Go step defs | `control/tests/acceptance/steps_*.go` |
| Go World state | `control/tests/acceptance/state.go` |
| Feature file | `specs/features/control-plane.feature` |

## Verification

Per phase: `cargo test -p kiseki-acceptance` + `cd control && go test ./...`
Final: 32/32 cucumber-rs GREEN, `docker compose up --build`, Go deleted.
Adversarial review after Phase G, before Go removal.
