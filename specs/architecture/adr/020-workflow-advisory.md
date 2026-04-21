# ADR-020: Workflow Advisory & Client Telemetry

**Status**: Accepted (implemented, 51/51 BDD scenarios pass)
**Date**: 2026-04-17
**Context**: new capability — HPC/AI workloads need to steer storage (prefetch, affinity, priority, phase-adaptive tuning) and consume caller-scoped feedback (backpressure, locality, materialization lag, QoS headroom). ADR-015 covers operator-facing observability; this ADR covers the orthogonal client-facing advisory/telemetry surface.

## Decision (analyst-level; architect will refine interfaces)

Introduce a **Workflow Advisory** cross-cutting concern carrying two flows
over one bidirectional advisory channel per declared workflow:

1. **Hints** (client → storage) — advisory, never authoritative (I-WA1).
2. **Telemetry feedback** (storage → client) — caller-scoped only (I-WA5, I-WA6).

Workflow is **not** a bounded context. It is a correlation + steering
construct owned entirely by the client, with a stateless routing layer on
the server side and bounded per-workflow state.

### Correlation identity

Every data-path operation issued while a workflow is active carries:

```
(org_id, project_id?, workload_id, client_id, workflow_id, phase_id)
```

- `client_id` pinned per native-client process (I-WA4).
- `workflow_id` ≥128-bit opaque, unique within workload (I-WA10).
- `phase_id` monotonic within workflow, bounded phase history (I-WA13).

### Advisory channel

- One bidi gRPC stream per active workflow, on the data fabric, under
  the same mTLS tenant certificate as the data path (I-Auth1, I-WA3).
- Authorization is **per-operation** on the stream, not only at
  establishment (I-WA3). Certificate revocation tears down the stream.
- Side-by-side with the data path — **not in-band**. Data-path requests
  may be annotated with a short `workflow_ref` header that the data-path
  code passes through; server-side the annotation is routed to the
  advisory subsystem asynchronously (I-WA2). Annotation is strictly
  best-effort:
  - malformed `workflow_ref` → ignored, no data-path impact
  - `workflow_ref` for an expired workflow → dropped silently on the
    advisory side with an `hint-rejected: workflow_unknown` audit event
  - advisory subsystem overloaded or unavailable → annotation enqueued
    with bounded buffer; on overflow dropped with a rate-limited
    `annotation_dropped` audit event. Data-path operation outcome is
    never affected (I-WA2).
- Closure of the advisory stream without `End` auto-expires the workflow
  on TTL. Process restart produces a fresh `client_id`; the old
  workflow expires on TTL and the new process must redeclare. No
  reattach protocol is defined in this ADR — it may be revisited as a
  follow-up feature with its own spec + adversary review.

### Hint taxonomy (must-have)

| Category | Example values | Acted on by |
|---|---|---|
| Workload profile | `ai-training`, `ai-inference`, `hpc-checkpoint`, `batch-etl`, `interactive` | Control Plane policy gate; tunes other hint defaults |
| Phase marker | `stage-in`, `compute`, `checkpoint`, `stage-out`, `epoch-N` (opaque semantic tag) | View (cache policy), Composition (write-absorb), Chunk (placement hot-set) |
| Access pattern | `sequential` / `random` / `strided` / `broadcast` | Native Client (prefetch), View (materialization priority) |
| Prefetch range | list of `(composition_id, offset, length)` | View, Chunk (opportunistic warm) |
| Priority class | `interactive` / `batch` / `bulk` within policy-allowed max | Gateway / Client QoS scheduler |
| Affinity preference | pool / rack / node preference within policy | Chunk placement engine |
| Retention intent | `temp` / `working` / `final` | Composition GC urgency, Chunk EC policy selection |
| Dedup intent | `shared-ensemble` / `per-rank` | Chunk dedup path (still bounded by I-X2) |
| Collective announcement | `{ranks, bytes_per_rank, deadline}` | Chunk write-absorb provisioning |

### Hint taxonomy (nice-to-have, deferred)

Co-access grouping, deadline, transient markers (`discardable after epoch N`),
NUMA/GPU topology, peer-rank state. Architect may add these in a follow-up.

### Telemetry feedback (must-have)

| Signal | Shape | Scoping |
|---|---|---|
| Backpressure | severity enum + retry_after_ms | Caller's own resources only |
| Materialization lag | ms | Caller's views only |
| Locality class | bucketed enum (local-node, local-rack, same-pool, remote, degraded) | Caller-owned chunks only |
| Prefetch effectiveness | bucketed hit-rate | Caller's declared prefetches only |
| QoS headroom | bucketed fraction | Caller's workflow/workload |
| Own-hotspot | composition_id + coarse level | Caller's own compositions |

### Tenant-hierarchy scoping

- Policy chain: **cluster → org → project → workload**. Each level
  narrows (never broadens) its parent's ceilings (I-WA7). Profile
  allow-lists inherit the same way.
- Workflow lives strictly within one workload (I-WA3).
- Disable switch at any level (I-WA12) — data path unaffected when
  advisory is disabled.

### Security posture

- Hints cannot extend capability (I-WA14).
- Telemetry is not an existence oracle (I-WA6) — unauthorized target →
  same shape as absent target, including timing distribution.
- Telemetry aggregation uses k-anonymity over neighbour workloads,
  k ≥ 5 (I-WA5).
- Covert-channel hardening: rejection latency and telemetry response
  size are bucketed (I-WA15).
- All advisory decisions audited on tenant shard; cluster-admin view
  sees opaque hashes (I-WA8, consistent with I-A3 / ADR-015).

### Isolation from data path

- Advisory channel on a separate gRPC service and (ideally) a separate
  server-side tokio runtime / goroutine pool from the data path.
- Hint handling is best-effort with bounded buffering; on overload the
  handler drops-and-audits rather than queuing.
- Data-path code never awaits advisory responses. At most it emits
  fire-and-forget annotations.

## Alternatives considered

1. **Attach hints as headers on existing data-path RPCs, no separate channel.**
   Rejected: couples hint handling to data-path latency, violates I-WA2
   isolation, and makes bidirectional telemetry awkward.
2. **Model workflow as a new bounded context with durable state.**
   Rejected: workflows are ephemeral correlation handles. Persisting them
   invites a new shared-state problem and gives little value beyond what
   the audit log already provides.
3. **Expose ADR-015 observability directly to clients.**
   Rejected: ADR-015 is operator-facing with aggregate/anonymized scope.
   Clients need caller-scoped, near-real-time feedback with a different
   privacy boundary (I-WA5/6).
4. **Server-authoritative hints (storage can infer and inject its own).**
   Rejected: inferring client intent from data-path patterns is already
   done internally; the point of this ADR is to let clients supply
   authoritative-to-themselves hints. Server-side inference remains
   available as a fallback when hints are absent.

## Consequences

- New crate `kiseki-advisory` (Rust) — hint validation, routing, rate
  limiting, telemetry emission, audit emission. Side-by-side with
  `kiseki-server`, not inside the data-path crates.
- New protobuf service `WorkflowAdvisory` with `DeclareWorkflow`,
  `EndWorkflow`, `PhaseAdvance`, a bidi `AdvisoryStream`, and
  `SubscribeTelemetry` (may be a stream within `AdvisoryStream`).
- Control Plane extensions: profile allow-lists, hint budgets, opt-out
  switches — inherited org → project → workload.
- Native Client extensions: `WorkflowSession` handle; existing data-path
  methods accept an optional `&WorkflowSession` for automatic
  correlation annotation.
- Audit additions: new event types per I-WA8. Tenant audit export
  (I-A2) includes them; cluster-admin export (I-A3) hashes the
  tenant-scoped identifiers.
- Metric additions (ADR-015 operator view): `advisory_hints_accepted`,
  `_rejected`, `_throttled`, `active_workflows`, `advisory_channel_latency`,
  tenant-anonymized.
- Performance: hint handling overhead target < 5µs p99 per accepted
  hint; telemetry emission frequency capped per subscription.
- Failure mode `F-ADV-1`: advisory-subsystem outage → data path
  unaffected; clients observe `advisory_unavailable` until restoration.
  To be added to `specs/failure-modes.md` (severity P2, blast radius:
  steering quality only).

### Changes from adversary gate-0 review

- I-WA6 extended to cover hint rejection (previously telemetry-only).
- I-WA3 tightened to per-operation authorization.
- I-WA5 defines explicit low-k behaviour (fixed-sentinel neighbour
  component, unchanged response shape).
- New invariants I-WA16 (hint payload size bound), I-WA17 (declare rate
  bound), I-WA18 (prospective policy application).
- I-WA11 tightened to enumerate forbidden advisory target field types.
- I-WA12 defines three-state opt-out with draining transition.
- I-WA13 specifies CAS serialization for PhaseAdvance.
- Reattach protocol explicitly dropped; TTL-only recovery.
- `client_id` construction simplified to CSPRNG (≥128 bits), pinning
  enforced by registrar.
- F-ADV-1 (advisory outage) and F-ADV-2 (audit storm) added to
  failure-modes.md.
- A-ADV-1..A-ADV-4 added to assumptions.md.

## Follow-ups (architect's scope)

- gRPC service definitions and message schemas.
- Exact integration surface between `kiseki-advisory` and each of
  Chunk, View, Composition, Gateway.
- Concrete k-anonymity bucketing algorithm and parameters.
- Exact latency-bucketing and size-bucketing schemes for I-WA15.
- Phase-history compaction format and retention per workload.
- Reattach protocol for process-restart scenarios (I-WA4 scenario).

## Follow-ups (adversary's scope — gate 0 before architect)

- Threat-model the covert-channel surface (timing, size, error-code).
- Validate that the inherent side-channels from backpressure signals
  are truly k-anonymised under worst-case neighbour composition.
- Probe the reattach protocol once drafted.
