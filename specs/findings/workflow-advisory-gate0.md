# Adversary Gate-0 — Workflow Advisory & Client Telemetry

**Target**: analyst output for ADR-020 (ubiquitous-language additions,
invariants I-WA1..I-WA15, domain-model cross-cutting concern,
`workflow-advisory.feature`, enforcement-map stubs).
**Mode**: architecture (no code exists).
**Reviewer**: adversary role, pre-architect gate.
**Date**: 2026-04-17.

Fifteen findings. None are blocking if applied; six require spec-level
fixes before architect begins interface design, the rest can be
handled in-line by the architect as constraints or deferred with
explicit follow-ups.

---

## Finding: Hint rejection can be an existence oracle
**Severity**: Critical
**Category**: Security > Trust boundaries / side-channel
**Location**: `specs/invariants.md` I-WA6 (telemetry only); `workflow-advisory.feature` "A workflow cannot cross workload boundaries"
**Spec reference**: I-WA6 explicitly covers telemetry; hint rejection is not covered.
**Description**: I-WA6 mandates constant-shape rejection for *telemetry* queries against unauthorized targets. The equivalent guarantee for *hint* rejection is absent. A hint targeting a composition the caller is not authorized for can currently return distinguishable errors ("scope_violation" vs "not_found") or distinguishable latency, allowing a client to probe for existence of cross-tenant or cross-workload compositions.
**Evidence**: scenario "A workflow cannot cross workload boundaries" returns "scope_violation" for a known neighbour composition. Without a parallel case for a non-existent composition, a caller timing the two can distinguish them.
**Suggested resolution**: extend I-WA6 (or add I-WA16) to cover every advisory request, not only telemetry. Error codes, payloads, and latency distributions for "unauthorized target" and "absent target" must be indistinguishable. Add a scenario exercising the equivalence explicitly.

## Finding: mTLS identity check is stream-level, not per-operation
**Severity**: Critical
**Category**: Security > Authentication / trust boundaries
**Location**: `specs/invariants.md` I-WA3; ADR-020 "Advisory channel" section
**Description**: I-WA3 binds a workflow to the workload identity "at DeclareWorkflow," without mandating re-validation on each hint or telemetry message on the long-lived bidi stream. Certificate revocation, credential rotation, and even TLS renegotiation during the life of a stream can leave a stream authorized under stale identity. The adjacent data path is per-request-authorized; advisory should match.
**Evidence**: the feature file shows DeclareWorkflow as the authorization moment; no scenario exercises per-message authorization.
**Suggested resolution**: tighten I-WA3 to require per-operation authorization — every hint submission, phase advance, and telemetry subscription re-validates the caller's currently-valid mTLS identity against the workflow's owning workload. Stream-level establishment is necessary but not sufficient.

## Finding: k-anonymity threshold is undefined under low-neighbour conditions
**Severity**: Critical
**Category**: Security > Tenant isolation / side-channel
**Location**: I-WA5; `workflow-advisory.feature` "Telemetry is computed over the caller's own resources only"
**Description**: I-WA5 requires k-anonymity ≥5 for cluster-level aggregate telemetry, but does not specify behaviour when the anonymity set is below the threshold (e.g., a pool with only one or two active neighbour workloads). Two failure modes: (a) emit the signal anyway → direct leak; (b) suppress the signal → presence-of-suppression itself becomes a side channel ("I got no neighbour info means I'm alone"), leaking cardinality.
**Evidence**: scenario only tests the populated case. No scenario defines low-k behaviour.
**Suggested resolution**: I-WA5 must mandate that the signal shape emitted to a caller is *identical* regardless of whether k is met, and that any neighbour-derived component is zeroed out uniformly rather than suppressed. The caller must not be able to detect whether the response contains neighbour-derived content.

## Finding: Reattach protocol referenced but not specified
**Severity**: High
**Category**: Correctness > Specification compliance
**Location**: `workflow-advisory.feature` scenario "New process gets a new client_id"
**Description**: the scenario offers two options — TTL expiry OR "explicit reattach protocol (with audit)" — but the reattach protocol is not specified anywhere in the analyst output. Either it needs a spec (with identity-binding guarantees: how does a new `client_id` inherit a prior workflow without opening impersonation?) or the option must be dropped.
**Suggested resolution**: remove the reattach option from the analyst spec. Workflows on a restarted process expire via TTL; the new process redeclares. Reattach may be revisited as a follow-up feature with its own spec + gate review.

## Finding: Prefetch hint tuple count is uncapped
**Severity**: High
**Category**: Robustness > Resource exhaustion
**Location**: `workflow-advisory.feature` scenario "Prefetch hints for shuffled epoch read order" (shows 4096 tuples); ADR-020 taxonomy
**Description**: no bound on tuples per prefetch hint. At 4096 tuples × ~40 bytes × 200 hints/sec, a single workload can push 32 MB/s at the hint handler, and a cluster with hundreds of workloads multiplies this. No per-hint or per-phase cap is specified.
**Suggested resolution**: add `max_prefetch_tuples_per_hint` to the budget (default 4096, max 16384, enforced server-side with "hint_too_large" rejection). Add a scenario covering the cap.

## Finding: Audit event storm risk from high-rate rejected hints
**Severity**: High
**Category**: Robustness > Resource exhaustion / observability
**Location**: I-WA8; enforcement-map row for I-WA8
**Description**: I-WA8 audits every hint accept/reject/throttle decision. A workload at its full 200 hints/sec with 100% rejection rate produces 200 tenant-audit events/sec. Multiply across workloads and the tenant audit shard faces a new volume class not previously anticipated. Interacts badly with I-L4 / I-A4 (audit is a GC consumer) even though I-A5 provides a safety valve.
**Suggested resolution**: allow the architect to implement batching or sampling of throttled-hint audit events (preserving at least one event per unique rejection reason per workflow per second, plus exact counts). Declare-workflow, end-workflow, phase-advance, and policy-violation rejections remain per-event. Tighten I-WA8 to reflect this.

## Finding: Workflow declare rate is uncapped
**Severity**: High
**Category**: Robustness > Resource exhaustion
**Location**: background block of `workflow-advisory.feature`
**Description**: the budget includes `concurrent_workflows` but no `workflow_declares_per_sec`. A client that rapid-fires Declare + End within the concurrency cap can churn advisory state indefinitely, generating audit load and stressing the id generator.
**Suggested resolution**: add `workflow_declares_per_sec` to the budget (default ~10/s) and add a scenario for the cap.

## Finding: Prefetch hints against log/delta positions have no explicit scope constraint
**Severity**: High
**Category**: Security > Tenant isolation
**Location**: ADR-020 hint taxonomy; I-WA11
**Description**: the taxonomy lists prefetch hints as `(composition_id, offset, length)`, which is fine (composition is tenant-scoped). But the analyst does not explicitly forbid a future prefetch variant that references shard ID + log position, which would be a direct cross-tenant probe surface. Need to close this forward.
**Suggested resolution**: tighten I-WA11 to mandate that all hint target fields are either (a) caller-owned opaque references (composition_id, view_id) validated for caller ownership, or (b) enum-bucketed classification values. Shard IDs, log positions, chunk IDs, node IDs, device IDs, and dedup hashes are explicitly forbidden as hint target fields.

## Finding: Annotation-on-data-path semantics not pinned
**Severity**: Medium
**Category**: Correctness > Implicit coupling
**Location**: ADR-020 "Advisory channel" section
**Description**: ADR says "data-path requests may be annotated with a short `workflow_ref` header." Unspecified: what happens if (a) the header is malformed, (b) the referenced workflow has expired, (c) the advisory subsystem is unavailable when the correlation would normally happen. I-WA2 is clear that the data path must not block, but the exact degradation of correlation is not.
**Suggested resolution**: add to ADR-020 that `workflow_ref` is a best-effort annotation: malformed → ignored, expired → dropped silently on the advisory side with an audit event, unavailable → annotation enqueued with bounded buffer then dropped. Data-path operation outcome is always unchanged.

## Finding: Concurrent PhaseAdvance semantics undefined
**Severity**: Medium
**Category**: Concurrency > Interleaved conflicts
**Location**: I-WA13; PhaseAdvance scenarios
**Description**: I-WA13 requires at most one active phase at an instant, but with multi-threaded clients (or rare client_id collisions) two PhaseAdvance calls can race. The linearization point is not specified.
**Suggested resolution**: specify that the advisory subsystem serializes PhaseAdvance per workflow (compare-and-swap on `phase_id`) and that a losing caller sees `phase_not_monotonic`. Add a scenario.

## Finding: In-flight hints at EndWorkflow are ambiguous
**Severity**: Medium
**Category**: Correctness > Failure cascades
**Location**: scenario "Workflow ends on explicit End"
**Description**: the scenario says "all subscribed telemetry streams for the workflow are closed" but does not specify the fate of hints already buffered or in-flight at End. Can in-flight hints have effects after End returns? Are they rejected with `workflow_ended` or silently dropped?
**Suggested resolution**: specify that EndWorkflow draws a line: hints submitted strictly before End are best-effort; hints submitted after End return `workflow_unknown`. Add a scenario.

## Finding: Opt-out transition semantics unspecified
**Severity**: Medium
**Category**: Correctness > Failure cascades
**Location**: I-WA12; "Tenant admin disables advisory for a workload" scenario
**Description**: "gracefully ended with audit" is hand-wavy. Are in-flight prefetch warms cancelled? Do already-accepted hints for the active phase continue to be honoured until the phase ends? What about cluster-wide disable during a checkpoint?
**Suggested resolution**: define three explicit states: enabled, draining (no new declares, existing workflows continue until current-phase end or TTL), disabled (all hint activity rejected, existing subscriptions closed, data path fully unaffected). Opt-out transitions enabled → draining → disabled. Add scenarios.

## Finding: client_id construction uses HMAC with undefined key
**Severity**: Medium
**Category**: Security > Cryptographic correctness
**Location**: enforcement-map row for I-WA4
**Description**: "client_id = HMAC(mTLS fingerprint, per-process startup nonce)" — HMAC requires a secret key. Neither operand is secret (the fingerprint is public; the nonce is process-local but not authenticated). This is a construction mistake.
**Suggested resolution**: simplify to `client_id = random(≥128 bits)` generated by the native client at startup. Pinning is enforced by the advisory registrar: registration binds (client_id, mTLS identity), and subsequent requests must present both. Update enforcement-map.

## Finding: Runtime revocation of profile/policy undefined
**Severity**: Medium
**Category**: Correctness > Missing negatives
**Location**: I-WA12 and elsewhere
**Description**: the spec defines DeclareWorkflow-time policy checks but not mid-workflow revocation. If `ai-training` is removed from the allow-list while a workflow is using it, does the workflow continue under the old profile? What about budget reductions?
**Suggested resolution**: specify that existing workflows run to completion (or TTL) under the policy that was effective at DeclareWorkflow. New phase advances re-validate; any phase advance under a revoked profile is rejected with `profile_revoked`. Budget reductions apply prospectively from the next second. Add a scenario.

## Finding: No failure-mode or assumption entries filed
**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: `specs/failure-modes.md`, `specs/assumptions.md`
**Description**: ADR-020 references `F-ADV-1` but the failure-modes catalogue does not contain it. The analyst spec also makes implicit assumptions (k=5 anonymity sufficient; 1h default TTL appropriate; 64-phase history adequate for HPC/AI; clients do not gain value from lying about profile) that are not recorded in `assumptions.md`.
**Suggested resolution**: add F-ADV-1 (advisory subsystem outage, P2) and F-ADV-2 (advisory audit storm, P2) to failure-modes.md. Add four to six entries to assumptions.md marked **Accepted** with risk notes.

---

## Summary

| Severity | Count | Disposition |
|---|---|---|
| Critical | 3 | Fix in analyst backpass before architect |
| High | 4 | Fix in analyst backpass before architect |
| Medium | 6 | Fix in analyst backpass; otherwise architect inherits as constraints |
| Low | 2 | Noted in ADR follow-ups (already captured) |

**Blocks architect phase until**: critical and high findings are applied to
the specs and re-reviewed.

**Out-of-scope for this gate** (architect/later-adversary turf): concrete
k-anonymity algorithm and bucketing parameters; latency-bucket widths;
phase-history compaction format; protobuf schema review; runtime
integrity checks on the advisory subsystem itself; covert-channel
analysis once code exists.
