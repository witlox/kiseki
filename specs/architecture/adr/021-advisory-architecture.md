# ADR-021: Workflow Advisory Architecture

**Status**: Proposed (architect phase — awaits adversary gate-1 review before implementation)
**Date**: 2026-04-17
**Context**: ADR-020 analyst-level decision; this ADR commits the architecture (crate shape, runtime isolation, advisory-to-data-path coupling, protobuf + intra-Rust boundaries).

## Decision

Three structural commitments that, together, make the analyst-level
invariants in ADR-020 enforceable at compile time and at runtime.

### 1. Advisory is a separate crate with an isolated runtime

- New Rust crate `kiseki-advisory`, located at `crates/kiseki-advisory/`.
- Compiled into `kiseki-server` but runs on a **dedicated tokio
  runtime** with its own thread pool, separate from the data-path
  runtime. Configured via `kiseki-server` at process start.
- All advisory ingress (`AdvisoryStream`, `DeclareWorkflow`,
  `PhaseAdvance`, telemetry subscriptions) is accepted on a separate
  gRPC listener from the data-path gRPC listeners.
- Advisory-audit emission uses `kiseki-audit`'s existing tenant-shard
  path but with its own bounded queue and drop-and-record-on-overflow
  policy (no awaits out of the advisory runtime into the data path).
- **Structural enforcement of I-WA2**: data-path crates do not depend
  on `kiseki-advisory` in their Cargo manifests. The only way an
  advisory event can affect data-path behaviour is through
  well-typed domain-level preferences (see §3), which the data path
  treats as advisory hints — never as preconditions.

### 2. Shared domain types live in `kiseki-common`

A small set of enums and structs representing "the advisory context
of one operation" is declared in `kiseki-common` (already a dependency
of every context). This lets data-path crates accept an
`Option<&OperationAdvisory>` on their operations without pulling in
the advisory runtime.

```text
kiseki-common        (domain types: WorkflowRef, OperationAdvisory, enums)
  ↑
kiseki-{log,chunk,composition,view,gateway-*,client}
  (accept Option<&OperationAdvisory>, use for preferences only)

kiseki-advisory      (runtime, router, budget, audit emitter)
  ├── depends on kiseki-common
  ├── depends on kiseki-audit
  └── depends on kiseki-proto (for WorkflowAdvisoryService)
  ↑
kiseki-server        (wires advisory runtime to each context)
```

**Cycle-free**: no data-path crate depends on `kiseki-advisory`; the
runtime wiring happens only in the `kiseki-server` binary.

### 3. Pull-based advisory lookup (not push into the data path)

When a data-path request arrives carrying a `workflow_ref` header:

### 3.a Header mechanism

The `workflow_ref` is carried as a **gRPC metadata entry**, not as a
protobuf field on any data-path message. Concrete binding:

- Metadata key: `x-kiseki-workflow-ref-bin` (binary metadata, per
  gRPC convention for raw-bytes values)
- Metadata value: the raw 16-byte `WorkflowRef` handle
- All data-path protos remain **unchanged** — this is the
  structural payoff that makes I-WA2 tractable (data-path code
  stays advisory-unaware).
- A gRPC interceptor in `kiseki-server` lifts the header into a
  request-scoped context at ingress. The context is accessed by
  each data-path handler through a small `kiseki-common` helper
  (`CurrentAdvisory::from_request_context()`), which returns an
  `Option<OperationAdvisory>` by calling `AdvisoryLookup::lookup_fast`.
- For intra-Rust calls (e.g., native client's native API path),
  the same helper reads from a task-local set by the caller. The
  native client's `WorkflowSession` handle scopes this automatically.
- For external protocols (NFS, S3) the HTTP-level header is
  `x-kiseki-workflow-ref` (plain, hex-encoded), translated by the
  protocol gateway into the gRPC binary metadata entry
  `x-kiseki-workflow-ref-bin` before forwarding to any internal
  gRPC service. This keeps external clients unaware of gRPC
  conventions.
- No data-path proto file contains `workflow_ref`. Any future
  attempt to add it is rejected at architecture review.

1. The `kiseki-server` gRPC interceptor extracts `workflow_ref` and
   stores it in the request context.
2. The data-path operation (e.g., `WriteChunk`) optionally consults
   `CurrentAdvisory::from_request_context()` to obtain an
   `Option<OperationAdvisory>`.
3. The data-path code may, synchronously and fallibly, call
   `AdvisoryLookup::lookup_fast(workflow_ref) -> Option<OperationAdvisory>`
   with a strict bounded deadline (≤ 500 µs, configurable, default
   200 µs). The method name carries the contract: implementations
   MUST NOT block, allocate on the happy path, or call non-O(1)
   functions.
3. On timeout, unavailability, or cache miss the lookup returns
   `None`. The data-path code proceeds exactly as it would for an
   operation without any `workflow_ref`.
4. There is no blocking wait, no retry, and no propagated error.
   The lookup is a hot-path cache read (see §4 below).

This guarantees **I-WA2** structurally: the data path cannot be
stalled or corrupted by the advisory subsystem. At worst, advisory
context is unavailable and steering quality degrades.

### 4. Advisory state shape and hot path

`kiseki-advisory` maintains three bounded in-memory caches keyed by
workflow:

| Cache | Contents | Size bound | Eviction |
|---|---|---|---|
| **Workflow table** | `(workflow_id) → { mTLS-identity, profile, current_phase, budgets, TTL }` | policy-bounded max concurrent workflows per workload × total workloads | TTL + End |
| **Effective-hints table** | `(workflow_id) → OperationAdvisory` (latest accepted hints, merged across phase) | 1 row per active workflow | replaced on new accept |
| **Prefetch ring** | per-workflow ring buffer of accepted prefetch tuples | `max_prefetch_tuples_per_hint × in-flight phases` | FIFO on cap |

Reads from the data path hit the effective-hints table (O(1)).
Writes into these caches happen on the advisory runtime only.
Cross-thread access uses `arc-swap` (snapshot-read, copy-on-write)
so the data-path read never takes a lock held by the advisory
runtime.

### 5. gRPC service shape

One new service, `WorkflowAdvisoryService`, on its own gRPC listener.
Unary: `DeclareWorkflow`, `EndWorkflow`, `PhaseAdvance`,
`GetWorkflowStatus` (for admin/debug within caller's own scope).
Bidi streaming: `AdvisoryStream` (hints in, telemetry out over the
same stream, multiplexed). Unary: `SubscribeTelemetry` (server-stream
variant for callers who don't want to send hints).

Full schema in `specs/architecture/proto/kiseki/v1/advisory.proto`.

### 6. Control-plane integration

New Go package `control/pkg/advisory`:
- Policy CRUD for profile allow-lists, budgets, opt-out state per
  org/project/workload. Inheritance computed server-side; effective
  policy returned to `kiseki-advisory` via existing `ControlService`.
- Opt-out state transitions (`enabled`/`draining`/`disabled`) are
  Raft-backed in the existing control-plane state store.
- Federation does NOT replicate workflow state (ephemeral, local).
  It DOES replicate policy (existing async config replication path).

### 7. k-anonymity bucketing: concrete algorithm

For pool/shard saturation signals that incorporate cross-workload
aggregate:

1. Compute aggregate metric `A` over all contributing workloads on
   the pool/shard.
2. Count distinct contributing workloads `k`.
3. If `k ≥ 5` (policy-configurable minimum): return
   `severity = bucket(A)`; retry-after = `bucket(compute_retry(A))`.
4. If `k < 5`: return `severity = bucket(A_caller_only)`;
   retry-after = `bucket(compute_retry(A_caller_only))`. The
   response **shape is identical** to the `k≥5` case; only the
   value of the neighbour-derived component is replaced by a
   sentinel bucket (`ok`, regardless of true aggregate) chosen to
   minimize caller utility of detecting the substitution.

Bucket function: fixed set `{ok, soft, hard}` for severity,
`{<50ms, 50-250ms, 250-1000ms, 1-10s, >10s}` for retry-after.

### 8. Covert-channel hardening: concrete widths

- **Rejection response timing**: every advisory rejection path (hint,
  subscription, declare, phase) pads response emission to the next
  100-µs boundary *after* a fixed minimum of 300 µs. Enforced by a
  common `emit_bucketed_response` helper in `kiseki-advisory`.
- **Telemetry message sizes**: protobuf messages padded to one of
  `{128, 256, 512, 1024}` bytes with a `reserved bytes padding` field
  repeated to the target size. Selection uses the nearest bucket
  ≥ actual size.
- **Error codes**: every rejection caused by authorization or
  scope violation returns the `SCOPE_NOT_FOUND` code with the same
  message payload, regardless of whether the cause was "unauthorized"
  or "absent". Internal audit records carry the true reason.
- **gRPC status code**: `WorkflowAdvisoryService` MUST return gRPC
  status `NOT_FOUND` (code 5) for every `SCOPE_NOT_FOUND` case. Using
  `PERMISSION_DENIED` (code 7) or `UNAUTHENTICATED` (code 16) on
  authorization failures would leak the distinction via the gRPC
  trailers, defeating the canonicalization above. All gRPC clients
  and middleware expose the status code, so this is not a
  "docs-only" rule — it is enforced by an integration test at
  Phase 11.5 exit that compares status-code distributions across
  authorized-absent and unauthorized-existing cases.

### 9. Phase-history compaction format

Per workflow, keep the last 64 phase records in the workflow table
(ring buffer of `PhaseRecord { phase_id, tag_hash, entered_at,
hints_accepted_count, hints_rejected_count }`). On eviction, the
evicted record is rolled up into a per-workflow
`PhaseSummary { from_phase_id, to_phase_id, total_hints_accepted,
total_hints_rejected, duration_ms }` audit event emitted to the
tenant audit shard. The summary replaces all evicted individual
records in audit history.

## Alternatives considered

1. **Put advisory code inside each data-path crate behind a feature
   flag.**
   Rejected: tight coupling; impossible to guarantee I-WA2 (hot
   data-path code lives in the advisory lifecycle), and per-crate
   feature flags multiply combinatorics of build variants.

2. **Separate OS process for advisory runtime, IPC'd from
   kiseki-server.**
   Rejected: IPC adds serialization cost on the hot-path lookup
   (§3) and complicates deployment (another process per node). The
   isolated-tokio-runtime pattern gives enough blast-radius
   reduction at much lower overhead.

3. **Define advisory traits in a new tiny crate `kiseki-advisory-api`
   separate from `kiseki-common`.**
   Considered. Rejected for now: the advisory domain types
   (`OperationAdvisory`, enums) are small, stable, and already
   conceptually part of the shared vocabulary (Workflow, Phase,
   AccessPattern appear in ubiquitous-language.md). Adding a
   one-concept crate adds build-graph overhead without payoff. Can
   be split out later if the type set grows.

4. **Push hints directly into each context via per-context channels
   (no `OperationAdvisory` aggregation).**
   Rejected: spreads fan-out logic across every context and makes
   I-WA11 (target-field restriction) and I-WA16 (size cap)
   harder to enforce. Centralizing in `kiseki-advisory` and passing
   an already-validated bundle simplifies data-path code.

### 10. Schema versioning

`advisory.proto` ships as `kiseki.v1`. Forward-evolution rules:

- **Additions** (new fields, new oneof variants, new enum values)
  stay within `v1`. Unknown fields are preserved by gRPC clients.
- **Deprecations** mark fields with `reserved` after one minor
  release; old clients continue to work.
- **Breaking changes** (semantic change of a field, required
  removal) move to `v2` with a deprecation window ≥ 2 releases in
  which both versions are served.
- Advisory-policy changes in the control plane (profile allow-list
  additions, budget changes) are config, not schema — no version
  bump needed.

### 11. Padding to bucket size

`AdvisoryError.padding`, `AdvisoryServerMessage.padding`,
`TelemetryEvent.padding`, `WorkflowStatus.padding`, and
`AdvisoryAuditBody.padding` carry the variable bytes needed to hit
one of the bucket sizes {128, 256, 512, 1024, 2048 for audit bodies}.
Computation at emit time:

```
serialized_size = serialize(rest_of_message).len();
target_bucket   = smallest bucket >= serialized_size + padding_overhead;
padding_len     = target_bucket - serialized_size - varint_overhead(target_bucket);
```

`varint_overhead(N)` accounts for the two-byte (tag + length-varint)
prefix of the padding field; standard protobuf wire format.
Implementations MUST use the `kiseki-advisory::emit_bucketed_response`
helper. Property test at Phase 11.5 exit: every response on
`WorkflowAdvisoryService` is exactly one of the bucket sizes.

## Consequences

- Adds one Rust crate (`kiseki-advisory`), one Go package
  (`control/pkg/advisory`), one proto file
  (`proto/kiseki/v1/advisory.proto`), one data-model stub
  (`data-models/advisory.rs`).
- Adds a new phase to the build sequence (see `build-phases.md`).
- Every data-path `*Ops` trait in `api-contracts.md` gains an
  optional `advisory: Option<&OperationAdvisory>` parameter on its
  methods. Callers that don't care pass `None`.
- Isolation requires `kiseki-server` to instantiate two tokio
  runtimes. Accepted cost.
- The `arc-swap` hot-path read is the only cross-runtime coupling.
  Property-test and benchmark-verified at Phase 11 exit.

## Open items (escalated to adversary gate-1)

- Validate that §3 (pull-based lookup) cannot itself become a DoS
  surface: a malicious client pummelling `workflow_ref` headers
  causes lookups. Mitigation: lookup cache is per-node, bounded,
  and miss cost is a `None` return (no upstream RPC).
- Validate §4 (`arc-swap` snapshot) meets latency targets on the
  actual data-path hot code (FUSE read/write, chunk write, view
  read).
- Validate §8 covert-channel widths are large enough to mask actual
  work variance under realistic load.
- Confirm §9 audit summary compaction does not itself become an
  existence oracle (size of summary varies with workflow activity).
