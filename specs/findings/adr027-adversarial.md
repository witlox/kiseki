# ADR-027 Adversarial Review — Single-Language Rust-Only

**Reviewer**: adversary
**Date**: 2026-04-21
**Artifact**: `specs/architecture/adr/027-single-language-rust-only.md`
**Mode**: Architecture (ADR gate-1)

## Summary

5 Critical, 3 High, 4 Medium, 2 Low findings.

---

## Finding: ADR-027-ADV-1 — Stale premise: "no code exists yet"

Severity: **Critical**
Category: Correctness > Specification compliance
Location: ADR-027 §Context, §Migration
Spec reference: `build-phases.md` Phase 11

Description: The ADR repeatedly states "no code exists yet" and "Phase 0 has not started." In reality:
- 1,490 lines of hand-written Go source code exist
- 32/32 Go BDD scenarios pass (godog, Strict:true)
- 2,079 lines of acceptance test step definitions across 12 files
- Control plane gRPC server is functional (tenant CRUD, IAM, namespace, quota, compliance tags, maintenance)
- Docker image builds and runs (`Dockerfile.control`)
- Generated proto stubs exist (18,998 lines)

The migration cost is therefore non-zero. The ADR's cost/benefit analysis is based on a false premise.

Suggested resolution: Rewrite §Context and §Migration to acknowledge existing Go code. Add a concrete migration plan with effort estimate (rewrite ~1,500 lines of Go business logic + port 2,079 lines of BDD step definitions to cucumber-rs).

---

## Finding: ADR-027-ADV-2 — 32 passing BDD scenarios at risk during port

Severity: **Critical**
Category: Correctness > Implicit coupling
Location: ADR-027 §Migration; `control/tests/acceptance/`
Spec reference: `specs/features/control-plane.feature` (305 lines, 32 scenarios)

Description: The 32 godog scenarios are currently GREEN with `Strict:true` — every step has a real assertion, not a stub. Porting to cucumber-rs means rewriting all 12 step-definition files. During the port, these scenarios will regress to RED, creating a gap where control-plane correctness is unverified.

The ADR says "BDD scenarios remain; the step definitions move from godog to cucumber-rs" but provides no strategy for maintaining test coverage during the transition.

Suggested resolution: Define a migration strategy: (a) keep Go tests running until Rust equivalents are GREEN, running both in parallel during transition; or (b) port one scenario at a time, keeping the remaining Go tests intact. Never have zero coverage.

---

## Finding: ADR-027-ADV-3 — Crate-graph enforcement is unproven

Severity: **High**
Category: Security > Trust boundaries
Location: ADR-027 §Enforcement
Spec reference: `module-graph.md` §Dependency rules

Description: The ADR replaces the language wall (Go physically cannot call Rust data-path code) with a crate-graph rule ("kiseki-control depends only on kiseki-common and kiseki-proto"). This is weaker:

1. **No tooling specified.** `cargo-deny`'s `bans` section can deny specific crates, but the ADR doesn't confirm this is configured or even possible for "deny all except X" patterns.
2. **Transitive leaks.** If `kiseki-common` grows a pub re-export from a data-path crate (even accidentally), the wall is breached.
3. **Compile-time only.** A contributor who adds `kiseki-log` to `kiseki-control/Cargo.toml` gets no error until CI runs — the language wall gave immediate "wrong language" feedback.

The ADR lists this as an "open item" but the decision is recorded as accepted rather than conditional on resolving it.

Suggested resolution: Before accepting, implement and demonstrate the enforcement mechanism. Options: (a) workspace-level `[workspace.metadata.arch-rules]` with a CI script, (b) `cargo-deny` ban configuration, or (c) a pre-commit hook that greps `kiseki-control/Cargo.toml` for forbidden deps. Demonstrate it catches a violation.

---

## Finding: ADR-027-ADV-4 — Async complexity on control-plane request handlers

Severity: **High**
Category: Robustness > Error handling quality
Location: ADR-027 §Consequences (Negative)
Spec reference: None

Description: The ADR acknowledges "async-Rust complexity on request handlers (cancellation safety, `'static` bounds)" as a negative consequence but dismisses it. This is a real engineering risk:

1. Control plane handlers manage **stateful multi-step operations** (tenant creation → cert issuance → audit event → response). In Go, these are linear functions. In async Rust with tonic, cancellation at any `.await` point can leave partial state.
2. The Go `sync.RWMutex` + in-memory map pattern used in `control_service.go` translates to either `tokio::sync::RwLock` (requires careful cancellation handling) or `std::sync::RwLock` wrapped in `block_in_place`.
3. Go's `context.Context` cancellation is explicit and opt-in per check. Rust's `.await` cancellation is implicit and pervasive.

The existing Go code is straightforward synchronous logic. The Rust port will be more complex for the same functionality.

Suggested resolution: Acknowledge this as a real cost. Consider whether `kiseki-control` should use synchronous handlers (blocking thread pool + `tonic` with `block_in_place`) rather than full async, keeping the control-plane simple since it's not on the hot path.

---

## Finding: ADR-027-ADV-5 — Loss of operator ecosystem is understated

Severity: **High**
Category: Robustness > Observability gaps
Location: ADR-027 §Alternatives considered, option 1
Spec reference: None

Description: The ADR mentions "Go's ecosystem for control planes (cobra, viper, operator-sdk, client-go patterns) is the golden path" as a pro of keeping Go, but then dismisses it. For a storage system targeting HPC/AI workloads:

1. **Kubernetes operator**: If kiseki ever needs a k8s operator (likely for cloud deployments), the Rust k8s ecosystem (`kube-rs`) is functional but less mature than `client-go` + `controller-runtime`.
2. **Admin CLI**: `clap` is excellent, but Go's `cobra` + `viper` + `pflag` pattern is the standard for infrastructure CLIs. Users of storage systems (HPC admins, SREs) expect Go-style CLIs.
3. **gRPC health checking**: Go has first-class support; Rust's is less standardized.

This matters because the control plane is the admin-facing surface. Admin tooling quality directly affects adoption.

Suggested resolution: Explicitly document that a k8s operator (if ever needed) will use `kube-rs`, and that the CLI will use `clap` with similar UX patterns. Accept this as a trade-off, not dismiss it.

---

## Finding: ADR-027-ADV-6 — FIPS argument is weak for control plane

Severity: **Medium**
Category: Security > Cryptographic correctness
Location: ADR-027 §Rationale, "Smaller FIPS surface"
Spec reference: ADR-001

Description: The ADR argues Rust-only gives "one aws-lc-rs FIPS module boundary." However:

1. The control plane doesn't do data-path encryption. Its FIPS needs are limited to TLS (mTLS for admin endpoints).
2. Go's BoringCrypto (via `GOEXPERIMENT=boringcrypto`) is FIPS 140-2 validated and is the standard approach for Go services needing FIPS TLS.
3. Having two FIPS modules (aws-lc-rs + BoringCrypto) is standard practice in mixed-language systems and doesn't meaningfully increase the certification surface — they cover different binaries.

The FIPS argument was valid for ADR-001 (rejecting C FFI in the data path) but is stretched thin when applied to a separate control-plane binary that only does TLS.

Suggested resolution: Remove or downweight the FIPS argument. The stronger arguments are domain-model unification, toolchain simplification, and split-context elimination.

---

## Finding: ADR-027-ADV-7 — Discovery service placement unclear

Severity: **Medium**
Category: Correctness > Implicit coupling
Location: ADR-027 §Open items
Spec reference: `module-graph.md`, `control/pkg/discovery`

Description: The ADR lists "decide the fate of `control/pkg/discovery`" as an open item. Discovery is on the data fabric (ADR-008: seed-based, no control-plane dependency). If it moves to `kiseki-control` (a management-plane binary), it violates the design that native clients discover without the control plane.

Currently `control/pkg/discovery` doesn't exist as code (placeholder in module-graph). But the architectural placement matters: discovery needs to run on storage nodes (data fabric), not control-plane nodes (management network).

Suggested resolution: Discovery belongs in `kiseki-server` (or a new `kiseki-discovery` crate linked into `kiseki-server`), not in `kiseki-control`. Resolve before accepting.

---

## Finding: ADR-027-ADV-8 — Existing Go tests use hashicorp/go-memdb for in-memory state

Severity: **Medium**
Category: Correctness > Semantic drift
Location: `control/go.mod`; `control/pkg/tenant/store.go`
Spec reference: None

Description: The Go control plane uses `hashicorp/go-memdb` (an in-memory database with radix-tree indexing, multi-table transactions, and watch channels). The Rust port will need equivalent functionality for:
- Multi-index tenant lookups (by ID, by name, by compliance tag)
- Transactional multi-table updates (create tenant + create default project atomically)
- Watch/subscribe for change notifications (federation sync)

There is no direct Rust equivalent of `go-memdb`. The port will likely use `HashMap`/`BTreeMap` with manual indexing, which loses the transaction and watch semantics.

Suggested resolution: Identify the Rust data structure strategy before porting. Options: (a) manual maps (simpler, loses transactions), (b) `sled` or similar embedded DB, (c) reuse openraft state machine pattern (already proven in the codebase).

---

## Finding: ADR-027-ADV-9 — Build time impact not assessed

Severity: **Medium**
Category: Robustness > Resource exhaustion
Location: ADR-027 §Consequences
Spec reference: None

Description: Adding `kiseki-control` and `kiseki-cli` to the Rust workspace increases compile times. The workspace already takes significant time to build (12 crates + proto generation + aws-lc-rs). The Go control plane currently compiles in ~5 seconds. Adding the control-plane logic to the Rust build graph means every `cargo test` touches more code.

Go's fast compile cycle is an advantage for control-plane iteration — admins and operators modifying policy logic get sub-second feedback.

Suggested resolution: Measure current `cargo build` time and estimate the impact. Consider whether `kiseki-control` should be a separate workspace (same repo, separate `Cargo.toml` root) to avoid polluting the data-path build.

---

## Finding: ADR-027-ADV-10 — No rollback plan

Severity: **Low**
Category: Correctness > Missing negatives
Location: ADR-027 §Migration
Spec reference: None

Description: The ADR provides no rollback strategy if the Rust control plane proves to be a worse choice (e.g., async complexity causes bugs, operator ecosystem gaps block adoption, build times become prohibitive). Since existing Go code will be deleted, reverting would mean rewriting it.

Suggested resolution: Keep the Go code in `control/` (or archive branch) for at least one release cycle after the Rust port is complete and validated.

---

## Finding: ADR-027-ADV-11 — godog Strict:true semantics differ from cucumber-rs

Severity: **Low**
Category: Correctness > Semantic drift
Location: ADR-027 §Migration step 7
Spec reference: `control/tests/acceptance/acceptance_test.go`

Description: godog's `Strict:true` fails on undefined steps. cucumber-rs has different default behavior (undefined steps are `Pending`, not failures). The port must explicitly configure cucumber-rs to match godog's strict semantics, or BDD discipline silently degrades.

Suggested resolution: Document that the cucumber-rs runner must use `--strict` or equivalent configuration to match current godog behavior.

---

## Finding: ADR-027-ADV-12 — Docker compose loses control-plane service

Severity: **Critical** (infrastructure)
Category: Robustness > Failure cascades
Location: `docker-compose.yml`, `Dockerfile.control`
Spec reference: None

Description: The existing e2e test infrastructure includes `kiseki-control` as a Docker service. ADR-027 proposes replacing the Go binary with a Rust one, but:

1. `Dockerfile.control` uses `golang:1.24` — needs complete replacement
2. `docker-compose.yml` references `kiseki-control` — needs updated build context
3. E2e tests that depend on the control plane (`test_server_health.py` references `control_addr`) will break during migration

There's no intermediate state where both old and new control planes coexist.

Suggested resolution: Plan the Docker migration explicitly. Option: build `kiseki-control` (Rust) from the same `Dockerfile.server` builder stage (it's already a Rust workspace), then update compose.

---

## Finding: ADR-027-ADV-13 — "Reuse of kiseki-common" argument has a coupling risk

Severity: **Critical**
Category: Security > Trust boundaries
Location: ADR-027 §Rationale, "Reuse of kiseki-common and kiseki-proto"
Spec reference: `module-graph.md` §Dependency rules

Description: The ADR argues that `kiseki-control` importing `kiseki-common` directly (instead of via proto) is a benefit — "validation logic written once." But this creates a subtle coupling risk:

If `kiseki-common` grows types that reference data-path internals (e.g., `ShardId` methods that assume local shard access, `Delta` types with methods that touch in-memory stores), the control plane silently gains access to data-path abstractions. Currently the proto boundary forces a clean serialization/deserialization step that prevents this.

The language wall previously meant Go code could ONLY see protobuf-generated types — no temptation to import internal Rust types. In Rust-only mode, the temptation is "just add `use kiseki_common::shard::ShardInternals`."

Suggested resolution: Define explicit visibility rules. `kiseki-common` should have `pub` types that are safe for both planes, and `pub(crate)` or feature-gated types that are data-path only. Or: split `kiseki-common` into `kiseki-types` (shared) and `kiseki-common` (data-path).

---

## Verdict

**ADR-027 has sound motivation** (domain model unification, toolchain simplification, split-context elimination) **but the decision document is based on a stale premise** and lacks concrete migration planning for the existing codebase.

### Blocking (must resolve before acceptance):
1. **ADV-1**: Rewrite to acknowledge existing Go code and real migration cost
2. **ADV-2**: Define BDD coverage preservation strategy during port
3. **ADV-3**: Demonstrate crate-graph enforcement mechanism
4. **ADV-13**: Define `kiseki-common` visibility rules to replace language wall

### Should resolve:
5. **ADV-4**: Address async complexity strategy for control-plane handlers
6. **ADV-5**: Explicitly accept operator ecosystem trade-off
7. **ADV-7**: Resolve discovery service placement
8. **ADV-12**: Plan Docker infrastructure migration

### Accept as trade-offs:
9. **ADV-6, ADV-8, ADV-9, ADV-10, ADV-11**: Acknowledged, documented, manageable
