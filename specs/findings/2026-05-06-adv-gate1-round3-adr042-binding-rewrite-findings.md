# Adversary Gate-1 Round-3 Findings — ADR-042 transport-binding round-2 amendments (2026-05-06)

**Type**: Adversary → Architect (round-3 verification of round-2 resolutions)
**Date**: 2026-05-06
**Reviewer**: adversary (architecture mode)
**Mode**: re-review of the 11 amendments architect made in response to the round-2 findings (R2-H1 + R2-M1..M6 + R2-L1..L4 + R2-O1..O3). Goal: verify the resolutions cleanly closed the originals AND find any net-new issues introduced by the new sections (§2.4.2.1 cxi DoS mitigations + §3.2.1 graceful drain + `DrainState` field + reason-vocabulary table + multi-stream baseline guidance).
**Verdict**: **PASS — 0 CRITICAL, 0 HIGH, 4 MEDIUM, 4 LOW.** All 14 round-2 findings cleanly closed. Net-new issues are all detail-level and concentrated in two sections (§3.2.1 drain semantics edge cases, §2.4.2.1 DoS-control resource accounting). None block phase 0–8 implementation; phase 9–10 (RDMA bindings) implementer should address M1+M2+M3 before the cxi binding ships.

## Coverage of round-2 findings

| Round-2 | Resolution location | Verified? |
|---|---|---|
| R2-H1 (cxi DoS) | §2.4.2.1 — rate limit + size caps + cooldown + bounded verify pool | ✓ four required controls all named; `KISEKI_NATIVE_CXI_ATTEST_*` env vars defined; metrics added |
| R2-M1 (handshake_failures reason vocab) | §12.1 — 13-value enum table | ✓ cxi-specific failure modes split out; alarm-routable |
| R2-M2 (dlopen distro/arch) | §2.3 + §2.4 — search list across Debian/RHEL/Alpine × `${arch}` | ✓ ARM (aarch64) + POWER (powerpc64le) covered |
| R2-M3 (probe timeout) | §3.1 — 3 s default + `KISEKI_NATIVE_PROBE_TIMEOUT_MS` + duration histogram | ✓ k8s startupProbe slack confirmed |
| R2-M4 (cxi cross-node replay) | §2.4.2 — explicit prose + bounded-exposure analysis + future-ADR for cluster-wide store | ✓ acknowledged with reasoning |
| R2-M5 (connection-pool eviction) | §3.2.1 — graceful drain protocol + `KISEKI_NATIVE_DRAIN_BUDGET_MS` + drain metric | ✓ but introduces R3-M1..M3 below |
| R2-M6 (Draining bindings) | §1.7 — `DrainState` field + state-vs-bindings rules table | ✓ but introduces R3-M4 below |
| R2-L1 (trust matrix drift) | §5.1 references §2.4.1 as authoritative | ✓ |
| R2-L2 (RequestPrincipal enforcement) | §1.8 — arch-check CI gate mandate | ✓ |
| R2-L3 (schema_version > 1) | §2.4.2 step 1 — fail-closed reject | ✓ |
| R2-L4 (multi-stream baseline) | §14.1 — `-q 4` / `-p 4` baselines + matched-concurrency rule | ✓ but loopback dev/test guidance is questionable (R3-L4 below) |
| R2-O1 (TCP-framed wire-version coupling) | §13.2 — lockstep-bump rule with ADR-041 | ✓ |
| R2-O2 (RDMA reduced BDD) | §16.1 phase 6 — minimum subset listed | ✓ |
| R2-O3 (fault-injection scenarios) | §16.1 phase 6 — list named, deferred to bdd-completion-plan | ✓ |

All 14 round-2 findings cleared at the spec layer.

---

## CRITICAL

(none)

---

## HIGH

(none)

---

## MEDIUM

### R3-M1: §3.2.1 drain protocol leaves RDMA QP cleanup unspecified

**Severity**: Medium
**Category**: Robustness > Resource exhaustion (kernel-side)
**Location**: ADR-042 §3.2.1 (graceful drain), §2.3 (ibverbs), §2.4 (libfabric)

**Description**: §3.2.1 specifies the application-level drain semantics (no new requests dispatched, in-flight requests run to completion, hard-close after `DRAIN_BUDGET_MS`). For TCP-based bindings (gRPC, TCP-framed) hard-close means closing the TCP socket — well-understood, kernel cleans up reliably.

For RDMA bindings, "hard-close" means transitioning the QP to ERR state, draining any pending work-completion entries, then destroying the QP. Each step has its own error path; doing it wrong leaks kernel-side resources (memory regions stay pinned, QP slots stay allocated, completion queue entries stay queued).

The spec doesn't address:
- Order of operations on RDMA hard-close: drain WC queue first, then transition QP, then destroy?
- Memory-region deregistration: when a hard-close fires, are pre-registered MRs freed immediately or kept until the next drain cycle?
- libfabric resource cleanup: provider-specific (`fi_close()` semantics differ across cxi/verbs); spec gives no per-provider guidance.

A kernel-resource leak at QP-destroy time eventually exhausts NIC resources (mlx5 devices have a finite QP count, ~256k typical). On a node that goes through many binding restarts in a session, the leaked QPs accumulate.

**Evidence**: §3.2.1 says "hard-close the connection" — for RDMA bindings, this is materially different from TCP socket close.

**Suggested resolution**: Architect adds §3.2.2 (per-binding hard-close discipline):

| Binding | Hard-close steps |
|---|---|
| gRPC/h2 | TCP socket close (kernel handles cleanup) |
| TCP-framed | TCP socket close |
| ibverbs | (1) Stop sender; (2) drain CQ for outstanding WCs; (3) `ibv_modify_qp` to ERR; (4) `ibv_destroy_qp`; (5) `ibv_dereg_mr` for each MR; (6) `ibv_destroy_cq` |
| libfabric | provider-specific via `fi_close()` on FID hierarchy in reverse-creation order; cxi/verbs differ on whether MR deregistration is automatic — implementer documents per provider |

Implementer's per-binding tests include a "1000 close-cycles" stress test asserting no kernel resource accumulation (`ibv_devinfo` MR/QP counters stable).

This is implementer-phase work; the spec needs the per-binding discipline named so the implementer doesn't ship a leaky cleanup path.

### R3-M2: §3.2.1 boundary between "queued client-side" and "in-flight server-side" is undefined for drain

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §3.2.1

**Description**: §3.2.1 says "in-flight requests run to completion" but doesn't define the boundary. A request the client has serialized and queued in its connection's send buffer but not yet flushed to the wire — is that "in-flight"? An RDMA `post_send` that's been issued but the WC hasn't been polled — in-flight or not?

The boundary matters for two reasons:
1. **Drain budget accounting**: does the client wait for server-side completion of all "queued" requests? A client with 100 queued requests on a draining connection waits for 100 round-trips before close — could blow `DRAIN_BUDGET_MS`.
2. **Idempotency-key dedup state**: a request the server NEVER saw still gets retried via idempotency_key (correct). A request the server saw but the response didn't reach the client gets retried via idempotency_key (correct, dedup table short-circuits). The boundary must distinguish these two cases unambiguously, or the client double-charges idempotency lookups.

**Evidence**: §3.2.1 prose says "in-flight requests run to completion" without defining the term; §1.6 stream cap doesn't help.

**Suggested resolution**: §3.2.1 amends with explicit definition:

> **In-flight** for drain purposes means: a request whose RPC envelope has been written to the binding's wire (TCP `write_all` returned, or RDMA `post_send` issued, or h2 stream advanced past HEADERS). Client-side queued-but-unsent requests (still in the connection's send buffer at drain start) are NOT in-flight; they are returned to the client's request pool to retry on a fresh connection (using the same idempotency_key — the server never saw them, so dedup short-circuits naturally).

This collapses the two cases cleanly: queued-but-unsent → re-issued with idempotency, in-flight-but-incomplete → drain-to-completion.

### R3-M3: `DrainState.accepts_new_work` flip mechanism unspecified

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §1.7 (state-vs-bindings rules), §9 (lease lifecycle)

**Description**: §1.7's `DrainState { quiesce_window_remaining_ms, accepts_new_work }` says the flag is "false during quiesce; true (briefly) during graceful release". §9.1 lease-tracker behavior on draining nodes is described in prose. But:

- **Who flips `accepts_new_work` from `false` to `true`?** The lease tracker per §9? The cluster control plane (ADR-021)? The drain coordinator (ADR-035)? Spec doesn't say.
- **What's the "graceful release" window's duration?** §1.7 says "briefly" but no number. ADR-035 has a quiesce window; this is something else.
- **Race conditions**: client reads topology version N where node X has `DrainState { accepts_new_work: false }`. Concurrently, the cluster flips it to `true`. Client reads version N+1 with `true`. In between, did the lease-tracker accept new lease acquisitions? If yes, ADR-035 drain protocol breaks.

**Evidence**: §1.7 introduces the flag but doesn't pin the lifecycle; §9.1 prose on draining is binding-agnostic and doesn't reference the flag directly.

**Suggested resolution**: Architect adds §1.7.1 (DrainState lifecycle):

> `DrainState.accepts_new_work` is set by `kiseki-control` (the drain coordinator, per ADR-035) at two transition points:
>
> 1. Node enters `Draining` (operator-triggered): cluster control sets `accepts_new_work = false` and starts the quiesce window timer.
> 2. Quiesce window expires OR all in-flight work for the node completes (whichever first): cluster control flips `accepts_new_work` to `true` for `KISEKI_NATIVE_DRAIN_GRACEFUL_RELEASE_MS` (default 5000 ms), then transitions the node to `Evicted`. The brief `true` window lets stragglers (e.g. a client with stale topology) complete their final ops without bouncing through the lease tracker.
>
> Topology version bumps on each transition. Clients reading a newer version always see consistent flag + node-state.

This needs to land before phase 5 (`NativeClient` + `TopologyCache`) implementation; otherwise the client doesn't know which `DrainState` transitions to act on.

### R3-M4: §3.2.1 trigger condition for client-side connection-pool diff check unspecified

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §3.2.1, §6 (topology refresh)

**Description**: §3.2.1 says "On topology version bump that changes node N's `bindings` set, the client diffs against its open connections to N." But what triggers the client to actually run that diff?

- **Per-response check**: every response trailer carries `topology_version`. Client diffs on every mismatch. Hot-path overhead but immediate detection.
- **On explicit `get_topology` refresh**: client runs diff only when it polls topology. Cheap but laggy; in-flight requests on stale-binding connections succeed but the next request after the version bump may pick the wrong (stale) binding.
- **On connection-error**: only when a request fails on the connection. Dead-letter behavior; scale-and-shape unclear.

The spec doesn't pick. Different implementations could pick differently; cross-implementation behavior would diverge.

**Evidence**: §3.2.1 prose is silent on the trigger.

**Suggested resolution**: §3.2.1 amends:

> The client runs the binding-set diff **on every topology version change observed via response trailer or explicit `get_topology` refresh**. The trigger is "version mismatch detected"; the trigger source is ANY response trailer or explicit poll. This ensures bound-by-one-RTT detection without per-response branching cost (the version-compare is already on the response path for §6 staleness detection).

Alternative path: spec says implementer's choice if they document it. But cross-binding consistency is easier with a single mandated trigger.

---

## LOW

### R3-L1: §3.2.1 drain budget vs long-running uploads

**Severity**: Low
**Category**: Correctness > Specification compliance / robustness
**Location**: ADR-042 §3.2.1, §13 (out of scope #2 — resume tokens)

**Description**: §3.2.1's `KISEKI_NATIVE_DRAIN_BUDGET_MS` defaults to 30 s. A client doing a multi-GB upload via `put_object_stream` may have an in-flight request that's been running for minutes (well past 30 s) when a binding-restart fires. Hard-close at budget kills the upload; idempotency_key retry has to start from scratch because v1 doesn't have resume tokens (§13.4 explicitly out of scope).

Operator pain: a node-level binding restart during a long-running training-data upload silently loses the upload's progress. The user sees `Aborted{binding_drain_timeout}` and a long retry.

**Evidence**: §3.2.1 specifies 30 s default; §13 punts resume tokens to a future amendment.

**Suggested resolution**: §3.2.1 adds a note acknowledging the trade-off: "The default 30 s budget is tuned for typical request sizes (≤ 64 MiB streaming). Long-running multipart uploads exceeding this budget will hard-close on drain and require client-side restart of the multipart session via the same `idempotency_key` (the existing in-flight parts are reclaimed by the orphan-fragment scrub per F-NG12). Operators on workloads with multi-GB streaming uploads should raise the budget via `KISEKI_NATIVE_DRAIN_BUDGET_MS`; the cost is a longer drain tail at upgrade time."

This is documentation, not a redesign. The future resume-token ADR closes it more cleanly.

### R3-L2: `cxi_attestation_throttled_total{source}` label cardinality on large clusters

**Severity**: Low
**Category**: Robustness > Observability
**Location**: ADR-042 §2.4.2.1 (metric `kiseki_native_cxi_attestation_throttled_total{source, reason}`)

**Description**: The `source` label is per-peer-node (the cluster peer node identifier from the cxi auth-key context). On a 100-node cluster with the four reason values, that's 400 timeseries from this metric alone — fine. On a 10000-node mega-cluster (Frontier-class), it's 40 000 timeseries. Some Prometheus deployments will struggle.

**Evidence**: §2.4.2.1 doesn't bound the label cardinality.

**Suggested resolution**: §2.4.2.1 adds: "On clusters with > 1000 nodes, operators may want to drop the `source` label and rely on the throttled-rate aggregate instead, accepting reduced per-peer-node attribution. The metric's `source` label is the high-cardinality dimension; everything else is bounded."

Alternative: rotate `source` into a less-granular bucket (e.g. `source_role ∈ {peer, client}`) with a separate per-source count exposed only on operator query. Out of scope for v1.

### R3-L3: §2.4.2.1 bounded verify pool can starve under sustained load

**Severity**: Low
**Category**: Robustness > Resource exhaustion
**Location**: ADR-042 §2.4.2.1 (bounded ECDSA-verify pool)

**Description**: 16 workers × 64 queue depth = 80 outstanding verifies before pool overflow. Under sustained legitimate cxi connection-establishment rate (e.g. 200/sec across the cluster), the pool can sit at near-cap depth. An adversarial peer's surge (even at the per-source rate limit of 100/60s) coincides with legitimate load, and legitimate clients see `ResourceExhausted{cxi_attestation_queue_full}` — false positives.

The mitigation is "operator alarms on sustained near-cap depth via `kiseki_native_cxi_verify_queue_depth`" (per the metric in §2.4.2.1). Self-healing isn't specified; operator must intervene.

**Evidence**: §2.4.2.1 specifies `KISEKI_NATIVE_CXI_VERIFY_QUEUE_DEPTH` default 64 without justification.

**Suggested resolution**: §2.4.2.1 adds a sizing note: "The default 64 queue depth assumes peak legitimate establishment rate ≤ 100 cxi connections/sec across the cluster. On larger clusters, raise via `KISEKI_NATIVE_CXI_VERIFY_QUEUE_DEPTH`; the in-memory cost is ~256 bytes per queued verify (envelope reference + waker), bounded at `queue_depth × workers` slots."

### R3-L4: §14.1 single-node dev/test loopback verbs guidance is questionable

**Severity**: Low
**Category**: Correctness > Documentation
**Location**: ADR-042 §14.1 (single-node dev/test paragraph)

**Description**: §14.1 says: "For dev-loop measurements, use loopback HCA pairs (`ibv_devinfo` + `cma_roce_mode` discipline) or run the bench in a two-VM pair on a local KVM host."

Loopback verbs (one HCA talking to itself via the kernel software path) is rare and not always available. Mellanox/NVIDIA NICs typically don't expose a "loopback mode" for verbs — the path goes through hardware whether peer is local or remote. The two-VM-on-KVM path is more realistic for dev work.

**Evidence**: §14.1 guidance suggests loopback as primary; in practice it's secondary.

**Suggested resolution**: §14.1 amends: "For dev-loop measurements, the two-VM-on-KVM-host path is most reliable (each VM gets a virtual HCA via SR-IOV passthrough or libvirt rdma-cm bridging). Loopback HCA pairs are possible on some hardware but are not consistently available; treat them as a fallback. Real cluster-hardware perf-gate runs in CI on the dedicated bench fixture."

---

## Cross-cutting observations

### R3-O1: §1.7 `DrainState` field appears in topology but `Failed`/`Evicted` paths haven't been verified for client behavior on already-open connections

§1.7 says `Failed` and `Evicted` nodes have empty bindings. But: a client may already hold an open connection from before the state transition. The spec doesn't say close-on-state-change for these — §3.2.1 covers binding-set change but not node-state change. A client could continue using a connection to a `Failed` node until the connection naturally times out or errors.

For Failed: this is benign because the connection will fail naturally. For Evicted: same. But the spec is silent and a strict reading would let the client keep dialing post-eviction. Not high-impact; flag for clarity.

**Suggested resolution**: small prose note in §1.7: "On state transition to Failed or Evicted, clients SHOULD close existing connections to the node within one round-trip of observing the transition. Implementations that don't enforce this still correct themselves naturally because Failed/Evicted nodes don't accept new requests."

### R3-O2: §2.4.2 cross-node replay aggregate-rate metric not explicitly named

§2.4.2's "Cross-node replay surface" prose acknowledges the residual exposure but doesn't call out the dashboard alarm operators need to detect a distributed replay attack. The per-node `cxi_attestation_replay` rate stays low for a successful distributed replay (one envelope, N nodes, each sees one duplicate); aggregate-cluster-rate would catch it.

**Suggested resolution**: §12.1 or §2.4.2 notes: "Operators monitoring for cross-node replay attacks should alert on `sum by (cluster) (rate(kiseki_native_binding_handshake_failures_total{reason='cxi_attestation_replay'}))` rather than per-node rates. A distributed replay shows as low per-node + high aggregate."

### R3-O3: round-2 `R2-O3` (fault-injection BDD) deferred to bdd-completion-plan but the plan doesn't yet have those scenarios

The §16.1 phase 6 amendment lists fault-injection scenarios but defers concrete scenarios to `specs/implementation/bdd-completion-plan.md`. That plan currently doesn't have them (they'd be added during phase 6 work). Architect should ensure the implementer doesn't skip them when phase 6 lands; consider adding a checklist entry in `specs/implementation/adr-042-native-gateway.md` so the gate-2 audit catches their absence.

**Suggested resolution**: implementer adds a phase-6 acceptance criterion checklist line in `specs/implementation/adr-042-native-gateway.md`: "Phase 6 BDD includes the four fault-injection scenarios from §16.1: probe timeout, listener crash + restart, topology version regress, cxi attestation replay under load." Gate-2 auditor verifies presence.

---

## Recommendation

**Pass.** Architect's round-2 amendments cleanly close all 14 round-2 findings. The 0 HIGH + 4 MEDIUM + 4 LOW net-new is at the upper end of the predicted "≤ 2 MEDIUM net new" but expected — §3.2.1 graceful drain is genuinely complex and §2.4.2.1 DoS controls have real resource-accounting nuance.

**Phase 0–8 implementation is unblocked under the current spec.** The 4 MEDIUM findings should land before phase 9–10 (RDMA bindings) implementation:

- R3-M1 (RDMA hard-close discipline) is mandatory for any RDMA binding implementer.
- R3-M2 (in-flight boundary definition) is mandatory for client-side drain implementation.
- R3-M3 (DrainState lifecycle) is mandatory for the `kiseki-control` drain coordinator.
- R3-M4 (diff trigger) is mandatory for client-side topology consumer.

The 4 LOW findings are documentation polish. The 3 cross-cutting observations are operator-facing notes the architect should add but don't block any implementation phase.

Architect's round-2 prediction was "0 HIGH, ≤ 2 MEDIUM net new". Actual: 0 HIGH (✓), 4 MEDIUM (off by 2 — drain semantics are richer than predicted). Acceptable accuracy.

**This ADR is now fit for implementer phase 0 (contract type extraction). Recommend round-3 amendments before phase 5 + 9.**
