# Adversary Gate-1 Round-2 Findings — ADR-042 transport-binding amendments (2026-05-06)

**Type**: Adversary → Architect (post-amendment review)
**Date**: 2026-05-06
**Reviewer**: adversary (architecture mode)
**Mode**: re-review of the architect's amendments to the round-1 findings (4 HIGH, 6 MEDIUM, 3 LOW, 3 cross-cutting). Looking for issues introduced *by* the resolutions, not whether they cover the originals. The originals are coverage-checked first; the bulk of the work is finding what the new sections opened up.
**Verdict**: **PASS WITH FOLLOW-UPS — 0 CRITICAL, 1 HIGH, 6 MEDIUM, 4 LOW.** Architect's amendments cleanly resolve all 13 round-1 findings. Net new issues are mostly detail-level; the one HIGH (R2-H1, cxi-handshake DoS surface) is a security gap the spec must close before phase 10 implementation begins, but does not gate phase 0–8 (pre-RDMA) work.

## Coverage of round-1 findings

| Round-1 | Resolution location | Verified? |
|---|---|---|
| H1 (RDMA trust matrix) | ADR-042 §2.3 mTLS-over-rdma-cm; §2.4.1 trust matrix; §2.4.2 `CxiAttestationEnvelope`; §2.4.3 efa deferred; I-NG25 added | ✓ cxi has a real attestation mechanism; verbs inherits ibverbs/mTLS cleanly; efa cleanly scoped out |
| H2 (probe + listener phasing) | §3.1 split into 3 phases; §3.5 edge cases | ✓ probe-spawn separation with per-binding-failure tolerance + clear exit codes |
| H3 (per-node binding contract) | §1.7 contract types (`NodeBindings`, `BindingEndpoint`, `LatencyClass`); §3.2 per-edge selection; §6 binding-set publisher added | ✓ §3.2 vs §3.3 contradiction resolved (per-edge wins) |
| H4 (no rolling upgrades) | §13.2 explicit pre-1.0 stance | ✓ stated prominently |
| M1 (dlopen safety) | §2.3 + §2.4 absolute-path env vars + ownership check + audit log | ✓ but defaults are distro-specific (R2-M2 below) |
| M2 (pinning hardening) | §3.1 metric `kiseki_native_binding_pinned_total`; `auto` literal; tenant-policy out-of-scope note | ✓ |
| M3 (RequestPrincipal trait) | §1.8 trait; §5.1 + §5.2 reads via trait | ✓ but discipline only (R2-L2 below) |
| M4 (refresh attribution) | §3.4 + §12.1 `kiseki_native_topology_refresh_total{reason}` | ✓ |
| M5 (ranking coarse) | §3.6 acknowledged + future-ADR pointer | ✓ |
| M6 (RDMA falsifiable) | §14.1 fabric-peak procedure + 50%/2× perf-gate criterion | ✓ but multi-stream not addressed (R2-L4 below) |
| L1 (keymanager reference) | §11.1 ADR-022 rev-3 reference | ✓ |
| L2 (phase renumbering) | §16.1 phases 0-10 | ✓ |
| L3 (build-deps clarification) | §18 A7 amendment | ✓ |
| O2 (per-binding observability) | §12.1 metrics table | ✓ but reason labels too coarse for cxi (R2-M1 below) |
| O3 (BDD parameterization) | §16.1 phase 6 + tag scheme | ✓ |
| O4 (future binding-specific extensions) | §13.3 outline | ✓ |

All 16 substantive round-1 findings cleared at the spec layer. Net new issues from the resolutions:

---

## CRITICAL

(none)

---

## HIGH

### R2-H1: cxi attestation handshake exposes a pre-auth ECDSA-verify DoS surface

**Severity**: High
**Category**: Security > Resource exhaustion / pre-auth attack surface
**Location**: ADR-042 §2.4.2

**Description**: The 8-step server validation flow in §2.4.2 runs an ECDSA-P256 signature verification (step 6) and a cert-chain X.509 path validation (step 2) on every first message. Both are CPU-bound and pre-auth — they run before the connection has been authenticated to a tenant.

Cost analysis:
- ECDSA-P256 verify: ~50 µs per op on commodity x86_64 (rust-crypto / ring).
- Full X.509 path validation with CRL/OCSP lookup: 100 µs–10 ms depending on cache + network.
- Per-message worst-case: ~1 ms before the server can reject.

Attack model:
- cxi auth-key authenticates the connection at the libfabric provider layer — this means the attacker must be a cluster member to open a cxi connection. **But**: a compromised peer node IS a cluster member by construction. The threat model "compromised internal node" is exactly what the per-tenant SAN attestation is supposed to limit.
- Compromised peer floods the cxi listener with malformed `CxiAttestationEnvelope` messages (random signatures over arbitrary canonical_san). Each costs the server ~1 ms of CPU before reject.
- 1 000 envelopes/sec/attacker = 1 server core saturated before any legit work runs.
- Attack surface is ALL bindings' first-message handlers, not just cxi — but cxi is the deepest CPU spend per malformed message because of the explicit signature verify step. Other bindings rely on TLS handshake which also costs but has standard rate-limiting practice.

There's no rate limit specified. There's no per-source connection cap specified. There's no cert-chain size cap (an attacker can inflate cert_chain_der to 10 MB and the server will allocate + parse it).

**Evidence**: §2.4.2 enumerates 8 validation steps; none mentions throttling, rate limits, or input-size caps. The §1.5 64-MiB-per-stream cap and §1.6 256-stream-per-tenant cap apply to the application layer AFTER attestation succeeds — they don't gate the attestation handshake itself.

**Suggested resolution**: Architect adds §2.4.2.1 (cxi handshake DoS mitigations):

1. **Per-source connection-establishment rate limit**: at most N attestation attempts per (source_ip, 60-s window) before subsequent attempts return `ResourceExhausted{cxi_attestation_rate_limit}` without parsing the envelope. Default N = 100; configurable via `KISEKI_NATIVE_CXI_ATTEST_RATE_PER_SOURCE`.
2. **Envelope size caps**: `CxiAttestationEnvelope` body ≤ 16 KiB total (rejects oversized at frame-decode boundary, before postcard parses); `cert_chain_der` ≤ 8 KiB total (5 certs × 1.6 KiB typical for ECDSA-P256), enforced at parse time.
3. **Failed-attestation per-source cooldown**: after K consecutive attestation failures from the same `source_ip`, the source is parked for `cooldown_secs`. Defaults K=10, cooldown=60 s. Resets on first success.
4. **Metric**: `kiseki_native_cxi_attestation_throttled_total{source_ip}` so abuse from compromised peers surfaces in dashboards.

Also: ECDSA-P256 verify can be moved to a `tokio::task::spawn_blocking` thread pool with a bounded queue (default 16 workers). Pool overflow returns `ResourceExhausted{cxi_attestation_queue_full}` immediately. This isolates verify CPU from the runtime's reactor.

This must land before phase 10–11 (RDMA bindings) implementation begins. Phases 0–8 (gRPC + TCP-framed + selector + client) are not blocked — they don't ship the cxi attestation path.

---

## MEDIUM

### R2-M1: §12.1 handshake_failures reason label too coarse for cxi attestation

**Severity**: Medium
**Category**: Robustness > Observability gaps
**Location**: ADR-042 §12.1, cross-references §2.4.2

**Description**: §12.1's `kiseki_native_binding_handshake_failures_total{binding, reason}` lists `attestation_failed` as a single reason value. But §2.4.2's validation flow defines five distinct cxi-specific failure modes:

- `cxi_attestation_missing` (no envelope as first message)
- `cxi_san_mismatch` (envelope canonical_san ≠ cert SAN)
- `cxi_attestation_clock_skew` (timestamp outside ±30 s)
- `cxi_attestation_signature_invalid` (ECDSA verify fails)
- `cxi_attestation_replay` (nonce in bloom filter)

Collapsing them to a single label hides the diagnostic signal operators need. Distinguishing "bad client clock" from "deliberate replay" from "wrong cert" is the difference between a misconfigured tenant and an active attack.

**Suggested resolution**: §12.1 amends the `reason` label vocabulary to include the 5 cxi-specific values plus the existing TLS-shaped values (`san_invalid`, `cert_expired`, `cert_revoked`, `dlopen_failed`, `other`). The label cardinality grows but stays bounded (~12 values total across all bindings). Operators can then alarm on `cxi_attestation_replay > threshold` independently.

### R2-M2: dlopen default paths are distro-specific + arch-hardcoded

**Severity**: Medium
**Category**: Correctness > Specification compliance / portability
**Location**: ADR-042 §2.3, §2.4

**Description**: The defaults are:
- `KISEKI_NATIVE_IBVERBS_LIB=/usr/lib/x86_64-linux-gnu/libibverbs.so.1`
- `KISEKI_NATIVE_LIBFABRIC_LIB=/usr/lib/x86_64-linux-gnu/libfabric.so.1`

Two issues:

1. **Distro path conventions differ**:
   - Debian / Ubuntu: `/usr/lib/x86_64-linux-gnu/`
   - RHEL / CentOS / Rocky / Fedora: `/usr/lib64/`
   - SUSE: `/usr/lib64/`
   - Alpine: `/usr/lib/`
   The default works on Debian-family only. Operators on Red Hat-family hosts must override the env var, which is easy to forget — the binding will silently self-disqualify with `Unavailable { reason: "libibverbs not present" }` and the operator sees no RDMA.

2. **Architecture hardcoded**: `x86_64-linux-gnu` won't match on ARM64 (`aarch64-linux-gnu`) or POWER9 (`powerpc64le-linux-gnu`). HPC clusters on ARM (Fugaku-class, Grace Hopper) exist; the default fails them.

**Evidence**: ADR-042 §2.3 says default `/usr/lib/x86_64-linux-gnu/libibverbs.so.1`. No discussion of distro/arch variation.

**Suggested resolution**: Implementer searches a fixed list of probable absolute paths in order, taking the first that satisfies the ownership/permissions check:

```
# Searched in order, first match wins:
/usr/lib/${arch}-linux-gnu/lib<name>.so.1  # Debian/Ubuntu
/usr/lib64/lib<name>.so.1                  # RHEL/SUSE
/usr/lib/lib<name>.so.1                    # Alpine, others
```

`${arch}` is determined at probe time via `cfg!(target_arch=...)`. The env var override remains as an operator escape hatch when the auto-search fails.

This should land before phase 10 (ibverbs binding) implementation; not blocking phases 0–8.

### R2-M3: phase-1 startup probe wall-clock budget cuts close to k8s liveness defaults

**Severity**: Medium
**Category**: Robustness > Resource exhaustion (startup-time)
**Location**: ADR-042 §3.1

**Description**: §3.1 phase 1 says "5 s timeout per binding, fully sequential". With four bindings (gRPC, TCP-framed, ibverbs, libfabric), worst-case probe time is 20 s. Add phase 2 (port-conflict check, fast) + phase 3 (listener-spawn for each binding, can take 2–5 s for RDMA fabric init) ≈ 25–35 s total worst-case startup before the server is ready to serve.

Kubernetes liveness/startup-probe defaults:
- `livenessProbe.initialDelaySeconds`: 0 (default — implies pod is alive at start)
- `startupProbe.failureThreshold` × `periodSeconds`: typically 30 × 1 s = 30 s window before the pod is killed

35 s worst-case startup vs 30 s k8s default is too close. Operators on EKS/GKE/AKS will hit pod-restart loops on slow hosts (heavily loaded `dlopen`, libfabric provider discovery taking longer than expected).

**Evidence**: §3.1 says "≤ 20 s for all four bindings" but adds nothing about phase 3 listener-spawn time. §3.5 acknowledges `dlopen` can take seconds.

**Suggested resolution**: Two small changes:

1. **Tighten phase-1 default to 3 s per binding** (×4 = 12 s), but make it configurable via `KISEKI_NATIVE_PROBE_TIMEOUT_MS`. Most healthy hosts probe in <100 ms; 3 s tolerates load without leaving 5 s of slack on every binding.
2. **Operator docs prominently document the startup-time worst case + recommended k8s `startupProbe.failureThreshold` setting** (≥ 60 s window).

Additionally: emit `kiseki_native_binding_probe_duration_seconds{binding}` so operators can right-size the timeout per their environment.

### R2-M4: cxi cross-node replay window is per-server, not cluster-wide

**Severity**: Medium
**Category**: Security > Cryptographic correctness / replay defense
**Location**: ADR-042 §2.4.2

**Description**: §2.4.2 step 7 says "per-(canonical_san, nonce) bloom filter for last 60 s". The bloom filter is **per-server**. A captured `CxiAttestationEnvelope` can therefore be replayed on every other cluster node within the 60 s window — each replay establishes one new connection per node.

Attack scenario:
- Attacker passively captures a single attestation envelope (e.g. via fabric tap, bus probe).
- Attacker has 60 s to replay it.
- Cluster has N=10 nodes; each maintains an independent bloom filter.
- Attacker can establish 10 new connections, each lasting until the cert revokes or the server times the connection out — typical idle-timeout 5 minutes.

This isn't catastrophic — the connections die quickly without the attacker's actual private key (subsequent application messages need at minimum the cert's matching workflow). But it's a measurable weakening of the "single-use attestation" property the design implies.

**Evidence**: §2.4.2 explicitly says "per-server". §2.4.2 §"Why this is sound" doesn't acknowledge the cross-node replay surface.

**Suggested resolution**: Two paths; pick one:

1. **Accept the residual exposure explicitly**: add a paragraph to §2.4.2 §"Why this is sound" stating the cross-node replay surface and reasoning (60-s window × N nodes = bounded attack volume; subsequent application messages still need the private key for any signed/keyed verb; idle-timeout limits open-connection dwell). This is the cheap path — defensible because the threat model is bounded and the cost of a centralized nonce store is high.
2. **Cluster-wide replay defense**: replicate seen-nonce sets via Raft. Adds one Raft round-trip per cxi connection — meaningful latency cost on connection-establishment, but per-stream amortized cost is low since once-per-connection. Captured as a future-ADR alternative; not the v1 path.

Path 1 is the recommended v1 stance. The §2.4.2 prose update is a paragraph, not a redesign.

### R2-M5: per-edge selection silent on connection-pool eviction when ranking changes mid-session

**Severity**: Medium
**Category**: Correctness > Concurrency / specification compliance
**Location**: ADR-042 §3.2

**Description**: §3.2 says "each client → node connection picks independently" and "the contract types are identical, so request_id + idempotency_key carry across; the binding selection is per-connection, not per-session." This is the right model, but:

- A client maintains a pool: one connection per (node, binding). If a binding-restart event (§3.4) happens mid-session, the client's preferred binding for that node may change. What happens to the existing in-flight connection on the now-stale binding? Spec doesn't say.

- Specifically: if node A's `libfabric/cxi` listener restarts but the client has 5 in-flight requests on the old cxi connection, are those drained to completion or aborted? If aborted, are the affected requests retried on a fresh connection? Spec doesn't specify drain-vs-abort behavior.

- The client may also see a node downgrade its binding-set (operator removes ibverbs by stopping the rdma-core service). Same question: drain or abort?

**Evidence**: §3.2 + §3.4 + §6 each specify part of this story; none specify how the in-flight requests on a stale connection are handled.

**Suggested resolution**: Add §3.2.1 (binding-change connection lifecycle):

- On topology version bump that includes a binding-set change for node N, the client checks whether any of its open connections to N are on a now-removed binding.
- If yes: enter "graceful drain" mode for those connections — no new requests dispatched to them, in-flight requests run to completion (success or timeout), then close.
- New requests for N use the highest-ranked still-available binding for N.
- Drain timeout: per-binding drain budget (default 30 s). Past budget, hard-close + retry remaining via fresh connection (idempotency_key handles dedup).

This becomes part of the §16.1 phase 5 (`kiseki_client::native::NativeClient` + TopologyCache) work.

### R2-M6: NodeBindings advertisement undefined for `Draining` state

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §1.7, §6, §9

**Description**: §1.7 says `bindings: Vec<BindingEndpoint>` is "non-empty for any `Active` node". A `Draining` node (per §9 lease lifecycle + ADR-035) is by definition not `Active` but is still serving in-flight work for its quiesce window. What does the topology advertise for it?

- Empty bindings? Then clients reading topology can't dial it for legitimate in-flight work (lease-bound writes that need to finish on this leader).
- Same bindings as Active? Then clients freely route NEW work to a draining node, which contradicts ADR-035's drain semantics.
- Some marker that says "in-flight only"? Spec doesn't define.

**Evidence**: §1.7 says "Active" specifically; doesn't address Draining/Failed/Evicted. §3.2 per-edge selection doesn't filter by node state either.

**Suggested resolution**: §1.7 amended:

- `bindings: Vec<BindingEndpoint>` MUST be non-empty for `Active` and `Degraded` (still serving) nodes.
- For `Draining`: bindings are advertised as-is BUT each `BindingEndpoint` carries a `drain_state: Option<DrainState>` field documenting the quiesce-window remaining. Clients filter:
  - For lease-bound writes against an existing inode held by this node's leader: dial the draining node (drain protocol allows).
  - For new opens / new lease acquisitions: skip draining nodes; use a different shard leader.
- For `Failed` / `Evicted`: bindings empty; clients won't dial.

This adds a small field to `BindingEndpoint` but cleanly disambiguates the §9.1 case where new `acquire_lease` requests are refused with `Unavailable{node_draining}` while existing leases continue.

---

## LOW

### R2-L1: §5.1 trust matrix duplicates §2.4.1 (drift risk)

**Severity**: Low
**Category**: Documentation drift
**Location**: ADR-042 §5.1, §2.4.1

**Description**: §5.1 lists per-binding canonical-SAN extraction sites; §2.4.1 lists per-libfabric-provider trust anchors. They overlap on the libfabric/cxi row (both describe the trust anchor). Two tables with the same fact in different words is a drift hazard — a future update to one but not the other will create silent inconsistency.

**Suggested resolution**: §5.1 references §2.4.1 for the libfabric trust matrix instead of duplicating: "For libfabric providers, see the trust matrix in §2.4.1; the canonical-SAN extraction site for each provider is documented inline there."

### R2-L2: `RequestPrincipal` discipline is unenforceable mechanically

**Severity**: Low
**Category**: Correctness > Implicit coupling
**Location**: ADR-042 §1.8, §5.1

**Description**: §1.8 mandates "ServerImpl reads ONLY through this trait — never reaches into binding-specific stash locations directly." This is implementer discipline; no compiler check possible. A future contributor could add a binding-specific extension method to `ServerImpl` and the spec wouldn't catch it.

**Suggested resolution**: Implementer adds a clippy lint or grep-based PR check that fails the build if `ServerImpl` methods reference `tonic::Request` / `tcp_framed::ConnectionContext` / `cxi::AttestationContext` types directly. Architect documents this enforcement requirement in §1.8.

### R2-L3: `CxiAttestationEnvelope` schema_version > 1 behavior unspecified

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §2.4.2

**Description**: The struct has `schema_version: u8` and §2.4.2 says "bump on incompatible changes (defaults to 1)" but the validation flow doesn't include "reject schema_version > supported". Other spec layers (ADR-022 fjall encoding, raft log encoding) explicitly fail-close on schema_version > supported.

**Suggested resolution**: §2.4.2 step 1 extended to "Read first message; if not a valid `CxiAttestationEnvelope` decode OR `schema_version > 1`, close with `Unauthenticated{cxi_attestation_schema_too_new}`."

### R2-L4: §14.1 `ib_send_bw` / `fi_bw` baselines are single-stream; multi-stream cluster perf differs

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §14.1

**Description**: §14.1 step 1 uses `ib_send_bw` / `fi_bw` to get fabric_peak_throughput. Both are single-stream, single-node-pair benchmarks. Real workload is multi-stream + multi-node-pair. Fabric peak from these tools may be much higher than achievable per-stream throughput when the cluster is busy (PCIe contention, NIC queue contention, etc.).

The 50% perf-gate is conservative enough to absorb this gap, but the spec doesn't acknowledge it. Operators reading §14.1 would expect `kiseki throughput / ib_send_bw throughput` to be a meaningful single-number gauge of kiseki overhead — but the denominator's not what they think.

**Suggested resolution**: §14.1 amended to specify multi-stream benchmark variants:

- `ib_send_bw -m -s 65536 -n 100000 -q 4 <peer>` (4 concurrent QPs, more representative).
- `fi_bw -e msg -S 65536 -I 100000 -p N <peer>` (N parallel processes).

And note: "fabric_peak from single-stream micro-benchmarks should be treated as upper-bound; multi-stream variants are the more realistic baseline."

This is implementer-phase guidance, not a spec-redesign. Update §14.1 prose; the perf-gate criterion stays at 50%.

---

## Cross-cutting observations

### R2-O1: TCP-framed binding wire-version vs ADR-041 wire-version is implicitly linked but not formally cross-referenced

§13.2 mentions TCP-framed bumps to v3; ADR-041 currently has V1 (now superseded by V2 in the postcard migration this session). The two ADRs share the same wire-format primitives but each has its own version byte. A future update to ADR-041's framing should bump ADR-042's TCP-framed binding version in lockstep. Not specified anywhere.

**Suggested resolution**: §13.2 footnotes "TCP-framed binding's wire version is derived from ADR-041's fabric transport version; bumps in ADR-041 require a same-PR bump here."

### R2-O2: No mention of binding-specific BDD coverage gaps

§16.1 phase 6 mandates per-binding BDD via `@binding=*` tags. But: not every contract verb may be wire-testable on every binding within v1 timelines (RDMA bindings may ship phase 9–10 with reduced BDD coverage). Spec doesn't say.

**Suggested resolution**: §16.1 phase 6 amended to "Per-binding BDD coverage is required for the `gRPC` and `TcpFramed` bindings in v1. RDMA bindings may ship phase 9–10 with a reduced BDD subset (covering at minimum: handshake, attestation/auth, single-verb round-trip, error-mapping). Full BDD parity for RDMA bindings is a phase-10-follow-up."

### R2-O3: no fault-injection story for the binding-selection path

ADR-042 §15 hot spot 9 mentions "binding-selection failure modes" as a gate-1 concern. The amendments cover the happy path well. But there's no `fault_injection` BDD scenario set for: probe timeout simulation, listener crash + restart cycle, topology version regress. These would catch class-9 issues at gate-1 perf-check rather than in production.

**Suggested resolution**: Tag a future BDD-completion-plan addition: per-binding fault scenarios. Out of scope for ADR-042 v1; flagged here.

---

## Recommendation

**Pass with follow-ups.** Architect's amendments cleanly close all 13 round-1 findings. The 1 HIGH (R2-H1, cxi handshake DoS surface) is a security gap that must close before phase 10 implementation begins — but does NOT block phases 0–8 (gRPC + TCP-framed + selector + client + BDD + perf re-measure). Phases 0–8 can begin under the current spec.

The 6 MEDIUM findings cluster around portability (R2-M2 distro paths), startup time (R2-M3 k8s probes), drain semantics (R2-M5 + R2-M6), observability (R2-M1 cxi reason labels), and security boundary fine print (R2-M4 cross-node replay). All have proposed surgical fixes.

Implementer recommendation: phase 0 (contract types) starts immediately. Phases 9–10 (RDMA bindings) wait on R2-H1 + R2-M2 + R2-M5 + R2-M6 amendments.

Architect's round-1 prediction was "≤ 2 MEDIUM net new". Actual: 1 HIGH, 6 MEDIUM, 4 LOW. The HIGH is not a redesign — it's a missing-spec finding (rate limits + size caps that the architect didn't think to add). Adding §2.4.2.1 closes it cleanly.
