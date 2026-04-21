# ADR-027: Single-Language Implementation — Rust Only

**Status**: Accepted (implemented 2026-04-21, Go code removed)
**Date**: 2026-04-20 (proposed), 2026-04-21 (accepted + migrated)
**Context**: Supersedes the implicit language split in `docs/analysis/design-conversation.md` §2.13. No prior ADR recorded the Rust/Go decision.

## Context

Kiseki's original design split the implementation across two languages:

- **Rust** for the core (log, chunks, views, native client, hot paths)
- **Go** for the control plane (tenancy, IAM, policy, flavor, federation, audit export, CLI) and one half each of two cross-cutting contexts (`kiseki-audit` + `control/pkg/audit`; `kiseki-advisory` + `control/pkg/advisory`)
- **gRPC/protobuf** as the boundary

The split was recorded in `docs/analysis/design-conversation.md` §2.13 but never promoted to an ADR. It surfaces in `specs/architecture/module-graph.md` (Go modules section), `.claude/coding/go.md`, and in two contexts that are currently split across both languages. The split pre-dates ADR-001 (pure-Rust, no Mochi/FFI), which already identified "FIPS compliance surface across two languages" as a cost.

At proposal time, 1,490 lines of Go business logic existed with 32/32 BDD
scenarios passing (godog, Strict:true). The migration ported all 32 scenarios
to cucumber-rs backed by a new `kiseki-control` Rust crate (~650 lines,
10 modules) before deleting the Go code. See
`specs/implementation/adr027-go-to-rust-migration.md` for the migration plan
and `specs/findings/adr027-adversarial.md` for the gate-1 review.

## Decision

**Implement Kiseki in Rust only.** Retire the Go control plane, the Go CLI, and the Go halves of `audit` and `advisory`. Keep gRPC/protobuf as the wire boundary between the control plane and data plane so that a future non-Rust control plane remains possible.

Concretely:

1. **New Rust crates** replace the Go packages one-for-one:
   - `kiseki-control` — control plane daemon (tenancy, IAM, policy, flavor, federation, audit export, discovery)
   - `kiseki-cli` — admin CLI
   - The `control/pkg/audit` half is absorbed into `kiseki-audit`
   - The `control/pkg/advisory` half is absorbed into `kiseki-advisory`
2. **gRPC/protobuf stays as the wire boundary.** `kiseki-control` serves `ControlService`, `AuditExportService`, and policy endpoints over gRPC. `kiseki-server` consumes them as a client. No in-process shortcut across the boundary, even though both sides are now Rust.
3. **Architectural firewall is enforced by crate dependencies, not by language.** `kiseki-control` and `kiseki-cli` depend only on `kiseki-common` and `kiseki-proto`. They MUST NOT depend on any data-path crate (`kiseki-log`, `kiseki-chunk`, `kiseki-composition`, `kiseki-view`, `kiseki-gateway-*`, `kiseki-client`, `kiseki-keymanager`). Enforced by a `cargo-deny` or workspace-level architectural lint at CI.
4. **Control plane binaries live alongside data-plane binaries** in `crates/bin/`:
   - `bin/kiseki-control/` (new)
   - `bin/kiseki-cli/` (new)
5. **gRPC server framework**: `tonic` (already the Rust-side choice). Config: `figment` or `config-rs` for layered YAML/env overrides (parity with Go's viper pattern).
6. **Federation / state machine**: `kiseki-control` uses `openraft` (already the project's Raft choice per ADR-026) for replicated control-plane state (policy, opt-out state, tenant topology). This also eliminates the second Raft vendor that a Go control plane would have required (etcd client or dragonboat).

## Rationale

### One domain model

`specs/ubiquitous-language.md` defines Tenant, Org, Project, Workload, RetentionHold, Policy, Flavor, WorkflowRef, OperationAdvisory. Every one of these would otherwise need two implementations (Rust enums/structs + Go types). Two implementations drift: field renames, validation subtly different, invariant enforcement on one side only. Consolidating removes the class of bug where control-plane Go says a name is valid but data-path Rust rejects it (or vice versa).

### One error taxonomy

`specs/architecture/error-taxonomy.md` enumerates retriable / permanent / security error categories. A Go implementation mirrors the Rust taxonomy as Go types + gRPC status mappings. One language means one `thiserror`-derived enum hierarchy and one mapping to `tonic::Status`.

### Smaller FIPS surface

ADR-001 already cited "FIPS compliance surface across two languages" as a reason to reject C/C++ FFI. The same cost applies to Go: either BoringCrypto (Go's FIPS module) is part of the certification boundary, or the control plane sits outside the FIPS module boundary and the certification scope has to be carefully drawn. Rust-only gives one aws-lc-rs FIPS module boundary for the whole system.

### Cross-context crates stop being split

`kiseki-audit` and `kiseki-advisory` are currently split across Rust and Go. That means two queue implementations, two filter implementations, two sets of integration tests, two ways that tenant-scope validation can drift. In Rust-only, each is one crate with one set of invariants.

### Eliminated toolchain duplication

Today's per-commit gate has to run: `cargo fmt`, `clippy`, `cargo-deny`, `cargo test` **and** `go fmt`, `go vet`, `golangci-lint`, `go test -race`. Rust-only halves the CI configuration, halves the local developer setup, and removes one supply-chain audit surface (Go module proxy + checksum DB alongside crates.io).

### Reuse of `kiseki-common` and `kiseki-proto`

The CLI and control plane can import the real domain types rather than regenerated protobuf Go structs. Validation logic written once in `kiseki-common` (e.g., tenant-id parsing, flavor matching, policy inheritance) is reused verbatim in the control plane and the CLI.

### Build-phase cost is low now

Phase 0 has not started. Adding two Rust crates (`kiseki-control`, `kiseki-cli`) is cheaper than maintaining a separate `control/` Go module, its `go.mod`, its generated proto outputs, and its CI lane. The cost rises monotonically with every phase that ships Go code.

### Hiring and cognitive load

Contributors need one language, one async runtime (`tokio`), one tracing stack, one error model. Code review crosses fewer idiom boundaries. Onboarding doc shrinks.

## Alternatives considered

1. **Keep Go as specified.**
   - Pro: Go's ecosystem for control planes (cobra, viper, operator-sdk, client-go patterns) is the golden path; k8s, etcd, Consul all use it. GC is fine on cold paths. Operators extending the system are more likely to know Go.
   - Pro: the language wall *is* the architectural wall — the Go control plane physically cannot reach into data-plane memory or internals.
   - Con: every benefit above comes with the duplication, drift, and FIPS-surface costs enumerated in "Rationale". With no code written, the ecosystem-maturity argument is weaker than at a later stage.

2. **Port only the CLI to Rust, keep the Go control-plane daemon.**
   - Pro: preserves Go for the longer-lived daemon code where operator-sdk patterns matter most. Low churn.
   - Con: doesn't remove duplication for the split contexts (`audit`, `advisory`). Doesn't shrink the FIPS surface. Doesn't remove the second toolchain from CI. Half-measure.

3. **Rewrite the core in Go (single-language Go).**
   - Rejected immediately: Go GC and lack of precise control over allocation and layout disqualify it from the hot data path at 200 Gbps per NIC. This inverts the original rationale for Rust in the core.

4. **Separate Rust crate per Go package, but share no runtime (same-language boundary still isolated by process).**
   - Considered. Rejected: unnecessary. The isolation value of "separate OS process" is already provided by `kiseki-control` being a distinct binary. Running two daemons is orthogonal to the language question.

5. **Defer the decision until after Phase 3.**
   - Rejected: the decision is cheapest to reverse *now*. Each build phase that ships Go code raises the cost of consolidation and lets duplication set in. The analyst already flagged the split without recording a decision; formalizing now is overdue.

## Consequences

### Positive

- Single toolchain: `cargo fmt`, `clippy`, `cargo-deny`, `cargo test`, `cargo audit`. Lefthook configuration shrinks.
- Single FIPS module boundary (aws-lc-rs).
- Domain types (`Tenant`, `Policy`, `RetentionHold`, `Flavor`, `WorkflowRef`, `OperationAdvisory`) exist once in `kiseki-common`.
- `kiseki-audit` and `kiseki-advisory` become whole crates rather than split halves. Their invariants (I-A1..I-A3, I-WA1..I-WA16) are enforced in one place.
- `kiseki-control` can reuse `openraft` (ADR-026) for its replicated state rather than requiring a second Raft implementation (etcd/dragonboat).
- No generated Go protobuf stubs to keep in sync; one generated tree under `crates/kiseki-proto/`.
- CI matrix shrinks; no `go test -race` lane.

### Negative

- Loses the "language wall as architectural wall" property. Must be replaced with crate-graph enforcement (see "Enforcement" below). This is a **discipline cost** and must be tooled, not trusted.
- Rust's CLI/operator ecosystem (`clap`, `tonic`, `figment`) is less mature than Go's (`cobra`, `viper`, `operator-sdk`). Some patterns (admission webhooks, CRD controllers) will require more bespoke code if we ever grow a k8s operator.
- Contributors with Go-only platform experience face a higher barrier to writing control-plane extensions.
- `kiseki-control` uses `tokio` for async I/O and is exposed to async-Rust complexity on request handlers (cancellation safety, `'static` bounds) that Go handlers would not have had.
- One-time rewrite cost for the control-plane spec surface (`api-contracts.md`, `module-graph.md`, `.claude/coding/go.md` → remove or archive, `build-phases.md` may need to re-sequence control-plane phases).

### Enforcement (replacing the language wall)

The split previously enforced "control plane never reaches into data plane" structurally. In Rust-only, this is enforced by:

1. **Crate-graph rule.** `kiseki-control` and `kiseki-cli` depend only on `kiseki-common` and `kiseki-proto`. This is asserted by a CI check that greps Cargo manifests, or by `cargo-deny`'s `bans` section, or by a custom workspace lint.
2. **No re-export shortcut.** `kiseki-common` MUST NOT re-export internal types from data-path crates. This is already the case; restated here as a rule.
3. **gRPC boundary preserved.** Even though both sides are now Rust, control-plane-to-data-plane traffic still goes through `tonic` over gRPC, not through in-process trait calls. This keeps the wire contract as the source of truth and preserves the option of a non-Rust control plane later.
4. **Runtime separation.** `kiseki-control` runs as its own binary (`bin/kiseki-control/`), not as a library linked into `kiseki-server`. The isolation that process separation provides is preserved.

## Migration

No code exists yet. Migration is a spec update:

1. `docs/analysis/design-conversation.md` §2.13: annotate with a pointer to this ADR.
2. `specs/architecture/module-graph.md`: delete the "Go modules (control plane)" section; add the new Rust crates (`kiseki-control`, `kiseki-cli`) and update the "Bounded context → module mapping" table to say Rust for every row.
3. `specs/architecture/build-phases.md`: review Phase sequencing — the Go control-plane phase collapses into a Rust phase; audit/advisory phases no longer have a "Go side" task.
4. `.claude/CLAUDE.md` and `.claude/guidelines/go.md`: remove Go from the workflow router; keep `.claude/coding/go.md` archived (move to `specs/archive/` or delete) as a historical record.
5. `.claude/coding/rust.md`: add a "control plane" section describing `kiseki-control`/`kiseki-cli` conventions (config with `figment`, CLI with `clap`, server with `tonic` + `axum` for any REST admin surface).
6. `Makefile` (when it exists): drop Go lanes.
7. `specs/features/control-plane.feature`: BDD scenarios remain; the step definitions move from `godog` to `cucumber-rs`.

## Open items (escalated to adversary gate-1)

- Verify the crate-graph rule (control plane depends only on `kiseki-common`/`kiseki-proto`) is enforceable with `cargo-deny` alone, or whether a custom workspace lint is needed.
- Confirm `cucumber-rs` covers the Gherkin features that `godog` was planned to run, without step-definition regressions.
- Confirm FIPS posture: aws-lc-rs covers the control-plane's TLS needs (mTLS CA, admin endpoints) as well as the data-plane's. No Go BoringCrypto equivalent is needed.
- Verify that removing the Go language wall does not create a realistic path by which a control-plane code change accidentally links data-path crates. Propose a pre-merge check if manifest-grep is insufficient.
- Decide the fate of `control/pkg/discovery`: if fabric discovery uses libfabric/CXI, it was already going to need a Rust FFI layer; confirm the Rust-only home for it is `kiseki-control` (or a new `kiseki-discovery` crate).

## References

- ADR-001: Pure Rust, No Mochi Dependency (FIPS surface precedent).
- ADR-021: Workflow Advisory Architecture (defines the Rust+Go split for advisory that this ADR collapses).
- ADR-026: Raft Topology — openraft is the Rust-side Raft; now also the control plane's Raft.
- `docs/analysis/design-conversation.md` §2.13 — original (now superseded) language-split rationale.
- `specs/architecture/module-graph.md` — current two-language module layout (to be rewritten).
- `.claude/coding/go.md` — Go coding standards (to be archived on acceptance).
