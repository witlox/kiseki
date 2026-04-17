# Adversary Gate-1 — Workflow Advisory architecture

**Target**: architect output for ADR-021 (advisory-architecture),
`advisory.proto`, `data-models/advisory.rs`, enforcement-map +
api-contracts + error-taxonomy + build-phases + module-graph updates.
**Mode**: architecture (no code exists).
**Reviewer**: adversary role, pre-implementer gate.
**Date**: 2026-04-17.

Sixteen findings. Two critical, five high, six medium, three low.
Critical and high must be applied before implementer begins; the rest
may be carried into implementation as constraints.

---

## Finding: `AffinityPoolId` in `OperationAdvisory` creates a crate cycle
**Severity**: Critical
**Category**: Correctness > Implicit coupling / dependency graph
**Location**: `specs/architecture/data-models/advisory.rs:21,84,219`; `module-graph.md` dep rules
**Spec reference**: ADR-021 §2 (shared types in `kiseki-common`, no data-path→advisory edges)
**Description**: `advisory.rs` imports `AffinityPoolId` from `kiseki-chunk`. The architect committed to placing `OperationAdvisory` (and its fields) in `kiseki-common`, but `kiseki-chunk` *depends on* `kiseki-common`. If `kiseki-common` defines `OperationAdvisory` referencing `AffinityPoolId` defined in `kiseki-chunk`, the Cargo graph cycles. The same issue hits `TelemetryEvent::Backpressure { pool: AffinityPoolId, ... }`.
**Evidence**: `chunk.rs:36` defines `pub struct AffinityPoolId(pub uuid::Uuid)`; `advisory.rs:21` has `use crate::chunk::AffinityPoolId`; `module-graph.md` lists `kiseki-chunk` depending on `kiseki-common`.
**Suggested resolution**: move `AffinityPoolId` (plus any other cross-context opaque identifier consumed by advisory) into `kiseki-common`. Chunk continues to use it; no code change in `kiseki-chunk`. Alternatively, introduce a tenant-scoped opaque `PoolHandle` in `kiseki-common` used by the data model and resolve to `AffinityPoolId` inside `kiseki-advisory`.

## Finding: gRPC status code is a covert channel bypassing `SCOPE_NOT_FOUND`
**Severity**: Critical
**Category**: Security > Tenant isolation / covert channel
**Location**: `specs/architecture/error-taxonomy.md` kiseki-advisory section; ADR-021 §8; `advisory.proto` `AdvisoryErrorCode`
**Spec reference**: I-WA6, I-WA15, ADR-021 §8
**Description**: The architect unified the *application-level* error code on `ScopeNotFound` and fixed the message string. But gRPC carries a separate **status code** on every response (`OK`, `NOT_FOUND`, `PERMISSION_DENIED`, `UNAUTHENTICATED`, `UNAVAILABLE`, etc.). If the implementer follows gRPC convention (`PERMISSION_DENIED` for unauthorized, `NOT_FOUND` for absent), the status code distinguishes the two cases and the entire §8 canonicalization is defeated. Middleware, proxies, and client libraries expose the status code directly.
**Evidence**: neither ADR-021 §8 nor error-taxonomy specifies the gRPC status code mapping.
**Suggested resolution**: mandate that every scope-violation-or-absent response on `WorkflowAdvisoryService` carries gRPC status `NOT_FOUND` (code 5) regardless of underlying cause. Extend this guarantee to `AdvisoryError.code == SCOPE_NOT_FOUND`. Forbid `PERMISSION_DENIED` on this service for authorization failures. Add an integration test at Phase 11.5 exit that compares gRPC status code distributions. Document this in `error-taxonomy.md`.

## Finding: `PoolHandle` minting and lifecycle unspecified
**Severity**: High
**Category**: Correctness > Specification compliance
**Location**: `advisory.proto:218-224`; ADR-021
**Description**: `PoolHandle` is declared as "caller's tenant-scoped reference to an affinity pool, NOT the cluster-internal pool ID." But nowhere is it defined *how* a caller obtains one, *when* it expires, or *what happens* when a pool is decommissioned while a caller holds a stale handle. Without this, clients cannot actually use affinity hints, and the translation layer in `kiseki-advisory` has no contract.
**Suggested resolution**: specify that `PoolHandle`s are minted by `kiseki-advisory` at `DeclareWorkflow` time based on the workload's authorized pools and returned on `DeclareWorkflowResponse` in a new field `available_pools: repeated PoolDescriptor { handle, opaque_label }`. Handles are valid for the lifetime of the workflow. Pool decommission during workflow = handle becomes `SCOPE_NOT_FOUND` on use. Add scenarios to `workflow-advisory.feature` and `control-plane.feature`.

## Finding: Data-path header mechanism for `workflow_ref` is not specified
**Severity**: High
**Category**: Correctness > Implicit coupling
**Location**: ADR-021 §3; `api-contracts.md` OperationAdvisory table
**Description**: The architect commits "data-path RPC handler extracts `workflow_ref` and passes it to the data-path operation" — but does not commit to **how** the ref is carried. Is it a gRPC metadata header? A proto-level field on every data-path request? A per-crate decision? Without a commitment, every data-path proto needs ad-hoc retrofitting, and the I-WA2 isolation claim (data-path unaware of advisory) is contradicted if every proto file gains an advisory field.
**Suggested resolution**: specify **gRPC metadata header** `x-kiseki-workflow-ref` (binary-valued, 16 bytes). Data-path protos remain unchanged. The `kiseki-server` gRPC interceptor lifts the header into a request-scoped context and the `OperationAdvisory::workflow_ref` field is populated by the context, not the protobuf body. For the native client (intra-Rust calls), the context is passed via a thread-local or explicit argument. Document in ADR-021 and `api-contracts.md`; add a row in `enforcement-map.md`.

## Finding: Response messages with both success and error fields are schema-ambiguous
**Severity**: High
**Category**: Correctness > Missing negatives
**Location**: `advisory.proto` `DeclareWorkflowResponse`, `EndWorkflowResponse`, `PhaseAdvanceResponse`, `GetWorkflowStatusResponse`
**Description**: Each response has both a success payload (e.g., `workflow_ref`) and an `error` field side-by-side, not in a `oneof`. This admits states where both are set (undefined behaviour) or both are unset (client cannot tell). Implementers will write ad-hoc rules.
**Suggested resolution**: change each response to a `oneof outcome { success_payload, AdvisoryError error }`. For messages where the success payload is empty (End, PhaseAdvance), use `oneof { Empty ok; AdvisoryError error }`.

## Finding: Advisory audit event schema is unspecified at architect level
**Severity**: High
**Category**: Correctness > Specification compliance
**Location**: ADR-021 §9 (`PhaseSummary` mentioned); `proto/kiseki/v1/audit.proto`; `api-contracts.md`
**Description**: I-WA8 requires every advisory lifecycle event and policy-violation rejection to produce an audit event on the tenant audit shard with the correlation `(org, project, workload, client_id, workflow_id, phase_id, event_type, reason)`. The architect mentions `PhaseSummary` and the batched aggregate events but never commits to a proto message schema for these audit events. Without it, the audit export (I-A2) and cluster-admin anonymized view (I-A3) have no structural contract.
**Suggested resolution**: add a `AdvisoryAuditEvent` message family to `audit.proto` (or a new `advisory_audit.proto`) with variants for `DeclareWorkflow`, `EndWorkflow`, `PhaseAdvance`, `HintAccepted` (batched aggregate), `HintRejected`, `HintThrottled` (batched aggregate), `TelemetrySubscribed`, `BudgetExceeded`, `AdvisoryStateTransition`, `PhaseSummary`. Each carries the correlation. Hash the `workflow_id` and `phase_tag` for cluster-admin export (done at export time, not emission time).

## Finding: `AdvisoryLookup` trait does not express its deadline contract in the type system
**Severity**: High
**Category**: Correctness > Specification compliance
**Location**: `data-models/advisory.rs:182-191` (`AdvisoryLookup::lookup`)
**Description**: The bounded-deadline (≤500 µs, default 200 µs) and non-blocking requirements are stated in the doc-comment but not expressible on `fn lookup(&self, workflow_ref: &WorkflowRef) -> Option<OperationAdvisory>`. A correct implementation using a sync mutex or IO will compile and pass unit tests, then violate I-WA2 in production.
**Suggested resolution**: rename the method to `lookup_fast(..)` and add a companion `fn lookup_budget_ns() -> u64` returning the cache's committed upper bound. In the implementing crate, `lookup_fast` MUST NOT block, allocate, or call any function not known-O(1); property-tested at Phase 11.5 exit. Add an enforcement-map row that names the property test.

## Finding: Rack hint values are an undocumented side channel
**Severity**: Medium
**Category**: Security > Trust boundaries
**Location**: `advisory.proto:218-220` `AffinityHint.rack_hint`
**Description**: `rack_hint` is a `string` "opaque to caller, validated server-side." But the caller must supply a value, which means the caller somehow knows a valid rack label. Where? If rack labels are cluster-internal identifiers that become known to callers (via logs, discovery, etc.), they form a cross-tenant map. If they are tenant-scoped opaque tokens, the minting needs to be defined (same issue as `PoolHandle`).
**Suggested resolution**: drop `rack_hint` from the initial shipping scope of v1. Affinity preference is expressed at the `PoolHandle` granularity; rack-level colocation is deferred to a follow-up. Update ADR-021 §5 hint taxonomy.

## Finding: Two subscription paths create ambiguous semantics
**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: `advisory.proto` `SubscribeTelemetry` (server-stream) and `AdvisoryStream` (bidi) with `TelemetrySubscriptionOp` inside
**Description**: A caller can either open `AdvisoryStream` and send `TelemetrySubscriptionOp` messages, or call `SubscribeTelemetry` as a server-stream. If a caller does both (or one then the other), event ordering, dedup, and lifecycle become implementation-defined.
**Suggested resolution**: pick one canonical path. Recommendation: keep `SubscribeTelemetry` for telemetry-only clients and remove the `TelemetrySubscriptionOp` variant from `AdvisoryClientMessage` (telemetry events still flow on the `AdvisoryStream` server→client direction, just not as subscription ops). Document that the two channels are independent and a caller subscribed via both receives the same event twice (or spec dedup).

## Finding: Per-message `WorkflowCorrelation` on `AdvisoryStream` is either redundant or enforcement
**Severity**: Medium
**Category**: Correctness > Implicit coupling
**Location**: `advisory.proto` `AdvisoryClientMessage.correlation`
**Description**: One stream = one workflow. Re-sending correlation on every message is either defense-in-depth (catch protocol bugs) or redundant bytes. The architect didn't state which. If it is defense-in-depth, the server must *reject* messages whose correlation doesn't match the stream-established workflow — spec this and add a scenario. If it is redundant, remove the field.
**Suggested resolution**: keep the field and require exact-match validation against the stream-established workflow; add to the enforcement-map row for I-WA3.

## Finding: Telemetry subscription lifecycle across policy changes is undefined
**Severity**: Medium
**Category**: Correctness > Failure cascades
**Location**: I-WA18 (prospective policy); `advisory.proto` `SubscribeTelemetry`
**Description**: A caller subscribes to backpressure telemetry on `PoolHandle X`. Then policy narrows and the workload is no longer authorized for the pool underlying X. What happens to the subscription — closed with `SCOPE_NOT_FOUND`? Silently stops emitting? Events keep flowing from the already-accepted subscription?
**Suggested resolution**: on policy narrowing, evaluate each active subscription against the new policy; emit a terminal `StreamWarning { kind: SUBSCRIPTION_REVOKED }` and close the subscription. Data-path access to that pool is revoked by the data-path authorization path independently. Add a scenario to `workflow-advisory.feature`.

## Finding: `HintAck` timing vs audit batching relationship undocumented
**Severity**: Medium
**Category**: Correctness > Semantic drift
**Location**: I-WA8 batching rules; `advisory.proto` `HintAck`
**Description**: I-WA8 allows audit batching for `hint-accepted` and `hint-throttled` events. `HintAck` messages go per-hint. Implementers might infer "if audit is batched, ack is too." But the caller needs per-hint acks for flow control and correlation via `hint_id`. Clarify: acks are always per-hint-immediate; audit is the only thing batched.
**Suggested resolution**: add a note in `advisory.proto` near `HintAck` and in ADR-021 §9: "`HintAck` is emitted per hint. Audit-event batching applies only to the audit emission pipeline, not to client-visible acks." Keep I-WA8's batching note scoped to audit.

## Finding: `PhaseSummary` emission size could itself become an existence oracle
**Severity**: Medium
**Category**: Security > Side channel
**Location**: ADR-021 §9
**Description**: When the phase ring evicts a record, a `PhaseSummary` is emitted to the tenant audit shard. The summary's numeric fields (`total_hints_accepted`, `total_hints_rejected`) vary with workflow activity. An auditor (cluster admin with approved tenant access) watching emission timing or size patterns may infer neighbour workflow behavior. The tenant audit shard is the tenant's own, but cluster-admin summaries of audit shard load (I-A3) aggregate across tenants.
**Suggested resolution**: emit `PhaseSummary` at a fixed-size bucketed message (similar to §8 padding for telemetry) and pad the numeric counts to the nearest log2 bucket. Document the bucket scheme in ADR-021 §9 or in `audit.proto`.

## Finding: Server-side heartbeat on `AdvisoryStream` is undefined
**Severity**: Low
**Category**: Robustness > Error handling quality
**Location**: `advisory.proto` `AdvisoryClientMessage.Heartbeat`
**Description**: Client heartbeats keep the stream alive on the server side. But if the server side is alive but the advisory runtime is stuck (e.g., CPU starvation), the client has no way to detect it. Symmetric heartbeats or a periodic `StreamWarning { kind: HEARTBEAT }` from server would help.
**Suggested resolution**: add `StreamWarning::KIND_HEARTBEAT` and require the server to emit one every 10s of idleness on a stream. Clients missing three consecutive heartbeats should reconnect.

## Finding: advisory.proto schema-versioning policy not stated
**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: `advisory.proto` package `kiseki.v1`
**Description**: This ships as `v1` from day one. Protobuf allows field additions, but breaking changes (enum value rename, field removal) would require `v2`. Advisory is cross-cutting and frequently-evolved; commit to a rule so future adversaries can hold.
**Suggested resolution**: adopt the project-wide rule (already implicit elsewhere): additions are `v1.N`; deprecations are `reserved`; breaking changes go to `v2` with a migration window. Note in ADR-021.

## Finding: Padding-to-bucket mechanism not fully specified
**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: ADR-021 §8; `advisory.proto` every `padding` field
**Description**: Protobuf `bytes padding = 15` can carry 0..N bytes; the varint length prefix means total wire size is `tag(1B) + len_varint + content`. For fixed-size buckets {128, 256, 512, 1024}, the padding length must be computed after serializing the rest of the message, which is non-trivial (mutating the padding can change the varint length and thus total size). Implementers will get it wrong.
**Suggested resolution**: add to ADR-021 §8: "padding is computed as `target_bucket - serialized_size_of_all_other_fields - varint_overhead(target_bucket)`; implementations must round up to the next bucket when the message body already exceeds the smallest bucket. Helper in `kiseki-advisory::emit_bucketed_response`." Property-test: every response on `WorkflowAdvisoryService` is exactly one of the bucket sizes.

---

## Summary

| Severity | Count | Disposition |
|---|---|---|
| Critical | 2 | Must apply before implementer |
| High | 5 | Must apply before implementer |
| Medium | 6 | Apply now where cheap; others become Phase 11.5 constraints |
| Low | 3 | Noted as implementation constraints |

**Blocks implementer phase until**: all Critical and High findings are
applied to the architect artifacts (ADR-021, proto, data-model,
api-contracts, error-taxonomy, enforcement-map, module-graph) and
re-reviewed.

**Already in-spec, not raised here**: data-path hot-path latency
validation, arc-swap benchmark, covert-channel width under realistic
load — these remain as Phase 11.5 exit tests (already captured in
`build-phases.md`).
