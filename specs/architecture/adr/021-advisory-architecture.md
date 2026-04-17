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

1. The data-path RPC handler extracts `workflow_ref` and passes it
   to the data-path operation as part of `OperationAdvisory`.
2. The data-path code may, synchronously and fallibly, call
   `AdvisoryOps::lookup(workflow_ref) -> Option<OperationAdvisory>`
   with a strict bounded deadline (≤ 500 µs, configurable, default
   200 µs).
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
